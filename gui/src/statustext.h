#pragma once

#include <QString>
#include <QStringList>

// The main window's status line, from one 60 s tick's three background
// replies (an empty string is a reply that did not come) and the refreshes
// that failed ("<call>: <reason>"). A reading that did not come is said to be
// unknown or left out, never shown as 0 or as the previous tick's
// (bd DAS-Backup-Manager-amsg).
struct StatusText {
    QString text;
    QString toolTip;

    static StatusText assemble(const QString &statsJson, const QString &scheduleJson,
                               const QString &healthJson, const QStringList &failed);

    // The dashboard's "Last Backup" line from a HealthQuery reply's
    // `services.last_backup_age_secs`: how long ago the newest successful run
    // ended (-1: none recorded, or the history could not be read).
    static QString lastBackupLine(qint64 ageSecs);
};
