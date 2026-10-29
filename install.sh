#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
    echo "Run this installer with sudo: sudo ./install.sh" >&2
    exit 1
fi

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
binary="${project_dir}/target/release/switch-swap"
if [[ ! -x "${binary}" ]]; then
    echo "Release binary not found. Build it first with: cargo build --release" >&2
    exit 1
fi

install -Dm755 "${binary}" /usr/bin/switch-swap
install -Dm644 "${project_dir}/systemd/switch-swap.service" \
    /etc/systemd/system/switch-swap.service
install -d -m755 /etc/switch-swap
if [[ ! -e /etc/switch-swap/settings.conf ]]; then
    install -m600 /dev/null /etc/switch-swap/settings.conf
    printf 'enabled=false\nlayout=xbox\n' > /etc/switch-swap/settings.conf
fi
install -d -m755 /run/switch-swap
systemctl daemon-reload
systemctl enable --now switch-swap.service

echo "Installed. Launch switch-swap as a regular user."
echo "Apply/undo prompts for administrator authorization through polkit."
