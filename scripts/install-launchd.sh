#!/bin/bash

set -euo pipefail
export PATH="/usr/bin:/bin:/usr/sbin:/sbin"

readonly LABEL="com.github.pinkd.corplink-rs"
readonly SERVICE_TARGET="system/${LABEL}"
readonly INSTALLED_PLIST="/Library/LaunchDaemons/${LABEL}.plist"
readonly INSTALLED_BINARY="/usr/local/bin/corplink-rs"
readonly LOG_PATH="/var/log/corplink-rs.log"
readonly ERROR_LOG_PATH="/var/log/corplink-rs.err.log"
readonly LOCK_DIRECTORY="/var/run/corplink-rs-launchd.lock"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd -P)"
TEMPLATE_PATH="${REPO_ROOT}/launchd/${LABEL}.plist"

if [[ -x "${REPO_ROOT}/target/release/corplink-rs" ]]; then
    source_binary_path="${REPO_ROOT}/target/release/corplink-rs"
else
    source_binary_path="${REPO_ROOT}/corplink-rs"
fi
config_path="${REPO_ROOT}/config.json"
vpn_server_name=""
rust_log="corplink_rs=info"

usage() {
    cat <<EOF
Usage: sudo $0 [options]

Options:
  --binary PATH       source corplink-rs executable
                      (default: ${source_binary_path})
  --config PATH       config file passed to corplink-rs
                      (default: ${config_path})
  --vpn-server NAME   override the configured VPN server name
  --log-filter VALUE  RUST_LOG value (default: ${rust_log})
  -h, --help          show this help
EOF
}

die() {
    echo "error: $*" >&2
    exit 1
}

require_value() {
    [[ $# -ge 2 && -n "${2:-}" ]] || die "$1 requires a value"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)
            require_value "$@"
            source_binary_path="$2"
            shift 2
            ;;
        --config)
            require_value "$@"
            config_path="$2"
            shift 2
            ;;
        --vpn-server)
            require_value "$@"
            vpn_server_name="$2"
            shift 2
            ;;
        --log-filter)
            require_value "$@"
            rust_log="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

[[ ${EUID} -eq 0 ]] || die "run this installer with sudo"
[[ -f "${TEMPLATE_PATH}" ]] || die "launchd template not found: ${TEMPLATE_PATH}"
[[ -f "${config_path}" ]] || die "config file not found: ${config_path}"
[[ -f "${source_binary_path}" && -x "${source_binary_path}" ]] || \
    die "executable not found: ${source_binary_path}; build or unpack corplink-rs first"

absolute_file_path() {
    local path="$1"
    local directory
    directory="$(cd "$(dirname "${path}")" && pwd -P)"
    printf '%s/%s\n' "${directory}" "$(basename "${path}")"
}

process_pids() {
    local pids
    local status
    if pids="$(/usr/bin/pgrep -x corplink-rs 2>/dev/null)"; then
        printf '%s\n' "${pids}"
        return 0
    else
        status=$?
    fi
    [[ "${status}" -eq 1 ]] || die "unable to inspect running corplink-rs processes"
}

job_pid() {
    /bin/launchctl print "${SERVICE_TARGET}" 2>/dev/null | \
        /usr/bin/awk '$1 == "pid" && $2 == "=" { print $3; exit }'
}

wait_for_pid_exit() {
    local pid="$1"
    local attempt
    [[ -n "${pid}" ]] || return 0
    for ((attempt = 0; attempt < 30; attempt++)); do
        /bin/ps -p "${pid}" >/dev/null 2>&1 || return 0
        /bin/sleep 1
    done
    return 1
}

