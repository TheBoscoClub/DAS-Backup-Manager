#pragma once

#include <QObject>
#include <QString>
#include <QStringList>

class QDBusInterface;
class QDBusPendingCallWatcher;

class DBusClient : public QObject
{
    Q_OBJECT

public:
    explicit DBusClient(QObject *parent = nullptr);
    ~DBusClient() override;

    [[nodiscard]] bool isAvailable() const;

    // Async job-returning methods (return job_id via signal)
    void backupRun(const QString &configPath, const QString &mode,
                   const QStringList &sources, const QStringList &targets,
                   bool dryRun);
    void backupSnapshot(const QString &configPath, const QStringList &sources);
    void backupSend(const QString &configPath, const QStringList &targets);
    void backupBootArchive(const QString &configPath);
    void indexWalk(const QString &targetPath, const QString &dbPath);
    void restoreFiles(const QString &snapshot, const QString &dest,
                      const QStringList &files);
    void restoreSnapshot(const QString &snapshot, const QString &dest);

    // Synchronous methods
    QString configGet(const QString &configPath);
    bool configSet(const QString &configPath, const QString &tomlContent);
    QString scheduleGet(const QString &configPath);
    bool scheduleSet(const QString &configPath, const QString &incremental,
                     const QString &full, quint32 delay);
    bool scheduleEnable(const QString &configPath, bool enabled);
    bool subvolAdd(const QString &configPath, const QString &source,
                   const QString &name);
    bool subvolRemove(const QString &configPath, const QString &source,
                      const QString &name);
    bool subvolSetManual(const QString &configPath, const QString &source,
                         const QString &name, bool manual);
    QString healthQuery(const QString &configPath);
    bool jobCancel(const QString &jobId);

Q_SIGNALS:
    void jobStarted(const QString &jobId, const QString &operation);
    void jobProgress(const QString &jobId, const QString &stage,
                     int percent, const QString &message);
    void jobLog(const QString &jobId, const QString &level,
                const QString &message);
    void jobFinished(const QString &jobId, bool success,
                     const QString &summary);
    void errorOccurred(const QString &operation, const QString &error);

private Q_SLOTS:
    void onJobProgress(const QString &jobId, const QString &stage,
                       int percent, const QString &message);
    void onJobLog(const QString &jobId, const QString &level,
                  const QString &message);
    void onJobFinished(const QString &jobId, bool success,
                       const QString &summary);

private:
    void callAsync(const QString &method, const QList<QVariant> &args,
                   const QString &operation);
    [[nodiscard]] static QString mapDBusError(const QString &errorName,
                                              const QString &errorMessage);

    QDBusInterface *m_interface = nullptr;
    bool m_available = false;
};
