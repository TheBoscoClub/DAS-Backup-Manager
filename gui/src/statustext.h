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
    // `services`: the age of the newest snapshot (ageSecs, -1 unknown), and,
    // when the helper did not walk the targets because a job holds them, that
    // the figure is a kept reading and how old (readingAgeSecs, -1 when read
    // for this reply), or why there is none (bd DAS-Backup-Manager-5237.16).
    static QString lastBackupLine(qint64 ageSecs, qint64 readingAgeSecs, bool targetsBusy);
};