require_secure_directory() {
    local directory="$1"
    local owner
    local mode
    owner="$(/usr/bin/stat -f '%Su' "${directory}")"
    mode="$(/usr/bin/stat -f '%Lp' "${directory}")"
    [[ "${owner}" == "root" && $((8#${mode} & 8#022)) -eq 0 ]] || \
        die "refusing to install into non-root-writable directory: ${directory} (${owner}, mode ${mode})"
}

source_binary_path="$(absolute_file_path "${source_binary_path}")"
config_path="$(absolute_file_path "${config_path}")"
working_directory="$(dirname "${config_path}")"

temporary_plist=""
temporary_binary=""
backup_plist=""
backup_binary=""
lock_acquired=0
state_modified=0
old_job_loaded=0

cleanup() {
    local status=$?
    trap - EXIT
    set +e

    if [[ "${status}" -ne 0 && "${state_modified}" -eq 1 ]]; then
        echo "Installation failed; rolling back the previous launchd state..." >&2
        /bin/launchctl bootout "${SERVICE_TARGET}" >/dev/null 2>&1 || true

        if [[ -n "${backup_plist}" && -f "${backup_plist}" ]]; then
            /usr/bin/install -o root -g wheel -m 0644 "${backup_plist}" "${INSTALLED_PLIST}"
        else
            /bin/rm -f "${INSTALLED_PLIST}"
        fi
        if [[ -n "${backup_binary}" && -f "${backup_binary}" ]]; then
            /usr/bin/install -o root -g wheel -m 0755 "${backup_binary}" "${INSTALLED_BINARY}"
        else
            /bin/rm -f "${INSTALLED_BINARY}"
        fi

        if [[ "${old_job_loaded}" -eq 1 && -f "${INSTALLED_PLIST}" ]]; then
            rollback_pids="$(/usr/bin/pgrep -x corplink-rs 2>/dev/null)"
            rollback_pgrep_status=$?
            if [[ "${rollback_pgrep_status}" -eq 1 ]]; then
                /bin/launchctl enable "${SERVICE_TARGET}" >/dev/null 2>&1 || true
                /bin/launchctl bootstrap system "${INSTALLED_PLIST}" >/dev/null 2>&1 || true
                /bin/launchctl kickstart "${SERVICE_TARGET}" >/dev/null 2>&1 || true
            else
                echo "Old job was not restarted because another process exists or process inspection failed (PID(s): ${rollback_pids:-unknown})." >&2
            fi
        fi
    fi

    [[ -z "${temporary_plist}" ]] || /bin/rm -f "${temporary_plist}"
    [[ -z "${temporary_binary}" ]] || /bin/rm -f "${temporary_binary}"
    [[ -z "${backup_plist}" ]] || /bin/rm -f "${backup_plist}"
    [[ -z "${backup_binary}" ]] || /bin/rm -f "${backup_binary}"
    if [[ "${lock_acquired}" -eq 1 ]]; then
        /bin/rmdir "${LOCK_DIRECTORY}" >/dev/null 2>&1 || true
    fi

    exit "${status}"
}
trap cleanup EXIT

temporary_plist="$(/usr/bin/mktemp -t corplink-rs-launchd-plist)"
temporary_binary="$(/usr/bin/mktemp -t corplink-rs-launchd-binary)"
/bin/cp "${TEMPLATE_PATH}" "${temporary_plist}"
/usr/bin/install -o root -g wheel -m 0755 "${source_binary_path}" "${temporary_binary}"

/usr/bin/plutil -remove ProgramArguments "${temporary_plist}"
/usr/bin/plutil -insert ProgramArguments -array "${temporary_plist}"
/usr/bin/plutil -insert ProgramArguments.0 -string "${INSTALLED_BINARY}" "${temporary_plist}"
/usr/bin/plutil -insert ProgramArguments.1 -string "${config_path}" "${temporary_plist}"
/usr/bin/plutil -replace WorkingDirectory -string "${working_directory}" "${temporary_plist}"
/usr/bin/plutil -replace EnvironmentVariables.RUST_LOG -string "${rust_log}" "${temporary_plist}"
/usr/bin/plutil -replace EnvironmentVariables.CORPLINK_VPN_SERVER_NAME \
    -string "${vpn_server_name}" "${temporary_plist}"
/usr/bin/plutil -lint "${temporary_plist}" >/dev/null
[[ "$(/usr/bin/plutil -extract ProgramArguments.0 raw "${temporary_plist}")" == "${INSTALLED_BINARY}" ]] || \
    die "generated plist has an invalid executable argument"
[[ "$(/usr/bin/plutil -extract ProgramArguments.1 raw "${temporary_plist}")" == "${config_path}" ]] || \
    die "generated plist has an invalid config argument"
if /usr/libexec/PlistBuddy -c "Print :ProgramArguments:2" "${temporary_plist}" >/dev/null 2>&1; then
    die "generated plist has unexpected extra arguments"
fi

/bin/mkdir "${LOCK_DIRECTORY}" 2>/dev/null || \
    die "another launchd install or uninstall is already running"
lock_acquired=1

if [[ -e "${INSTALLED_PLIST}" ]]; then
    backup_plist="$(/usr/bin/mktemp -t corplink-rs-launchd-old-plist)"
    /bin/cp "${INSTALLED_PLIST}" "${backup_plist}"
fi
if [[ -e "${INSTALLED_BINARY}" ]]; then
    backup_binary="$(/usr/bin/mktemp -t corplink-rs-launchd-old-binary)"
    /bin/cp "${INSTALLED_BINARY}" "${backup_binary}"
fi

old_pid=""
if /bin/launchctl print "${SERVICE_TARGET}" >/dev/null 2>&1; then
    old_job_loaded=1
    [[ -f "${INSTALLED_PLIST}" ]] || die "loaded job has no plist at ${INSTALLED_PLIST}"
    old_pid="$(job_pid)"
fi

running_pids="$(process_pids)"
if [[ "${old_job_loaded}" -eq 0 && -n "${running_pids}" ]]; then
    die "a non-launchd corplink-rs process is already running (PID(s): ${running_pids})"
fi
if [[ "${old_job_loaded}" -eq 1 && -n "${running_pids}" && "${running_pids}" != "${old_pid}" ]]; then
    die "corplink-rs process ownership is ambiguous (job PID: ${old_pid:-none}; PID(s): ${running_pids})"
fi

state_modified=1
if [[ "${old_job_loaded}" -eq 1 ]]; then
    echo "Stopping the loaded ${LABEL} service..."
    /bin/launchctl bootout "${SERVICE_TARGET}"
    wait_for_pid_exit "${old_pid}" || die "the existing service did not stop"
fi
running_pids="$(process_pids)"
[[ -z "${running_pids}" ]] || die "a corplink-rs process appeared during installation"

binary_directory="$(dirname "${INSTALLED_BINARY}")"
require_secure_directory "$(dirname "${binary_directory}")"
if [[ ! -d "${binary_directory}" ]]; then
    /usr/bin/install -d -o root -g wheel -m 0755 "${binary_directory}"
fi
require_secure_directory "${binary_directory}"
/usr/bin/install -o root -g wheel -m 0755 "${temporary_binary}" "${INSTALLED_BINARY}"
/usr/bin/touch "${LOG_PATH}" "${ERROR_LOG_PATH}"
/usr/sbin/chown root:wheel "${LOG_PATH}" "${ERROR_LOG_PATH}"
/bin/chmod 0600 "${LOG_PATH}" "${ERROR_LOG_PATH}"
/usr/bin/install -o root -g wheel -m 0644 "${temporary_plist}" "${INSTALLED_PLIST}"

/bin/launchctl enable "${SERVICE_TARGET}"
/bin/launchctl bootstrap system "${INSTALLED_PLIST}"
/bin/launchctl kickstart -p "${SERVICE_TARGET}" >/dev/null

ready_pid=""
for ((attempt = 0; attempt < 30; attempt++)); do
    current_job_pid="$(job_pid)"
    running_pids="$(process_pids)"
    if [[ -n "${current_job_pid}" && "${running_pids}" == "${current_job_pid}" ]]; then
        ready_pid="${current_job_pid}"
        break
    fi
    if [[ "${running_pids}" == *$'\n'* ]]; then
        die "more than one corplink-rs process started (PID(s): ${running_pids})"
    fi
    /bin/sleep 1
done

[[ -n "${ready_pid}" ]] || die "launchd did not start an owned corplink-rs process"
/bin/sleep 2
[[ "$(job_pid)" == "${ready_pid}" && "$(process_pids)" == "${ready_pid}" ]] || \
    die "the launchd process did not remain stable after startup"

echo "Installed and started ${SERVICE_TARGET}."
echo "PID: ${ready_pid}"
echo "Binary: ${INSTALLED_BINARY}"
echo "Status: sudo launchctl print ${SERVICE_TARGET}"
echo "Logs:   sudo tail -f ${ERROR_LOG_PATH} ${LOG_PATH}"
