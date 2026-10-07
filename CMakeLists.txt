cmake_minimum_required(VERSION 3.25)
project(DAS-Backup-Manager
    VERSION 0.7.23.0
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
option(REQUIRE_ALL_SHELL_CASES "Fail a shell suite that prints NOT RUN, a case its host could not run (CI sets this)" OFF)

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
# Install targets for reference scripts and config
# =============================================================================

set(DAS_SCRIPT_DIR "${CMAKE_INSTALL_PREFIX}/lib/das-backup")

install(PROGRAMS
    scripts/backup-run.sh
    scripts/backup-verify.sh
    scripts/boot-archive-cleanup.sh
    scripts/das-partition-drives.sh
    scripts/install-backup-timer.sh
    scripts/recovery-os-vm.sh
    DESTINATION "${DAS_SCRIPT_DIR}"
)

install(FILES
    config/btrbk.conf
    DESTINATION "${DAS_SCRIPT_DIR}/config"
)

# The recovery-os-updater VM (bd DAS-Backup-Manager-7wb). recovery-os-vm.sh
# defines the domain from this installed copy, found beside itself.
install(FILES
    packaging/libvirt/recovery-os-updater.xml
    DESTINATION "${DAS_SCRIPT_DIR}/libvirt"
)

# =============================================================================
# Backup units: written by `btrdasd setup`, not installed here
# =============================================================================
#
# das-backup{,-full}.{service,timer} — like das-scrub and das-backup-doctor —
# are rendered by `btrdasd setup` from config.toml into /etc/systemd/system
# (cron entries on sysvinit and OpenRC): their schedule, prefix and scrub
# state are the host's, so the only copy ever right is setup's. Until
# bd DAS-Backup-Manager-7rf CMake also installed its own copies under
# lib/systemd/system, which systemd ran whenever setup's were gone — with a
# USB-glob condition that skipped runs silently and a six-hour timeout that
# setup's units never had. `btrdasd setup --upgrade` removes the copies older
# versions left (indexer/src/setup/retired_units.rs).

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

    # Installing enables and reloads nothing. The helper needs no enabling:
    # D-Bus starts it on first use (SystemdService= in
    # dbus/org.dasbackup.Helper1.service), and `btrdasd setup --upgrade`
    # reloads systemd and restarts it after an update. This block used to run
    # `systemctl daemon-reload` and `systemctl enable btrdasd-helper.service`
    # from install(CODE): on the INSTALLING machine — under DESTDIR, the
    # package build host — with ERROR_QUIET discarding every failure
    # (bd DAS-Backup-Manager-cway).
endif()

# =============================================================================
# Man page
# =============================================================================

install(FILES docs/btrdasd.1 DESTINATION "${CMAKE_INSTALL_PREFIX}/share/man/man1")

# =============================================================================
# Shell completions (generated at build time, installed as files)
# =============================================================================
#
# The btrdasd this build makes prints them into the build tree, and they are
# installed like any other file, so a DESTDIR install carries them. They used
# to be generated at INSTALL time, by running ${CMAKE_INSTALL_PREFIX}/bin/btrdasd
# and writing under ${CMAKE_INSTALL_PREFIX}/share: outside DESTDIR, with
# whatever btrdasd the installing machine had (an old one, or none), and with
# ERROR_QUIET discarding every failure (bd DAS-Backup-Manager-cway).
#
# Relative destinations, resolved against the prefix at install time — where
# the old block wrote them. The three paths are on `btrdasd setup
# --uninstall-all`'s list (cmake_installed_paths in indexer/src/setup/installer.rs).

if(BUILD_INDEXER)
    set(DAS_COMPLETIONS_DIR "${CMAKE_BINARY_DIR}/completions")
    add_custom_target(btrdasd_completions ALL
        COMMAND "${CMAKE_COMMAND}"
            "-DBTRDASD=${CARGO_TARGET_DIR}/release/btrdasd"
            "-DOUT_DIR=${DAS_COMPLETIONS_DIR}"
            -P "${CMAKE_SOURCE_DIR}/cmake/GenerateCompletions.cmake"
        BYPRODUCTS
            "${DAS_COMPLETIONS_DIR}/btrdasd.bash"
            "${DAS_COMPLETIONS_DIR}/_btrdasd"
            "${DAS_COMPLETIONS_DIR}/btrdasd.fish"
        COMMENT "Generating bash, zsh and fish completions from the btrdasd just built"
        VERBATIM
    )
    # Only once cargo has built the binary.
    add_dependencies(btrdasd_completions btrdasd_rust)

    install(FILES "${DAS_COMPLETIONS_DIR}/btrdasd.bash"
        DESTINATION share/bash-completion/completions
        RENAME btrdasd)
    install(FILES "${DAS_COMPLETIONS_DIR}/_btrdasd"
        DESTINATION share/zsh/site-functions)
    install(FILES "${DAS_COMPLETIONS_DIR}/btrdasd.fish"
        DESTINATION share/fish/vendor_completions.d)
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
    # With the binary this build makes, the suite replays each aborted run's
    # record-run vector through the real btrdasd (bd DAS-Backup-Manager-2my);
    # without it the summary says those replays did not run.
    if(BUILD_INDEXER)
        set_tests_properties(shell-backup-exit-semantics PROPERTIES
            ENVIRONMENT "REAL_BTRDASD=${CMAKE_BINARY_DIR}/cargo-target/release/btrdasd")
    endif()
    add_test(
        NAME shell-verify-sources
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_verify_sources.sh
    )
    add_test(
        NAME shell-subvol-sync
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_subvol_sync.sh
    )
    add_test(
        NAME shell-unmount-all
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_unmount_all.sh
    )
    add_test(
        NAME shell-recovery-os
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_recovery_os.sh
    )
    add_test(
        NAME shell-recovery-os-vm
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_recovery_os_vm.sh
    )
    # With the binary this build makes, the suite also reads boot records the
    # real btrdasd wrote (the contract, bd DAS-Backup-Manager-1yg and -df0);
    # without it the summary says they did not run.
    if(BUILD_INDEXER)
        set_tests_properties(shell-recovery-os-vm PROPERTIES
            ENVIRONMENT "REAL_BTRDASD=${CMAKE_BINARY_DIR}/cargo-target/release/btrdasd")
    endif()
    add_test(
        NAME shell-maintenance-lock
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_maintenance_lock.sh
    )
    add_test(
        NAME shell-record-run
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_record_run.sh
    )
    add_test(
        NAME shell-script-rewrite
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_script_rewrite.sh
    )
    # parent_disk: the disk under a partition comes from lsblk, and an answer
    # lsblk cannot give is "unknown" (bd DAS-Backup-Manager-b1s).
    add_test(
        NAME shell-parent-disk
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_parent_disk.sh
    )
    add_test(
        NAME shell-early-exit-readers
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_early_exit_readers.sh
    )
    # boot-archive-cleanup.sh itself, run for real in a sandbox of stub btrfs,
    # mountpoint and btrdasd: what it may delete (bd DAS-Backup-Manager-1bsx),
    # and what it prints, read back by backup-run.sh's run_archive_cleanup in
    # a dry run and a real one (bd DAS-Backup-Manager-zwr), and a target whose
    # mount state it cannot tell (bd DAS-Backup-Manager-2dmn). Nothing is
    # listed, mounted or deleted.
    add_test(
        NAME shell-boot-archive-cleanup
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_boot_archive_cleanup.sh
    )
    # Installs this very build into a scratch DESTDIR: every file lands there,
    # the completions among them, and installing runs nothing on this machine
    # (bd DAS-Backup-Manager-cway). Needs the finished build, as ctest does.
    if(BUILD_INDEXER)
        set(DAS_EXPECT_COMPLETIONS --completions)
    else()
        set(DAS_EXPECT_COMPLETIONS --no-completions)
    endif()
    add_test(
        NAME shell-install-destdir
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_install_destdir.sh
            ${CMAKE_COMMAND} ${CMAKE_BINARY_DIR} ${DAS_EXPECT_COMPLETIONS}
    )
    # The config docs/INSTALL.md shows for an installation without the wizard
    # is one the btrdasd this build makes accepts (bd DAS-Backup-Manager-x0m1).
    if(BUILD_INDEXER)
        add_test(
            NAME shell-install-doc-config
            COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_install_doc_config.sh
                ${CMAKE_BINARY_DIR}/cargo-target/release/btrdasd
        )
    endif()
    # CI's Arch container jobs install through .github/scripts/pacman-install.sh,
    # which retries a failed install (bd DAS-Backup-Manager-ravr). This runs that
    # script against a stub pacman in a scratch tree: nothing is installed.
    add_test(
        NAME shell-pacman-install
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_pacman_install.sh
    )

    # A suite whose host cannot run a case prints "NOT RUN" and exits 0: right
    # on a developer's machine, and a silent hole in CI. Five suites skipped
    # their en_US.UTF-8 cases on every CI run for as long as the Arch container
    # had no such locale, and passed (bd DAS-Backup-Manager-1bsx). CI generates
    # the locale and sets REQUIRE_ALL_SHELL_CASES; then a "NOT RUN" anywhere in a
    # suite's output fails that suite, so a pass means every case ran. The
    # exit suite's replays through the real btrdasd are such a case too: ctest
    # sets REAL_BTRDASD for them whenever the indexer is built.
    if(REQUIRE_ALL_SHELL_CASES)
        get_property(shell_suites DIRECTORY PROPERTY TESTS)
        if(NOT shell_suites)
            message(FATAL_ERROR "REQUIRE_ALL_SHELL_CASES is on and no shell suite was found to apply it to")
        endif()
        set_tests_properties(${shell_suites} PROPERTIES FAIL_REGULAR_EXPRESSION "NOT RUN")
    endif()
endif()

# =============================================================================
# What installing leaves to setup
# =============================================================================
#
# Last, so it prints after every file. Installing writes files and nothing
# else; the host is configured by `btrdasd setup`, which writes config.toml
# and the units and enables the timers (bd DAS-Backup-Manager-cway).

install(CODE [[
    message(STATUS "Installed files only: nothing was enabled, started or reloaded.")
    message(STATUS "Next, on the machine that runs the backups: `sudo btrdasd setup` "
                   "for a first install, `sudo btrdasd setup --upgrade` after an update "
                   "(docs/INSTALL.md).")
]])
