#pragma once

#include <QSet>
#include <QString>
#include <QStringList>

// The jobs the D-Bus helper is running for this GUI, by id, from the reply
// that started each one until its JobFinished (bd DAS-Backup-Manager-hoh).
//
// The helper sends JobFinished for every job it ends — except when it is
// restarted (an upgrade runs `systemctl try-restart`): the old helper exits,
// and the jobs it was running end without one. When the helper's bus name
// loses its owner, every job still open is over, and the GUI says so rather
// than waiting for a signal that will never come. Pure, so the smoke test
// pins it without a bus.
class HelperJobs
{
public:
    void started(const QString &jobId) { m_open.insert(jobId); }
    void finished(const QString &jobId) { m_open.remove(jobId); }

    // The bus name's owner changed from `oldOwner` to `newOwner` (either
    // empty for none). A name that had an owner and lost it, or passed to
    // another, ends every open job: returns them, sorted, and forgets them.
    // A name gaining its first owner ends nothing.
    QStringList ownerChanged(const QString &oldOwner, const QString &newOwner)
    {
        Q_UNUSED(newOwner)
        if (oldOwner.isEmpty() || m_open.isEmpty())
            return {};
        QStringList ended(m_open.cbegin(), m_open.cend());
        ended.sort();
        m_open.clear();
        return ended;
    }

    // The summary a job ended this way is reported with.
    static QString endedSummary()
    {
        return QStringLiteral(
            "The job ended without a result: the D-Bus helper restarted while it "
            "ran (an upgrade restarts it). Check the backup history before running "
            "it again.");
    }

private:
    QSet<QString> m_open;
};
