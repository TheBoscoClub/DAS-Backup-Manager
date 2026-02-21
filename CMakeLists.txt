cmake_minimum_required(VERSION 3.25)
project(DAS-Backup-Manager
    VERSION 0.1.0
    DESCRIPTION "DAS backup manager with btrbk integration, SQLite FTS5 content indexing, and KDE Plasma GUI"
    LANGUAGES CXX
)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# =============================================================================
# Install targets for scripts and systemd units
# =============================================================================

# Scripts install to /usr/local/lib/das-backup/
set(DAS_SCRIPT_DIR "${CMAKE_INSTALL_PREFIX}/lib/das-backup")

install(PROGRAMS
    scripts/backup-run.sh
    scripts/backup-verify.sh
    scripts/boot-archive-cleanup.sh
    scripts/das-partition-drives.sh
    scripts/install-backup-timer.sh
    DESTINATION "${DAS_SCRIPT_DIR}"
)

# btrbk reference config (not installed to /etc — user copies manually)
install(FILES
    config/btrbk.conf
    config/das-backup-email.conf.example
    DESTINATION "${DAS_SCRIPT_DIR}/config"
)

# systemd units install to /etc/systemd/system/
install(FILES
    systemd/das-backup.service
    systemd/das-backup.timer
    systemd/das-backup-full.service
    systemd/das-backup-full.timer
    DESTINATION "/etc/systemd/system"
)

# =============================================================================
# Future components (uncomment when ready)
# =============================================================================

# add_subdirectory(indexer)  # C++ SQLite FTS5 content indexer
# add_subdirectory(gui)      # Qt6/KDE Plasma backup browser
