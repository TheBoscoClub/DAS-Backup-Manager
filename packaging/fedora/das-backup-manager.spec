Name:           das-backup-manager
Version:        0.5.0
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
cargo build --release --manifest-path indexer/Cargo.toml
%cmake -DBUILD_INDEXER=OFF
%cmake_build

%install
%cmake_install
install -Dm755 indexer/target/release/btrdasd %{buildroot}%{_bindir}/btrdasd

%files
%license LICENSE
%{_bindir}/btrdasd
%{_bindir}/btrdasd-gui
%{_prefix}/lib/das-backup/
%{_unitdir}/das-backup.service
%{_unitdir}/das-backup-full.service
%{_unitdir}/das-backup.timer
%{_unitdir}/das-backup-full.timer
%{_datadir}/applications/org.theboscoclub.btrdasd-gui.desktop
%{_datadir}/kxmlgui5/btrdasd-gui/
%{_datadir}/icons/hicolor/scalable/apps/btrdasd-gui.svg
