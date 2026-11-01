Name:           pdcli
Version:        0.5.1
Release:        1%{?dist}
Summary:        Unofficial Proton Drive desktop and command-line client
License:        MIT
URL:            https://github.com/tirbofish/proton-sdk-rs2
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  protobuf-compiler
BuildRequires:  pkgconfig(dbus-1)
BuildRequires:  pkgconfig(gtk+-3.0)
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  pkgconfig(libxdo)
BuildRequires:  pkgconfig(libfuse3)
BuildRequires:  pkgconfig(libayatana-appindicator-0.1)
BuildRequires:  systemd-rpm-macros
Requires:       fuse3
Requires:       systemd

%description
pdcli mounts Proton Drive through FUSE and provides a GNOME desktop window,
command-line controls, and a background synchronization daemon.

%prep
%autosetup

%build
cargo build --locked --release -p pdcli

%install
install -Dpm0755 target/release/pdcli %{buildroot}%{_bindir}/pdcli
install -Dpm0755 target/release/pdcli-gui %{buildroot}%{_bindir}/pdcli-gui
install -Dpm0644 packaging/io.github.tirbofish.pdcli.desktop %{buildroot}%{_datadir}/applications/io.github.tirbofish.pdcli.desktop
install -Dpm0644 packaging/systemd/pdcli.service %{buildroot}%{_prefix}/lib/systemd/user/pdcli.service
install -Dpm0644 packaging/icons/hicolor/scalable/apps/io.github.tirbofish.pdcli.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.github.tirbofish.pdcli.svg
install -Dpm0644 crates/pdcli/assets/io.github.tirbofish.pdcli.open-document.desktop %{buildroot}%{_datadir}/applications/io.github.tirbofish.pdcli.open-document.desktop
install -Dpm0644 crates/pdcli/assets/mime/io.github.tirbofish.pdcli.xml %{buildroot}%{_datadir}/mime/packages/io.github.tirbofish.pdcli.xml
install -Dpm0644 crates/pdcli/assets/icons/application-vnd.proton.doc.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/mimetypes/application-vnd.proton.doc.svg
install -Dpm0644 crates/pdcli/assets/icons/application-vnd.proton.sheet.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/mimetypes/application-vnd.proton.sheet.svg

%files
%license LICENSE.md
%doc crates/pdcli/README.md
%{_bindir}/pdcli
%{_bindir}/pdcli-gui
%{_datadir}/applications/io.github.tirbofish.pdcli.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.github.tirbofish.pdcli.svg
%{_datadir}/applications/io.github.tirbofish.pdcli.open-document.desktop
%{_datadir}/mime/packages/io.github.tirbofish.pdcli.xml
%{_datadir}/icons/hicolor/scalable/mimetypes/application-vnd.proton.doc.svg
%{_datadir}/icons/hicolor/scalable/mimetypes/application-vnd.proton.sheet.svg
%{_prefix}/lib/systemd/user/pdcli.service

%changelog
* Sun Nov 01 2026 pdcli contributors <4tkbytes@pm.me> - 0.5.1-1
- Release 0.5.1
* Sun Nov 01 2026 pdcli contributors <4tkbytes@pm.me> - 0.5.0-1
- Release 0.5.0
* Sat Oct 03 2026 pdcli contributors <4tkbytes@pm.me> - 0.4.0-1
- Release 0.4.0
* Tue Sep 22 2026 pdcli contributors <4tkbytes@pm.me> - 0.3.2-1
- Release 0.3.2
* Wed Sep 16 2026 pdcli contributors <4tkbytes@pm.me> - 0.3.1-1
- Release 0.3.1
* Wed Sep 16 2026 pdcli contributors <4tkbytes@pm.me> - 0.3.0-1
- Initial package
