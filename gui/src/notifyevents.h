#pragma once

#include <QString>

// Every KNotification event id the GUI sends. Each must have an
// [Event/<id>] group in btrdasd-gui.notifyrc, or KNotifications drops the
// event ("No event config could be found", bd DAS-Backup-Manager-w1a0);
// ctest gui-notifyrc checks it. Send events only through these names.
namespace NotifyEvent {
inline const QString BackupComplete = QStringLiteral("backupComplete");
inline const QString BackupFailed = QStringLiteral("backupFailed");
inline const QString RecoveryOsComplete = QStringLiteral("recoveryOsComplete");
inline const QString RecoveryOsFailed = QStringLiteral("recoveryOsFailed");
} // namespace NotifyEvent
