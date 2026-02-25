cmake_minimum_required(VERSION 3.25)
project(DAS-Backup-Manager
    VERSION 0.5.1
    DESCRIPTION "DAS backup manager with btrbk integration, SQLite FTS5 content indexing, and KDE Plasma GUI"
    LANGUAGES CXX
)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# =============================================================================
# Build options
# =============================================================================

include(CTest)
option(BUILD_GUI "Build KDE Plasma GUI (requires Qt6/KF6)" ON)
option(BUILD_INDEXER "Build btrdasd Rust binary via cargo" ON)

# =============================================================================
# Rust indexer (btrdasd binary)
# =============================================================================

if(BUILD_INDEXER)
    include(ExternalProject)

    set(CARGO_BUILD_TYPE "$<IF:$<CONFIG:Debug>,debug,release>")
    set(CARGO_TARGET_DIR "${CMAKE_BINARY_DIR}/cargo-target")

    ExternalProject_Add(btrdasd_rust
        SOURCE_DIR "${CMAKE_SOURCE_DIR}/indexer"
        CONFIGURE_COMMAND ""
        BUILD_COMMAND cargo build --release --manifest-path "${CMAKE_SOURCE_DIR}/indexer/Cargo.toml" --target-dir "${CARGO_TARGET_DIR}"
        INSTALL_COMMAND ""
        BUILD_IN_SOURCE FALSE
        BUILD_ALWAYS TRUE
    )

    install(PROGRAMS "${CARGO_TARGET_DIR}/release/btrdasd"
        DESTINATION "${CMAKE_INSTALL_PREFIX}/bin")
endif()

# =============================================================================
# Install targets for reference scripts and config
# =============================================================================

set(DAS_SCRIPT_DIR "${CMAKE_INSTALL_PREFIX}/lib/das-backup")

install(PROGRAMS
    scripts/backup-run.sh
    scripts/backup-verify.sh
    scripts/boot-archive-cleanup.sh
    scripts/das-partition-drives.sh
    scripts/install-backup-timer.sh
    DESTINATION "${DAS_SCRIPT_DIR}"
)

install(FILES
    config/btrbk.conf
    config/das-backup-email.conf.example
    DESTINATION "${DAS_SCRIPT_DIR}/config"
)

# =============================================================================
# Systemd units (templated for prefix flexibility)
# =============================================================================

configure_file(systemd/das-backup.service.in
    ${CMAKE_BINARY_DIR}/das-backup.service @ONLY)
configure_file(systemd/das-backup-full.service.in
    ${CMAKE_BINARY_DIR}/das-backup-full.service @ONLY)

install(FILES
    ${CMAKE_BINARY_DIR}/das-backup.service
    ${CMAKE_BINARY_DIR}/das-backup-full.service
    systemd/das-backup.timer
    systemd/das-backup-full.timer
    DESTINATION lib/systemd/system
)

# =============================================================================
# KDE Plasma GUI (optional)
# =============================================================================

if(BUILD_GUI)
    add_subdirectory(gui)
endif()
