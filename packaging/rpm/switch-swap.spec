Name:           switch-swap
Version:        %{version}
Release:        1%{?dist}
Summary:        System-wide Xbox and Nintendo Switch controller layout remapper
License:        No license declared
BuildArch:      x86_64
Requires:       gtk4
Requires:       polkit
Requires:       systemd

%description
Switch Swap is a GTK application and system service that detects up to four
Linux evdev game controllers and swaps the face-button positions between Xbox
and Nintendo Switch layouts.

%prep

%build

%install
install -Dpm0755 %{_sourcedir}/switch-swap %{buildroot}%{_bindir}/switch-swap
install -Dpm0644 %{_sourcedir}/switch-swap.service %{buildroot}/usr/lib/systemd/system/switch-swap.service
install -Dpm0644 %{_sourcedir}/settings.conf %{buildroot}%{_sysconfdir}/switch-swap/settings.conf
install -Dpm0644 %{_sourcedir}/switch-swap.desktop %{buildroot}%{_datadir}/applications/switch-swap.desktop
install -Dpm0644 %{_sourcedir}/io.github.switchswap.SwitchSwap.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.github.switchswap.SwitchSwap.svg

%post
systemctl daemon-reload
systemctl enable --now switch-swap.service

%preun
if [ "$1" -eq 0 ]; then
    systemctl disable --now switch-swap.service
fi

%postun
systemctl daemon-reload

%files
%{_bindir}/switch-swap
/usr/lib/systemd/system/switch-swap.service
%config(noreplace) %{_sysconfdir}/switch-swap/settings.conf
%{_datadir}/applications/switch-swap.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.github.switchswap.SwitchSwap.svg
