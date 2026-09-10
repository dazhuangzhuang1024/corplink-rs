#!/bin/bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd -P)"
WIREGUARD_DIR="${SCRIPT_DIR}/wireguard-go"

if ! git -C "${WIREGUARD_DIR}" rev-parse --git-dir >/dev/null 2>&1; then
    git -C "${REPO_ROOT}" submodule update --init --recursive libwg/wireguard-go
fi

make -C "${WIREGUARD_DIR}" libwg
mv "${WIREGUARD_DIR}/libwg.a" "${WIREGUARD_DIR}/libwg.h" "${SCRIPT_DIR}/"
