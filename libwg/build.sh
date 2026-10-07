#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd -P)"
WIREGUARD_DIR="${SCRIPT_DIR}/wireguard-go"

# git -C <dir> succeeds in an empty submodule directory too, by finding the
# superproject; a checkout of its own has no path up to its toplevel
is_toplevel() {
    local cdup
    cdup="$(git -C "$1" rev-parse --show-cdup 2>/dev/null)" && [[ -z "${cdup}" ]]
}

update_submodule() {
    # the url is synced only when needed, so a configured mirror survives
    git -C "${REPO_ROOT}" submodule update --init --recursive -- libwg/wireguard-go || {
        git -C "${REPO_ROOT}" submodule sync --recursive -- libwg/wireguard-go
        git -C "${REPO_ROOT}" submodule update --init --recursive -- libwg/wireguard-go
    }
}

# Build the wireguard-go commit this repo records: a clone without
# --recursive only has an empty directory there, and `git pull` does not move
# an existing checkout to a newly recorded commit. A checkout with commits of
# its own is built as it is, as is everything with LIBWG_KEEP_SUBMODULE=1 or
# outside a git checkout of this repo.
if [[ "${LIBWG_KEEP_SUBMODULE:-0}" != 1 ]] && is_toplevel "${REPO_ROOT}" &&
    recorded="$(git -C "${REPO_ROOT}" rev-parse -q --verify :libwg/wireguard-go)"; then
    if ! is_toplevel "${WIREGUARD_DIR}"; then
        update_submodule
    elif [[ "$(git -C "${WIREGUARD_DIR}" rev-parse HEAD)" != "${recorded}" ]]; then
        # a recorded commit not fetched yet comes from upstream, e.g. after a pull
        if ! git -C "${WIREGUARD_DIR}" cat-file -e "${recorded}^{commit}" 2>/dev/null ||
            git -C "${WIREGUARD_DIR}" merge-base --is-ancestor HEAD "${recorded}"; then
            update_submodule
        else
            echo "warning: libwg/wireguard-go is not at the recorded commit ${recorded}" \
                "and has commits of its own; building it as it is" \
                "(git submodule update switches to the recorded one)" >&2
        fi
    fi
fi

make -C "${WIREGUARD_DIR}" libwg
mv "${WIREGUARD_DIR}/libwg.a" "${WIREGUARD_DIR}/libwg.h" "${SCRIPT_DIR}/"
