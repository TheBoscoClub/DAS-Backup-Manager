Name:           das-backup-manager
Version:        0.7.1
Release:        1%{?dist}
Summary:        DAS backup manager with btrbk, SQLite FTS5, KDE GUI

License:        MIT
URL:            https://github.com/TheBoscoClub/DAS-Backup-Manager
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires:  rust cargo cmake >= 3.25 extra-cmake-modules gcc-c++
BuildRequires:  cmake(Qt6Core) cmake(Qt6Widgets) cmake(Qt6Sql)
BuildRequires:  cmake(KF6CoreAddons) cmake(KF6I18n) cmake(KF6XmlGui)
BuildRequires:  cmake(KF6ConfigWidgets) cmake(KF6IconThemes) cmake(KF6Crash) cmake(KF6KIO)
Requires:       btrbk btrfs-progs smartmontools zsh util-linux
Requires:       qt6-qtbase kf6-kcoreaddons kf6-ki18n kf6-kxmlgui
Requires:       kf6-kconfigwidgets kf6-kiconthemes kf6-kcrash kf6-kio

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
