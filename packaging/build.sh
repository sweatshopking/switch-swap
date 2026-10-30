#!/usr/bin/env bash
# Builds release packages. Run inside the matching build environment:
#
#   packaging/build.sh deb-appimage   on Ubuntu 22.04  -> dist/*.deb, dist/*.AppImage
#   packaging/build.sh rpm            on Fedora 41     -> dist/*.rpm
#
# The oldest supported bases are used on purpose: a binary built against
# Ubuntu 22.04's glibc and GTK 4.6 runs on every newer distribution too.
# Packages are built for the machine it runs on: x86_64 (amd64) or aarch64 (arm64).
#
# packaging/build-in-containers.sh runs both with podman or docker; the GitHub
# workflow uses the same targets.
set -euo pipefail

# A custom tag also requires its trusted LINUXDEPLOY_SHA256 in the build environment.
LINUXDEPLOY_TAG="${LINUXDEPLOY_TAG:-1-alpha-20251107-1}"
# linuxdeploy's GTK plugin, pinned to a reviewed commit and checksum.
GTK_PLUGIN_COMMIT=7a3fbc31a9e5075073ff8790f26effbac5f84453
GTK_PLUGIN_SHA256=b0f4cbc684a0103a9651f0955b635eaea0096b3a66c0f5a2c2aa337960375171
APP=switch-swap
APP_ID=io.github.switchswap.SwitchSwap

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
WORK="$TARGET/package-work"
DIST="$ROOT/dist"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
BIN="$TARGET/release/$APP"
ARCH="$(uname -m)" # x86_64 or aarch64: the names linuxdeploy, AppImage and rpm use
case "$ARCH" in
    x86_64) DEB_ARCH=amd64 ;;
    aarch64) DEB_ARCH=arm64 ;;
    *) echo "error: unsupported architecture $ARCH" >&2; exit 1 ;;
esac
mkdir -p "$WORK" "$DIST"

log() { printf '\n==> %s\n' "$*"; }

install_deps_ubuntu() {
    log "Installing build dependencies (apt)"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
        ca-certificates curl gcc pkg-config file dpkg-dev libgtk-4-dev \
        desktop-file-utils librsvg2-common shared-mime-info >/dev/null
}

install_deps_fedora() {
    log "Installing build dependencies (dnf)"
    dnf install -y -q ca-certificates curl gcc pkgconf-pkg-config file rpm-build \
        gtk4-devel desktop-file-utils >/dev/null
}

install_rust() {
    if ! command -v cargo >/dev/null; then
        log "Installing Rust"
        curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
    fi
    # shellcheck disable=SC1091
    source "${CARGO_HOME:-$HOME/.cargo}/env"
}

build_app() {
    log "Building $APP $VERSION"
    cargo build --release --locked
    desktop-file-validate "$ROOT/packaging/$APP_ID.desktop"
}

# Lays out the installed file tree shared by the .deb and .rpm.
stage_tree() {
    local dest="$1"
    rm -rf "$dest"
    install -Dm755 "$BIN" "$dest/usr/bin/$APP"
    install -Dm644 "$ROOT/systemd/$APP.service" "$dest/usr/lib/systemd/system/$APP.service"
    install -Dm644 "$ROOT/systemd/$APP.service.d/10-controller-device-access.conf" \
        "$dest/usr/lib/systemd/system/$APP.service.d/10-controller-device-access.conf"
    install -Dm644 "$ROOT/packaging/settings.conf" "$dest/etc/$APP/settings.conf"
    install -Dm644 "$ROOT/packaging/$APP_ID.desktop" "$dest/usr/share/applications/$APP_ID.desktop"
    install -Dm644 "$ROOT/packaging/$APP_ID.svg" "$dest/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"
    install -Dm644 "$ROOT/packaging/$APP_ID-symbolic.svg" \
        "$dest/usr/share/icons/hicolor/scalable/apps/$APP_ID-symbolic.svg"
    install -Dm644 "$ROOT/LICENSE" "$dest/usr/share/licenses/$APP/LICENSE"
}

