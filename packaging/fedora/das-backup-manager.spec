Name:           das-backup-manager
Version:        0.7.20.0
Release:        1%{?dist}
Summary:        DAS backup manager with btrbk, SQLite FTS5, KDE GUI

License:        MIT
URL:            https://github.com/TheBoscoClub/DAS-Backup-Manager
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires:  rust cargo cmake >= 3.25 extra-cmake-modules gcc-c++
BuildRequires:  systemd-rpm-macros
BuildRequires:  cmake(Qt6Core) cmake(Qt6Widgets) cmake(Qt6Sql)
BuildRequires:  cmake(KF6CoreAddons) cmake(KF6I18n) cmake(KF6XmlGui)
BuildRequires:  cmake(KF6ConfigWidgets) cmake(KF6IconThemes) cmake(KF6Crash) cmake(KF6KIO)
BuildRequires:  cmake(KF6Notifications) cmake(KF6StatusNotifierItem)
BuildRequires:  cmake(Qt6Charts)
Requires:       btrbk btrfs-progs smartmontools bash util-linux
Recommends:     s-nail mbuffer
Requires:       qt6-qtbase qt6-qtcharts kf6-kcoreaddons kf6-ki18n kf6-kxmlgui
Requires:       kf6-kconfigwidgets kf6-kiconthemes kf6-kcrash kf6-kio
Requires:       kf6-knotifications kf6-kstatusnotifieritem

%description
ButteredDASD manages BTRFS-based backups to DAS (Direct Attached Storage)
enclosures. Features config-driven backup orchestration via btrbk, full-text
content indexing with SQLite FTS5, and a KDE Plasma 6 GUI for browsing and
restoring files from backup snapshots.

%prep
%autosetup -n DAS-Backup-Manager-%{version}

%build
cargo build --release --features dbus --manifest-path indexer/Cargo.toml
%cmake -DBUILD_INDEXER=OFF
%cmake_build

%install
%cmake_install
install -Dm755 indexer/target/release/btrdasd %{buildroot}%{_bindir}/btrdasd
install -Dm755 indexer/target/release/btrdasd-helper %{buildroot}%{_libexecdir}/btrdasd-helper
install -Dm644 dbus/org.dasbackup.Helper1.conf %{buildroot}%{_datadir}/dbus-1/system.d/org.dasbackup.Helper1.conf
install -Dm644 dbus/org.dasbackup.Helper1.service %{buildroot}%{_datadir}/dbus-1/system-services/org.dasbackup.Helper1.service
install -Dm644 polkit/org.dasbackup.policy %{buildroot}%{_datadir}/polkit-1/actions/org.dasbackup.policy
install -Dm644 systemd/btrdasd-helper.service %{buildroot}%{_unitdir}/btrdasd-helper.service

%files
%license LICENSE
%{_bindir}/btrdasd
%{_libexecdir}/btrdasd-helper
%{_bindir}/btrdasd-gui
%{_prefix}/lib/das-backup/
%{_unitdir}/das-backup.service
%{_unitdir}/das-backup-full.service
%{_unitdir}/das-backup.timer
%{_unitdir}/das-backup-full.timer
%{_datadir}/applications/org.theboscoclub.btrdasd-gui.desktop
%{_datadir}/kxmlgui5/btrdasd-gui/
%{_datadir}/dbus-1/system.d/org.dasbackup.Helper1.conf
%{_datadir}/dbus-1/system-services/org.dasbackup.Helper1.service
%{_datadir}/polkit-1/actions/org.dasbackup.policy
%{_unitdir}/btrdasd-helper.service
%{_datadir}/icons/hicolor/scalable/apps/btrdasd-gui.svg
%{_mandir}/man1/btrdasd.1*

%changelog
* Fri Aug 28 2026 TheBoscoClub <gjbr@pm.me> - 0.7.20.0-1
- Security and correctness release from an independent review of the privilege boundary
- archive_boot no longer deletes the live @ before locating its replacement (bd 5ig)
- D-Bus helper joins the backup/scrub maintenance interlock (bd dca)
- config_path removed from 17 D-Bus methods; daemon uses the canonical path only (bd wd7)
- restore gains [restore] allowed_roots, a fail-closed traversal guard, and O_NOFOLLOW writes (bd s05)
- stream_command drains stderr concurrently; no longer deadlocks past a pipe buffer (bd az3)
- job_cancel enforces job ownership (bd h2s)
- Fail-silent cluster: btrbk counters, MountGuard::Drop, mkdir shell-outs, schedule parsers (bd 06p)

* Sat Apr 11 2026 TheBoscoClub <gjbr@pm.me> - 0.7.12.2-1
- Bump to 0.7.12.2: debian Build .deb step now uses shell:bash (fixes ${var//-/\~} "Bad substitution" on dash); PKGBUILD bumped from stale 0.7.10 and gained hicolor-icon-theme dep; verified clean local Arch build

* Sat Apr 11 2026 TheBoscoClub <gjbr@pm.me> - 0.7.12.1-1
- Bump to 0.7.12.1: fix debian/changelog version lag; CI now sed-patches debian changelog version at build time

* Sat Apr 11 2026 TheBoscoClub <gjbr@pm.me> - 0.7.12-1
- Bump to 0.7.12: repair release-packages CI workflow end-to-end; permanent fix for 2026-03-05 DAS ESP wipe incident (render_esp_hook removed)

* Mon Mar 16 2026 TheBoscoClub <gjbr@pm.me> - 0.7.11-1
- Bump to 0.7.11: GUI accelerator/progress dock/status bar fixes

* Mon Mar 16 2026 TheBoscoClub <gjbr@pm.me> - 0.7.10-1
- Bump to 0.7.10: fix backup history recording from CLI and shell script

* Sat Mar 07 2026 TheBoscoClub <gjbr@pm.me> - 0.7.9-1
- Bump to 0.7.9: remove Dockerfile and Docker references (incompatible with DAS backup operations)

* Sat Mar 07 2026 TheBoscoClub <gjbr@pm.me> - 0.7.8-1
- Bump to 0.7.8: replace QSplitter with native dock resize for progress panel

* Sat Mar 07 2026 TheBoscoClub <gjbr@pm.me> - 0.7.7-1
- Bump to 0.7.7: resizable log view (native dock resize), smart auto-scroll, non-fatal email errors, s-nail v15-compat

* Thu Mar 05 2026 TheBoscoClub <gjbr@pm.me> - 0.7.6-1
- Bump to 0.7.6: email reports, btrbk.conf canonical path fix, dry-run DB fix

* Thu Mar 05 2026 TheBoscoClub <gjbr@pm.me> - 0.7.5-1
- Bump to 0.7.5: source volume auto-mount, snapshot_name/target_labels config

* Thu Mar 05 2026 TheBoscoClub <gjbr@pm.me> - 0.7.4-1
- Initial RPM package for Fedora
