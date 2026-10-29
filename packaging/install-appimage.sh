#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
    echo "Run this installer with sudo: sudo ./install-appimage.sh ./Switch-Swap-VERSION-x86_64.AppImage" >&2
    exit 1
fi

if [[ $# -ne 1 || ! -f "$1" ]]; then
    echo "Usage: sudo $0 /path/to/Switch-Swap-VERSION-x86_64.AppImage" >&2
    exit 2
fi

image="$(realpath -- "$1")"
if [[ -e /etc/systemd/system/switch-swap.service || -e /usr/lib/systemd/system/switch-swap.service ]]; then
    echo "A switch-swap.service already exists. Remove the existing installation before using the AppImage service installer." >&2
    exit 1
fi

install -Dm755 "${image}" /opt/switch-swap/switch-swap.AppImage
install -d -m755 /etc/switch-swap
if [[ ! -e /etc/switch-swap/settings.conf ]]; then
    install -m600 /dev/null /etc/switch-swap/settings.conf
    printf 'enabled=false\nlayout=xbox\n' > /etc/switch-swap/settings.conf
fi
install -d -m755 /etc/systemd/system
cat > /etc/systemd/system/switch-swap.service <<'EOF'
[Unit]
Description=System-wide controller layout remapping service
After=systemd-udevd.service

[Service]
Type=simple
ExecStart=/opt/switch-swap/switch-swap.AppImage service
User=root
Group=root
RuntimeDirectory=switch-swap
RuntimeDirectoryMode=0755
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/etc/switch-swap /run/switch-swap
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
RestrictAddressFamilies=AF_UNIX
DevicePolicy=closed
DeviceAllow=/dev/input/event* rw
DeviceAllow=/dev/uinput rw

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --now switch-swap.service
echo "Installed the Switch Swap service. Launch the AppImage as your regular user."
