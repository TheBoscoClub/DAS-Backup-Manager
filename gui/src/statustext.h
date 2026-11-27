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

// A job's failure, refusal or stop as a status label shows it (spec
// 2026-11-27 reader-friendly output, bd DAS-Backup-Manager-3gzs.12): the
// status word and the deciding line first and set apart, the remaining
// segments of the job's text listed beneath, a long list cut to a count.
// Nothing is lost: the whole of the job's text stays in `plain`, which the
// label carries as its tooltip.
struct OutcomeText {
    QString html;  // every piece of helper or script text escaped; for a Qt::RichText label
    QString plain; // the whole text, as the label showed it before; tooltip and test hook

    // `word`: FAILED, REFUSED, NOT STARTED ... `summary`: the job's own text,
    // segments separated by "; " or newlines. `lead`: the deciding line when
    // the caller knows it (a refusal's reason), else the first segment that
    // names a refusal, failure or stop, else the first. `note`: the remedy,
    // one sentence, between the deciding line and the segments.
    static OutcomeText make(const QString &word, const QString &summary,
                            const QString &lead = QString(), const QString &note = QString());

    // The job's text cut into its segments, empty ones dropped.
    static QStringList segments(const QString &summary);

    // How many segments are listed before the rest is a count.
    static constexpr int ShownSegments = 5;

    // A tooltip that wraps and keeps its line breaks: Qt wraps only rich text.
    static QString tip(const QString &plain);
};
