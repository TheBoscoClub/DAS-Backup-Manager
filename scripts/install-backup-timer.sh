#!/usr/bin/env zsh
# install-backup-timer.sh - Install DAS backup systemd service and timer
# Run as root: sudo ./install-backup-timer.sh

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: This script must be run as root"
    exit 1
fi

SCRIPT_DIR="${0:A:h}"

echo "Installing DAS backup systemd units..."

PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Copy service and timer files
cp "$PROJECT_DIR/systemd/das-backup.service" /etc/systemd/system/
cp "$PROJECT_DIR/systemd/das-backup.timer" /etc/systemd/system/
cp "$PROJECT_DIR/systemd/das-backup-full.service" /etc/systemd/system/
cp "$PROJECT_DIR/systemd/das-backup-full.timer" /etc/systemd/system/

# Reload systemd
systemctl daemon-reload

# Enable timers (but don't start - user decides when)
systemctl enable das-backup.timer
systemctl enable das-backup-full.timer

echo ""
echo "Installed:"
echo "  /etc/systemd/system/das-backup.service"
echo "  /etc/systemd/system/das-backup.timer"
echo "  /etc/systemd/system/das-backup-full.service"
echo "  /etc/systemd/system/das-backup-full.timer"
echo ""
echo "To start nightly backups:"
echo "  sudo systemctl start das-backup.timer"
echo ""
echo "To run a backup now:"
echo "  sudo systemctl start das-backup.service"
echo ""
echo "To check timer status:"
echo "  systemctl list-timers das-backup*"
echo ""
echo "To view logs:"
echo "  journalctl -u das-backup.service"
