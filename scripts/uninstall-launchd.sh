#!/bin/bash

set -euo pipefail
export PATH="/usr/bin:/bin:/usr/sbin:/sbin"

readonly LABEL="com.github.pinkd.corplink-rs"
readonly SERVICE_TARGET="system/${LABEL}"
readonly INSTALLED_PLIST="/Library/LaunchDaemons/${LABEL}.plist"
readonly INSTALLED_BINARY="/usr/local/bin/corplink-rs"
readonly LOCK_DIRECTORY="/var/run/corplink-rs-launchd.lock"

die() {
    echo "error: $*" >&2
    exit 1
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

[[ ${EUID} -eq 0 ]] || die "run this uninstaller with sudo"

/bin/mkdir "${LOCK_DIRECTORY}" 2>/dev/null || \
    die "another launchd install or uninstall is already running"
cleanup() {
    local status=$?
    trap - EXIT
    /bin/rmdir "${LOCK_DIRECTORY}" >/dev/null 2>&1 || true
    exit "${status}"
}
trap cleanup EXIT

if /bin/launchctl print "${SERVICE_TARGET}" >/dev/null 2>&1; then
    /bin/launchctl bootout "${SERVICE_TARGET}"
fi

if [[ -e "${INSTALLED_PLIST}" ]]; then
    /bin/rm -f "${INSTALLED_PLIST}"
fi
if [[ -e "${INSTALLED_BINARY}" ]]; then
    /bin/rm -f "${INSTALLED_BINARY}"
fi

echo "Uninstalled ${SERVICE_TARGET}."
echo "Configuration, cookies, and /var/log/corplink-rs*.log were preserved."
running_pids="$(process_pids)"
if [[ -n "${running_pids}" ]]; then
    echo "Warning: non-launchd corplink-rs process(es) remain: ${running_pids}" >&2
fi
