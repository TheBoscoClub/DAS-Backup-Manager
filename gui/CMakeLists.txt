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
    Sql
)

find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS
    CoreAddons
    I18n
    XmlGui
    ConfigWidgets
    IconThemes
    Crash
    KIO
)

add_executable(btrdasd-gui)

target_sources(btrdasd-gui PRIVATE
    src/main.cpp
    src/mainwindow.cpp
    src/database.cpp
    src/snapshotmodel.cpp
    src/filemodel.cpp
    src/searchmodel.cpp
    src/snapshottimeline.cpp
    src/indexrunner.cpp
    src/snapshotwatcher.cpp
)

target_compile_options(btrdasd-gui PRIVATE -Wall -Wextra -Wpedantic -Werror)

target_link_libraries(btrdasd-gui PRIVATE
    Qt6::Core
    Qt6::Widgets
    Qt6::Sql
    KF6::CoreAddons
    KF6::I18n
    KF6::XmlGui
    KF6::ConfigWidgets
    KF6::IconThemes
    KF6::Crash
    KF6::KIOWidgets
)

install(TARGETS btrdasd-gui ${KDE_INSTALL_TARGETS_DEFAULT_ARGS})
install(FILES src/btrdasd-gui.rc DESTINATION ${KDE_INSTALL_KXMLGUIDIR}/btrdasd-gui)

feature_summary(WHAT ALL INCLUDE_QUIET_PACKAGES FATAL_ON_MISSING_REQUIRED_PACKAGES)

# Tests
if(BUILD_TESTING)
    find_package(Qt6 ${QT_MIN_VERSION} CONFIG REQUIRED COMPONENTS Test)
    include(ECMAddTests)

    ecm_add_test(tests/databasetest.cpp src/database.cpp
        TEST_NAME databasetest
        LINK_LIBRARIES Qt6::Test Qt6::Sql
    )

    ecm_add_test(tests/snapshotmodeltest.cpp src/snapshotmodel.cpp src/database.cpp
        TEST_NAME snapshotmodeltest
        LINK_LIBRARIES Qt6::Test Qt6::Sql
    )

    ecm_add_test(tests/filemodeltest.cpp src/filemodel.cpp src/database.cpp
        TEST_NAME filemodeltest
        LINK_LIBRARIES Qt6::Test Qt6::Sql
    )

    ecm_add_test(tests/searchmodeltest.cpp src/searchmodel.cpp src/filemodel.cpp src/database.cpp
        TEST_NAME searchmodeltest
        LINK_LIBRARIES Qt6::Test Qt6::Sql
    )
endif()