build_deb() {
    log "Packaging .deb"
    local stage="$WORK/deb"
    stage_tree "$stage"
    mkdir -p "$stage/DEBIAN" "$WORK/debian"
    # dpkg-shlibdeps needs a debian/control to exist; it only reads the package name from it
    printf 'Source: %s\n\nPackage: %s\nArchitecture: %s\n' "$APP" "$APP" "$DEB_ARCH" >"$WORK/debian/control"
    local linked
    linked="$(cd "$WORK" && dpkg-shlibdeps -O "$stage/usr/bin/$APP" 2>/dev/null | sed 's/^shlibs:Depends=//')"
    cat >"$stage/DEBIAN/control" <<EOF
Package: $APP
Version: $VERSION
Section: games
Priority: optional
Architecture: $DEB_ARCH
Maintainer: sweatshopking <336541686+sweatshopking@users.noreply.github.com>
Depends: $linked, systemd, udev, pkexec | policykit-1
Homepage: https://github.com/sweatshopking/switch-swap
Description: Make any game controller play as an Xbox or Switch controller
 Switch Swap presents a chosen game controller to games as a wired Xbox 360
 pad or a Nintendo Switch Pro Controller, keeping button positions, passing
 rumble through, and hiding the original so games see only one controller.
EOF
    printf '/etc/%s/settings.conf\n' "$APP" >"$stage/DEBIAN/conffiles"
    install -m755 "$ROOT/packaging/deb/postinst" "$ROOT/packaging/deb/prerm" \
        "$ROOT/packaging/deb/postrm" "$stage/DEBIAN/"
    dpkg-deb --root-owner-group --build "$stage" "$DIST/${APP}_${VERSION}_${DEB_ARCH}.deb" >/dev/null
    dpkg-deb --info "$DIST/${APP}_${VERSION}_${DEB_ARCH}.deb" | sed -n '/Depends/p'
}

# Downloads $1 to $2 unless already there, checking it against SHA-256 $3.
fetch_pinned() {
    local url="$1" dest="$2" sha="$3"
    if [ ! -f "$dest" ]; then
        local download
        download="$(mktemp "$WORK/download.XXXXXX")"
        curl -fsSL -o "$download" "$url" || { rm -f "$download"; echo "error: cannot download $url" >&2; return 1; }
        mv "$download" "$dest"
    fi
    if [ "$(sha256sum "$dest" | cut -d ' ' -f1)" != "${sha,,}" ]; then
        rm -f "$dest"
        echo "error: checksum mismatch for $url" >&2
        return 1
    fi
    chmod +x "$dest"
}

# Lists the distribution packages (and exact versions) that the libraries copied
# into the AppImage came from, so anyone can get the matching source, as their
# (L)GPL licenses ask.
bundled_sources() {
    local appdir="$1" lib pkg distro
    distro="$(. /etc/os-release && echo "$PRETTY_NAME")"
    echo "Libraries copied unmodified from $distro into this AppImage."
    echo "Source for each is available from https://launchpad.net/ubuntu/+source/<source>/<version>"
    echo "or with: apt-get source <source>=<version>"
    echo
    echo "source version"
    for lib in "$appdir"/usr/lib/*.so*; do
        pkg="$(dpkg -S "*/$(basename "$lib")" 2>/dev/null | head -1 | cut -d: -f1)" || true
        [ -n "$pkg" ] && dpkg-query -W -f='${source:Package} ${source:Version}\n' "$pkg" || true
    done | sort -u
}

