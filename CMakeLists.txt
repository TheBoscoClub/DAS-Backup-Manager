cmake_minimum_required(VERSION 3.25)
project(DAS-Backup-Manager
    VERSION 0.7.22.0
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
option(BUILD_HELPER "Build btrdasd-helper D-Bus daemon (requires zbus)" ON)
option(BUILD_FFI "Build libbuttered_dasd_ffi.so shared library for GUI" ON)

# =============================================================================
# Defense-in-depth: refuse to install zero-byte Rust binaries
# =============================================================================
#
# Three real incidents (May 2026) had cmake --install copy 0-byte stubs over
# valid binaries in /usr, producing services that failed at runtime with exec
# status 203/EXEC (no ELF magic, no shebang). The root cause is that cargo
# can leave an empty file at the output path when a build is interrupted, and
# install(PROGRAMS) faithfully copies whatever's there.
#
# This helper emits an install(CODE) block that aborts the install if the
# given artifact is missing or zero bytes — catching the bug at install time,
# before /usr is corrupted. See bd issues DAS-Backup-Manager-29j (root cause
# investigation) and DAS-Backup-Manager-9rx (this defense).
#
function(das_require_nonzero_artifact PATH DESCRIPTION)
    install(CODE "
        set(_p \"${PATH}\")
        if(NOT EXISTS \"\${_p}\")
            message(FATAL_ERROR
                \"\\n\\n=== INSTALL ABORTED ===\\n\"
                \"Pre-install check failed: ${DESCRIPTION} is missing.\\n\"
                \"  Expected at: \${_p}\\n\\n\"
                \"This usually means 'cmake --install build' was run before \"
                \"'cmake --build build' — ExternalProject_Add artifacts are \"
                \"produced at build time, not configure time.\\n\\n\"
                \"Recovery: run 'cmake --build build' first.\")
        endif()
        file(SIZE \"\${_p}\" _sz)
        if(_sz EQUAL 0)
            message(FATAL_ERROR
                \"\\n\\n=== INSTALL ABORTED ===\\n\"
                \"Pre-install check failed: ${DESCRIPTION} is zero bytes.\\n\"
                \"  Path: \${_p}\\n\\n\"
                \"This is the failure class tracked by bd DAS-Backup-Manager-29j: \"
                \"cargo can leave an empty stub at the output path when a previous \"
                \"build was interrupted. install(PROGRAMS) would faithfully copy \"
                \"that stub to /usr, producing a binary that fails at runtime with \"
                \"exec status 203/EXEC (Exec format error — no ELF magic, no shebang).\\n\\n\"
                \"Recovery: run 'cmake --build build' to rebuild ${DESCRIPTION}, \"
                \"verify the output binary is non-zero, then re-run install.\")
        endif()
        message(STATUS \"Install check OK: ${DESCRIPTION} (\${_sz} bytes)\")
    ")
endfunction()

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

    das_require_nonzero_artifact("${CARGO_TARGET_DIR}/release/btrdasd"
                                  "btrdasd CLI binary")
    install(PROGRAMS "${CARGO_TARGET_DIR}/release/btrdasd"
        DESTINATION "${CMAKE_INSTALL_PREFIX}/bin")
endif()

# =============================================================================
# D-Bus helper daemon (btrdasd-helper, requires zbus/tokio)
# =============================================================================

if(BUILD_HELPER)
    include(ExternalProject)

    set(CARGO_TARGET_DIR "${CMAKE_BINARY_DIR}/cargo-target")

    ExternalProject_Add(btrdasd_helper_rust
        SOURCE_DIR "${CMAKE_SOURCE_DIR}/indexer"
        CONFIGURE_COMMAND ""
        BUILD_COMMAND cargo build --release --features dbus --bin btrdasd-helper
            --manifest-path "${CMAKE_SOURCE_DIR}/indexer/Cargo.toml"
            --target-dir "${CARGO_TARGET_DIR}"
        INSTALL_COMMAND ""
        BUILD_IN_SOURCE FALSE
        BUILD_ALWAYS TRUE
    )

    das_require_nonzero_artifact("${CARGO_TARGET_DIR}/release/btrdasd-helper"
                                  "btrdasd-helper D-Bus daemon")
    install(PROGRAMS "${CARGO_TARGET_DIR}/release/btrdasd-helper"
        DESTINATION "${CMAKE_INSTALL_PREFIX}/libexec")
endif()

# =============================================================================
# FFI shared library (libbuttered_dasd_ffi.so)
# =============================================================================

if(BUILD_FFI)
    include(ExternalProject)

    set(CARGO_TARGET_DIR "${CMAKE_BINARY_DIR}/cargo-target")

    ExternalProject_Add(buttered_dasd_ffi
        SOURCE_DIR "${CMAKE_SOURCE_DIR}/indexer"
        CONFIGURE_COMMAND ""
        BUILD_COMMAND cargo build --release --features ffi
            --manifest-path "${CMAKE_SOURCE_DIR}/indexer/Cargo.toml"
            --target-dir "${CARGO_TARGET_DIR}"
        INSTALL_COMMAND ""
        BUILD_IN_SOURCE FALSE
        BUILD_ALWAYS TRUE
    )

    das_require_nonzero_artifact("${CARGO_TARGET_DIR}/release/libbuttered_dasd.so"
                                  "libbuttered_dasd FFI library")
    install(FILES "${CARGO_TARGET_DIR}/release/libbuttered_dasd.so"
        DESTINATION "${CMAKE_INSTALL_PREFIX}/lib"
        RENAME "libbuttered_dasd_ffi.so")

    install(FILES "${CMAKE_SOURCE_DIR}/indexer/include/btrdasd_ffi.h"
        DESTINATION "${CMAKE_INSTALL_PREFIX}/include")
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
# D-Bus and polkit integration files
# =============================================================================

if(BUILD_HELPER)
    install(FILES dbus/org.dasbackup.Helper1.conf
        DESTINATION "${CMAKE_INSTALL_PREFIX}/share/dbus-1/system.d")

    install(FILES dbus/org.dasbackup.Helper1.service
        DESTINATION "${CMAKE_INSTALL_PREFIX}/share/dbus-1/system-services")

    install(FILES polkit/org.dasbackup.policy
        DESTINATION "${CMAKE_INSTALL_PREFIX}/share/polkit-1/actions")

    install(FILES systemd/btrdasd-helper.service
        DESTINATION lib/systemd/system)

    install(CODE "
        execute_process(
            COMMAND systemctl daemon-reload
            ERROR_QUIET
        )
        execute_process(
            COMMAND systemctl enable btrdasd-helper.service
            ERROR_QUIET
        )
    ")
endif()

# =============================================================================
# Man page
# =============================================================================

install(FILES docs/btrdasd.1 DESTINATION "${CMAKE_INSTALL_PREFIX}/share/man/man1")

# =============================================================================
# Shell completions (generated at install time)
# =============================================================================

if(BUILD_INDEXER)
    install(CODE "
        execute_process(
            COMMAND \${CMAKE_INSTALL_PREFIX}/bin/btrdasd completions bash
            OUTPUT_FILE \${CMAKE_INSTALL_PREFIX}/share/bash-completion/completions/btrdasd
            ERROR_QUIET
        )
        execute_process(
            COMMAND \${CMAKE_INSTALL_PREFIX}/bin/btrdasd completions zsh
            OUTPUT_FILE \${CMAKE_INSTALL_PREFIX}/share/zsh/site-functions/_btrdasd
            ERROR_QUIET
        )
        execute_process(
            COMMAND \${CMAKE_INSTALL_PREFIX}/bin/btrdasd completions fish
            OUTPUT_FILE \${CMAKE_INSTALL_PREFIX}/share/fish/vendor_completions.d/btrdasd.fish
            ERROR_QUIET
        )
    ")
endif()

# =============================================================================
# KDE Plasma GUI (optional)
# =============================================================================

if(BUILD_GUI)
    add_subdirectory(gui)
endif()

# =============================================================================
# Shell regression suites
# =============================================================================
#
# CTest is already included above. These two suites existed for months with no
# runner at all -- `add_test` appeared exactly once in the repo (the GUI smoke
# test) and CI's shellcheck job globbed only scripts/*.sh. A test nothing runs
# has never been observed failing, and never being observed failing is the one
# thing that stops a test being evidence.
#
# Both are self-contained: each extracts the function under test from the live
# script with sed, stubs the surrounding globals, and writes only beneath a
# mktemp directory. No root, no devices, no network -- safe in a container.
if(BUILD_TESTING)
    add_test(
        NAME shell-create-snapshot-dirs
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_create_snapshot_dirs.sh
    )
    add_test(
        NAME shell-esp-label-derivation
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_esp_label_derivation.sh
    )
    add_test(
        NAME shell-backup-exit-semantics
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_backup_exit_semantics.sh
    )
endif()
