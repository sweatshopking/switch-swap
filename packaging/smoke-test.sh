#!/usr/bin/env bash
# Installs a built package on a fresh system and checks it starts:
#   packaging/smoke-test.sh deb|rpm|appimage
# Run as root in a clean container of the distribution being tested, with the
# packages in dist/. Containers have no running systemd, so this checks the
# package installs, its files are in place, the program runs, and the window
# opens and stays open; the service itself is tested on real hardware.
set -euo pipefail
kind="${1:?usage: $0 deb|rpm|appimage}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="$(uname -m)"
cd "$ROOT"

log() { printf '\n==> %s\n' "$*"; }

install_display() {
    if command -v apt-get >/dev/null; then
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends xvfb xauth dbus libegl1 libgles2 "$@" >/dev/null
    elif command -v dnf >/dev/null; then
        dnf install -y -q xorg-x11-server-Xvfb xauth dbus-daemon libglvnd-gles mesa-dri-drivers "$@" >/dev/null
    else
        zypper --non-interactive -q install xvfb-run dbus-1-daemon Mesa-libGLESv2-2 Mesa-dri "$@" >/dev/null
    fi
}

case "$kind" in
    deb)
        log "Installing the .deb with apt"
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        case "$ARCH" in x86_64) deb_arch=amd64 ;; aarch64) deb_arch=arm64 ;; esac
        apt-get install -y -qq "$ROOT"/dist/switch-swap_*_"$deb_arch".deb >/dev/null
        install_display
        program=switch-swap
        ;;
    rpm)
        log "Installing the .rpm"
        if command -v dnf >/dev/null; then
            dnf install -y -q "$ROOT"/dist/switch-swap-*."$ARCH".rpm >/dev/null
        else
            zypper --non-interactive -q install --allow-unsigned-rpm "$ROOT"/dist/switch-swap-*."$ARCH".rpm >/dev/null
        fi
        install_display
        program=switch-swap
        ;;
    appimage)
        # No GTK installed: everything the window needs beyond the libraries
        # every desktop has (and AppImages leave out on purpose: fonts, text
        # shaping, GL, X11/Wayland) must be in the AppImage.
        install_display file libharfbuzz0b libfontconfig1 libfreetype6 libgl1 libgmp10 \
            libx11-6 libxext6 libxrender1 libxi6 libxcursor1 libxrandr2 libxinerama1 \
            libxkbcommon0 libwayland-client0 libwayland-egl1 libwayland-cursor0
        program="$(ls "$ROOT"/dist/Switch-Swap-*-"$ARCH".AppImage)"
        chmod +x "$program"
        export APPIMAGE_EXTRACT_AND_RUN=1
        ;;
    *)
        echo "usage: $0 deb|rpm|appimage" >&2
        exit 2
        ;;
esac

if [ "$kind" != appimage ]; then
    log "Checking installed files"
    for file in /usr/bin/switch-swap \
        /usr/lib/systemd/system/switch-swap.service \
        /usr/lib/systemd/system/switch-swap.service.d/10-controller-device-access.conf \
        /etc/switch-swap/settings.conf \
        /usr/share/applications/io.github.switchswap.SwitchSwap.desktop \
        /usr/share/icons/hicolor/scalable/apps/io.github.switchswap.SwitchSwap.svg \
        /usr/share/licenses/switch-swap/LICENSE; do
        [ -e "$file" ] || { echo "missing: $file" >&2; exit 1; }
    done
    command -v pkexec >/dev/null || { echo "pkexec was not installed as a dependency" >&2; exit 1; }
fi

log "Running $program --version"
"$program" --version

log "Opening the window"
# Containers lack the desktop session and GPU every real desktop has: give
# the window a session bus and let it draw in software.
# With no service running, the window opens and offers to install it.
GSK_RENDERER=cairo dbus-run-session -- xvfb-run -a sh -c "\"$program\" > /tmp/switch-swap-window.log 2>&1 & pid=\$!; sleep 5; kill -0 \$pid && kill \$pid" ||
    { echo "the window did not stay open:" >&2; cat /tmp/switch-swap-window.log >&2; exit 1; }
cat /tmp/switch-swap-window.log
log "OK: $kind on $(. /etc/os-release && echo "$PRETTY_NAME") ($ARCH)"