build_appimage() {
    log "Packaging AppImage"
    local expected_sha256
    if [ "$LINUXDEPLOY_TAG" = "1-alpha-20251107-1" ]; then
        case "$ARCH" in
            x86_64) expected_sha256="c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d" ;;
            aarch64) expected_sha256="620095110d693282b8ebeb244a95b5e911cf8f65f76c88b4b47d16ae6346fcff" ;;
        esac
    else
        expected_sha256="${LINUXDEPLOY_SHA256:-}"
    fi
    if [[ ! "$expected_sha256" =~ ^[[:xdigit:]]{64}$ ]]; then
        echo "error: set LINUXDEPLOY_SHA256 to the trusted SHA-256 for linuxdeploy $LINUXDEPLOY_TAG" >&2
        return 1
    fi
    local tools="$WORK/tools"
    mkdir -p "$tools"
    fetch_pinned "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_TAG/linuxdeploy-$ARCH.AppImage" \
        "$tools/linuxdeploy" "$expected_sha256"
    fetch_pinned "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/$GTK_PLUGIN_COMMIT/linuxdeploy-plugin-gtk.sh" \
        "$tools/linuxdeploy-plugin-gtk.sh" "$GTK_PLUGIN_SHA256"

    local appdir="$WORK/AppDir"
    rm -rf "$appdir"
    install -Dm644 "$ROOT/packaging/$APP_ID-symbolic.svg" \
        "$appdir/usr/share/icons/hicolor/scalable/apps/$APP_ID-symbolic.svg"
    install -Dm644 "$ROOT/LICENSE" "$appdir/usr/share/licenses/$APP/LICENSE"
    local output="$DIST/Switch-Swap-${VERSION}-${ARCH}.AppImage"
    # containers have no FUSE, so let the tools unpack themselves
    export APPIMAGE_EXTRACT_AND_RUN=1
    (
        cd "$WORK"
        export PATH="$tools:$PATH" DEPLOY_GTK_VERSION=4 LDAI_OUTPUT="$output"
        # linuxdeploy leaves out libraries every desktop has; FriBidi is on that
        # list but can be missing from a desktop without GTK apps, so it's bundled.
        local fribidi
        fribidi="$(ldconfig -p | sed -n 's/.*libfribidi\.so\.0 .*=> //p' | head -1)"
        linuxdeploy --appdir "$appdir" --executable "$BIN" --library "$fribidi" \
            --desktop-file "$ROOT/packaging/$APP_ID.desktop" \
            --icon-file "$ROOT/packaging/$APP_ID.svg" \
            --plugin gtk >"$WORK/linuxdeploy.log" 2>&1 || { tail -40 "$WORK/linuxdeploy.log"; exit 1; }
        bundled_sources "$appdir" >"$appdir/usr/share/licenses/$APP/BUNDLED-LIBRARIES.txt"
        linuxdeploy --appdir "$appdir" --output appimage >>"$WORK/linuxdeploy.log" 2>&1 ||
            { tail -40 "$WORK/linuxdeploy.log"; exit 1; }
    )
    "$output" --version
    install -m755 "$ROOT/packaging/install-appimage.sh" "$DIST/install-appimage.sh"
}

build_rpm() {
    log "Packaging .rpm"
    local stage="$WORK/rpm-root"
    stage_tree "$stage"
    local top="$WORK/rpmbuild"
    rm -rf "$top"
    mkdir -p "$top"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
    sed -e "s/@VERSION@/$VERSION/" "$ROOT/packaging/rpm/$APP.spec.in" >"$top/SPECS/$APP.spec"
    rpmbuild -bb --quiet \
        --define "_topdir $top" \
        --define "stage $stage" \
        "$top/SPECS/$APP.spec"
    cp "$top"/RPMS/"$ARCH"/*.rpm "$DIST/"
    rpm -qp --requires "$DIST"/*."$ARCH".rpm | grep -v '^rpmlib' | tr '\n' ' '
    echo
}

case "${1:-}" in
    deb-appimage)
        [ "${SKIP_DEPS:-}" = 1 ] || install_deps_ubuntu
        install_rust
        build_app
        build_deb
        build_appimage
        ;;
    rpm)
        [ "${SKIP_DEPS:-}" = 1 ] || install_deps_fedora
        install_rust
        build_app
        build_rpm
        ;;
    *)
        echo "usage: $0 deb-appimage|rpm" >&2
        exit 2
        ;;
esac
log "Done: $(ls "$DIST")"
