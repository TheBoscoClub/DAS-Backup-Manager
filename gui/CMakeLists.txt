cmake_minimum_required(VERSION 3.25)

set(QT_MIN_VERSION "6.6.0")
set(KF_MIN_VERSION "6.0.0")

find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)
include(FeatureSummary)

find_package(Qt6 ${QT_MIN_VERSION} CONFIG REQUIRED COMPONENTS
    Core
    Widgets
    DBus
)

find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS
    CoreAddons
    I18n
    XmlGui
    ConfigWidgets
    IconThemes
    Crash
    KIO
    Notifications
    StatusNotifierItem
)

add_executable(btrdasd-gui)

target_sources(btrdasd-gui PRIVATE
    src/main.cpp
    src/mainwindow.cpp
    src/snapshotmodel.cpp
    src/filemodel.cpp
    src/searchmodel.cpp
    src/snapshottimeline.cpp
    src/indexrunner.cpp
    src/snapshotwatcher.cpp
    src/restoreaction.cpp
    src/settingsdialog.cpp
    src/dbusclient.cpp
    src/sidebar.cpp
    src/progresspanel.cpp
    src/backuphistory.cpp
    src/backuppanel.cpp
    src/healthdashboard.cpp
    src/configdialog.cpp
    src/snapshotbrowser.cpp
    src/setupwizard.cpp
)

target_compile_features(btrdasd-gui PRIVATE cxx_std_20)
target_compile_options(btrdasd-gui PRIVATE -Wall -Wextra -Wpedantic -Werror)
target_compile_definitions(btrdasd-gui PRIVATE
    BTRDASD_VERSION="${CMAKE_PROJECT_VERSION}")

target_link_libraries(btrdasd-gui PRIVATE
    Qt6::Core
    Qt6::Widgets
    Qt6::DBus
    KF6::CoreAddons
    KF6::I18n
    KF6::XmlGui
    KF6::ConfigWidgets
    KF6::IconThemes
    KF6::Crash
    KF6::KIOWidgets
    KF6::Notifications
    KF6::StatusNotifierItem
)

install(TARGETS btrdasd-gui ${KDE_INSTALL_TARGETS_DEFAULT_ARGS})
install(FILES src/btrdasd-gui.rc DESTINATION ${KDE_INSTALL_KXMLGUIDIR}/btrdasd-gui)
install(FILES org.theboscoclub.btrdasd-gui.desktop DESTINATION ${KDE_INSTALL_APPDIR})
install(FILES icons/btrdasd-gui.svg DESTINATION ${KDE_INSTALL_ICONDIR}/hicolor/scalable/apps)

feature_summary(WHAT ALL INCLUDE_QUIET_PACKAGES FATAL_ON_MISSING_REQUIRED_PACKAGES)

# Tests — direct-DB tests removed; database logic is tested in Rust indexer.
# TODO: Add D-Bus-based model tests with mock JSON data.
if(BUILD_TESTING)
    find_package(Qt6 ${QT_MIN_VERSION} CONFIG REQUIRED COMPONENTS Test)
    include(ECMAddTests)
endif()
