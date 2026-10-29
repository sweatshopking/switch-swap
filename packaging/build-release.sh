#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
version="${1:-}"
if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.]+)?$ ]]; then
    echo "Usage: $0 VERSION (for example: $0 0.1.1)" >&2
    exit 2
fi

cd "${project_dir}"
binary="${project_dir}/target/release/switch-swap"
if [[ ! -x "${binary}" ]]; then
    echo "Release binary not found. Run cargo build --release first." >&2
    exit 1
fi

for tool in dpkg-deb rpmbuild linuxdeploy; do
    if ! command -v "${tool}" >/dev/null 2>&1; then
        echo "Missing packaging tool: ${tool}" >&2
        exit 1
    fi
done

dist="${project_dir}/dist"
work="${project_dir}/target/package-work"
rm -rf -- "${work}"
mkdir -p "${dist}" "${work}"

package_dir="${work}/deb-root"
install -d "${package_dir}/DEBIAN" "${package_dir}/usr/bin" \
    "${package_dir}/usr/lib/systemd/system" "${package_dir}/etc/switch-swap" \
    "${package_dir}/usr/share/applications" \
    "${package_dir}/usr/share/icons/hicolor/scalable/apps" \
    "${package_dir}/usr/share/switch-swap" \
    "${package_dir}/usr/lib/systemd/system/switch-swap.service.d"
install -m755 "${binary}" "${package_dir}/usr/bin/switch-swap"
install -m644 systemd/switch-swap.service "${package_dir}/usr/lib/systemd/system/"
install -m644 systemd/switch-swap.service.d/10-controller-device-access.conf \
    "${package_dir}/usr/lib/systemd/system/switch-swap.service.d/"
install -m644 packaging/settings.conf "${package_dir}/etc/switch-swap/settings.conf"
install -m644 packaging/switch-swap.desktop "${package_dir}/usr/share/applications/"
install -m644 packaging/io.github.switchswap.SwitchSwap.svg \
    "${package_dir}/usr/share/icons/hicolor/scalable/apps/"
install -m644 packaging/io.github.switchswap.SwitchSwap-symbolic.svg \
    "${package_dir}/usr/share/icons/hicolor/scalable/apps/"
install -m644 packaging/switch-swap-logo.svg \
    "${package_dir}/usr/share/switch-swap/"
cat > "${package_dir}/DEBIAN/control" <<EOF
Package: switch-swap
Version: ${version}
Section: games
Priority: optional
Architecture: amd64
Depends: libgtk-4-1, polkitd, systemd
Maintainer: Switch Swap contributors
Description: Per-controller Xbox and Nintendo Switch button layout remapper
 Rust GTK application and service that remaps selected game controllers.
EOF
cat > "${package_dir}/DEBIAN/conffiles" <<'EOF'
/etc/switch-swap/settings.conf
EOF
install -m755 packaging/deb/postinst "${package_dir}/DEBIAN/postinst"
install -m755 packaging/deb/prerm "${package_dir}/DEBIAN/prerm"
install -m755 packaging/deb/postrm "${package_dir}/DEBIAN/postrm"
dpkg-deb --root-owner-group --build "${package_dir}" \
    "${dist}/switch-swap_${version}_amd64.deb"

rpm_root="${work}/rpmbuild"
install -d "${rpm_root}/SOURCES" "${rpm_root}/RPMS" "${rpm_root}/BUILD" \
    "${rpm_root}/BUILDROOT" "${rpm_root}/SPECS" "${rpm_root}/SRPMS"
install -m755 "${binary}" "${rpm_root}/SOURCES/switch-swap"
install -m644 systemd/switch-swap.service "${rpm_root}/SOURCES/"
install -m644 systemd/switch-swap.service.d/10-controller-device-access.conf \
    "${rpm_root}/SOURCES/"
install -m644 packaging/settings.conf "${rpm_root}/SOURCES/"
install -m644 packaging/switch-swap.desktop "${rpm_root}/SOURCES/"
install -m644 packaging/io.github.switchswap.SwitchSwap.svg "${rpm_root}/SOURCES/"
install -m644 packaging/io.github.switchswap.SwitchSwap-symbolic.svg "${rpm_root}/SOURCES/"
install -m644 packaging/switch-swap-logo.svg "${rpm_root}/SOURCES/"
rpmbuild -bb packaging/rpm/switch-swap.spec \
    --define "_topdir ${rpm_root}" \
    --define "version ${version}" \
    --define "_rpmdir ${rpm_root}/RPMS"
rpm_artifact="${rpm_root}/RPMS/x86_64/switch-swap-${version}-1.x86_64.rpm"
if [[ ! -f "${rpm_artifact}" ]]; then
    echo "rpmbuild succeeded but the expected RPM was not found at ${rpm_artifact}." >&2
    exit 1
fi
install -m644 "${rpm_artifact}" "${dist}/"

appdir="${work}/SwitchSwap.AppDir"
install -d "${appdir}/usr/bin" "${appdir}/usr/share/applications" \
    "${appdir}/usr/share/icons/hicolor/scalable/apps"
install -m755 "${binary}" "${appdir}/usr/bin/switch-swap"
install -m644 packaging/switch-swap.desktop "${appdir}/"
install -m644 packaging/switch-swap.desktop "${appdir}/usr/share/applications/"
install -m644 packaging/io.github.switchswap.SwitchSwap.svg \
    "${appdir}/io.github.switchswap.SwitchSwap.svg"
install -m644 packaging/io.github.switchswap.SwitchSwap.svg \
    "${appdir}/usr/share/icons/hicolor/scalable/apps/"
install -m644 packaging/io.github.switchswap.SwitchSwap-symbolic.svg \
    "${appdir}/usr/share/icons/hicolor/scalable/apps/"
install -Dpm0644 packaging/switch-swap-logo.svg \
    "${appdir}/usr/share/switch-swap/switch-swap-logo.svg"
(
    cd "${work}"
    linuxdeploy --appdir "${appdir}" \
        --desktop-file "${project_dir}/packaging/switch-swap.desktop" \
        --icon-file "${project_dir}/packaging/io.github.switchswap.SwitchSwap.svg" \
        --plugin gtk \
        --output appimage
)
appimage="$(find "${work}" -maxdepth 1 -type f -name '*.AppImage' \
    ! -name 'linuxdeploy*' ! -name 'appimagetool*' -print -quit)"
if [[ -z "${appimage}" ]]; then
    echo "linuxdeploy reported success but did not produce an AppImage." >&2
    exit 1
fi
mv -- "${appimage}" "${dist}/Switch-Swap-${version}-x86_64.AppImage"

(
    cd "${dist}"
    sha256sum "switch-swap_${version}_amd64.deb" \
        "switch-swap-${version}-1.x86_64.rpm" \
        "Switch-Swap-${version}-x86_64.AppImage" > SHA256SUMS
)
