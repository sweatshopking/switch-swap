#!/usr/bin/env bash
# Installs the Switch Swap service from an AppImage without opening its window:
#   sudo ./install-appimage.sh ./Switch-Swap-VERSION-x86_64.AppImage
# It runs the same installer as the window's "Install or update system service"
# button: the AppImage is copied to /opt/switch-swap (or /var/lib/switch-swap
# where /opt is read-only, as on SteamOS) and the service is enabled.
set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
    echo "Run this installer with sudo: sudo $0 /path/to/Switch-Swap-VERSION-x86_64.AppImage" >&2
    exit 1
fi
if [[ $# -ne 1 || ! -f "$1" ]]; then
    echo "Usage: sudo $0 /path/to/Switch-Swap-VERSION-x86_64.AppImage" >&2
    exit 2
fi

image="$(realpath -- "$1")"
chmod +x "${image}"
# Unpacking instead of mounting works without FUSE; the AppImage runtime sets
# APPIMAGE to its own path, which tells the installer to copy the AppImage.
APPIMAGE_EXTRACT_AND_RUN=1 "${image}" install-service
