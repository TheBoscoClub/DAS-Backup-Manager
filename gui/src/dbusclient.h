#pragma once

#include "helperjobs.h"

#include <QObject>
#include <QPair>
#include <QString>
#include <QStringList>

#include <optional>

struct BackupSteps;
class QDBusInterface;
class QDBusPendingCall;
class QDBusPendingCallWatcher;

class DBusClient : public QObject
{
    Q_OBJECT

public:
    explicit DBusClient(QObject *parent = nullptr);
    ~DBusClient() override;

    [[nodiscard]] bool isAvailable() const;
    [[nodiscard]] QString unavailableReason() const;

    // Async job-returning methods (return job_id via signal)
    void backupRun(const QString &mode,
                   const QStringList &sources, const QStringList &targets,
                   bool dryRun, const BackupSteps &steps);
    void indexWalk(const QString &targetPath);
    void restoreFiles(const QString &snapshot,
                      const QString &dest, const QStringList &files);
    void restoreSnapshot(const QString &snapshot,
                         const QString &dest);

    // Synchronous methods
    QString configGet();
    // What the Backup panel shows, as JSON (see panelconfig.h); empty on failure.
    QString panelConfigGet();
    // Warnings for `tomlContent` (keys a known table does not have, with the
    // nearest key); nullopt when the helper could not check it.
    std::optional<QStringList> configCheck(const QString &tomlContent);
    bool configSet(const QString &tomlContent);
    QString scheduleGet();
    bool scheduleSet(const QString &incremental,
                     const QString &full, quint32 delay);
    bool scheduleEnable(bool enabled);
    bool subvolAdd(const QString &source,
                   const QString &name);
    bool subvolRemove(const QString &source,
                      const QString &name);
    bool jobCancel(const QString &jobId);
    // Subvolume coverage (bd DAS-Backup-Manager-sp78.4). The pairs no longer
    // backed up and the accept log, as JSON; nullopt when the call failed
    // (errorOccurred says why).
    std::optional<QString> coverageList();
    // Accept one pair, named whole; what the helper did, or nullopt when it
    // refused (errorOccurred says why: polkit, or the pair is not reported).
    std::optional<QString> coverageAccept(const QString &volume,
                                          const QString &subvolume,
                                          const QString &target);

    // Async versions of slow methods (non-blocking, result via signal)
    void healthQueryAsync();
    // Turns a HealthQuery reply into healthQueryResult, and a failed one into
    // refreshFailed first: every caller is a background refresh. Public so a
    // test can hand it a completed error reply.
    void watchHealthQuery(const QDBusPendingCall &pending);
    void scheduleGetAsync();
    void indexStatsAsync();
    void indexListSnapshotsAsync();

    // Index read methods (read-only, no polkit auth for active sessions)
    QString indexStats();
    QString indexListSnapshots();
    QString indexListFiles(qint64 snapshotId,
                           qint64 limit = 10000, qint64 offset = 0);
    QString indexSearch(const QString &query, qint64 limit);
    QString indexBackupHistory(qint64 limit);
    QString indexSnapshotPath(qint64 snapshotId);

    // --- Recovery drives (bd DAS-Backup-Manager-8249 stage 3) ---
    void recoveryOsStatusAsync();                                   // -> recoveryOsStatusResult(json); empty on error/unavailable
    void recoveryOsSession(const QStringList &labels, bool unattended,
                           const QString &mode);                    // a job: jobStarted(id, "Recovery OS session"); never passes accept_boot_record_risk
    void recoveryOsRecord(const QString &label);                  // a job: jobStarted(id, "Recovery OS record"); the CLI's `recovery-os record` for one drive
    void recoveryOsSessionEnd(const QString &label);                // -> recoveryOsSessionEndResult(label, ok, lines); 10-minute timeout
    void recoveryOsScheduleSet(const QStringList &labels, qint64 atEpoch,
                               const QString &mode);                // -> recoveryOsScheduleResult(unit); atEpoch 0 clears
    void recoveryOsConsole(const QString &label);                   // -> recoveryOsConsoleResult(label, path)
    // The viewer to launch for a console socket: program and arguments. Pure,
    // so the test can pin it without a socket.
    [[nodiscard]] static QPair<QString, QStringList> consoleCommand(const QString &socketPath);
    static constexpr int SessionEndTimeoutMs = 600000;
    [[nodiscard]] int sessionEndTimeoutMs() const; // what the session-end interface is really set to

Q_SIGNALS:
    void jobStarted(const QString &jobId, const QString &operation);
    void jobProgress(const QString &jobId, const QString &stage,
                     int percent, const QString &message);
    void jobLog(const QString &jobId, const QString &level,
                const QString &message);
    void jobFinished(const QString &jobId, bool success,
                     const QString &summary);
    void errorOccurred(const QString &operation, const QString &error);
    // A job-starting call dropped because no helper is reachable: no job and
    // no reply will follow. Not an error dialog — helperUnavailable said it
    // once. Queued, so it arrives after the caller has marked its request.
    void callDropped(const QString &operation);

    // Fired once, deferred to the next event-loop iteration, if the helper
    // could not be activated at startup. Collapses what would otherwise be
    // one errorOccurred per view's first call (a six-dialog cascade) into a
    // single notification. See bd issue DAS-Backup-Manager-mw0.
    void helperUnavailable(const QString &reason);

    // Async result signals
    void healthQueryResult(const QString &json);
    // A background refresh that failed (HealthQuery, ScheduleGet, IndexStats):
    // shown in place by the status bar and the dashboard; not errorOccurred,
    // which the main window turns into a modal dialog, and these run unasked
    // every 60 s (bd DAS-Backup-Manager-amsg).
    void refreshFailed(const QString &operation, const QString &reason);
    void scheduleGetResult(const QString &json);
    void indexStatsResult(const QString &json);
    void indexListSnapshotsResult(const QString &json);
    void recoveryOsStatusResult(const QString &json);
    // A RecoveryOsStatus call that failed: the panel shows it in place and
    // disables its buttons; it is not errorOccurred, which the main window
    // turns into a modal dialog and the panel refreshes every 30 s.
    void recoveryOsStatusError(const QString &reason);
    void recoveryOsSessionEndResult(const QString &label, bool ok, const QString &lines);
    void recoveryOsScheduleResult(const QString &unit);
    void recoveryOsConsoleResult(const QString &label, const QString &path);

private Q_SLOTS:
    // The helper's bus name changed owner: the jobs it was running are over
    // (helperjobs.h, bd DAS-Backup-Manager-hoh).
    void onHelperOwnerChanged(const QString &service, const QString &oldOwner,
                              const QString &newOwner);
    void onJobProgress(const QString &jobId, const QString &stage,
                       int percent, const QString &message);
    void onJobLog(const QString &jobId, const QString &level,
                  const QString &message);
    void onJobFinished(const QString &jobId, bool success,
                       const QString &summary);

public:
    // Public because it is a pure mapping from a D-Bus error onto the text the
    // user actually sees when the helper is unavailable, and that text is worth
    // pinning in a test (bd DAS-Backup-Manager-a59). No state is exposed.
    [[nodiscard]] static QString mapDBusError(const QString &errorName,
                                              const QString &errorMessage);

private:
    void callAsync(const QString &method, const QList<QVariant> &args,
                   const QString &operation);

    QDBusInterface *m_interface = nullptr;
    // Same service, path and name; only the call timeout differs
    // (QDBusAbstractInterface::setTimeout is per interface object).
    QDBusInterface *m_slowInterface = nullptr;
    bool m_available = false;
    QString m_unavailableReason;
    HelperJobs m_jobs;
};
