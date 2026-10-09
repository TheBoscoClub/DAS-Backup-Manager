#pragma once

#include <QObject>
#include <QPair>
#include <QString>
#include <QStringList>

struct BackupSteps;
class QDBusInterface;
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
    bool configSet(const QString &tomlContent);
    QString scheduleGet();
    bool scheduleSet(const QString &incremental,
                     const QString &full, quint32 delay);
    bool scheduleEnable(bool enabled);
    bool subvolAdd(const QString &source,
                   const QString &name);
    bool subvolRemove(const QString &source,
                      const QString &name);
    bool subvolSetManual(const QString &source,
                         const QString &name, bool manual);
    QString healthQuery();
    bool jobCancel(const QString &jobId);

    // Async versions of slow methods (non-blocking, result via signal)
    void healthQueryAsync();
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

    // Fired once, deferred to the next event-loop iteration, if the helper
    // could not be activated at startup. Collapses what would otherwise be
    // one errorOccurred per view's first call (a six-dialog cascade) into a
    // single notification. See bd issue DAS-Backup-Manager-mw0.
    void helperUnavailable(const QString &reason);

    // Async result signals
    void healthQueryResult(const QString &json);
    void scheduleGetResult(const QString &json);
    void indexStatsResult(const QString &json);
    void indexListSnapshotsResult(const QString &json);
    void recoveryOsStatusResult(const QString &json);
    void recoveryOsSessionEndResult(const QString &label, bool ok, const QString &lines);
    void recoveryOsScheduleResult(const QString &unit);
    void recoveryOsConsoleResult(const QString &label, const QString &path);

private Q_SLOTS:
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
};
