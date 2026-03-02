#include "dbusclient.h"

#include <QDBusConnection>
#include <QDBusInterface>
#include <QDBusPendingCall>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusReply>

namespace {
const auto ServiceName = QStringLiteral("org.dasbackup.Helper1");
const auto ObjectPath = QStringLiteral("/org/dasbackup/Helper1");
const auto InterfaceName = QStringLiteral("org.dasbackup.Helper1");
} // namespace

DBusClient::DBusClient(QObject *parent)
    : QObject(parent)
    , m_interface(new QDBusInterface(
          ServiceName, ObjectPath, InterfaceName,
          QDBusConnection::systemBus(), this))
{
    m_available = m_interface->isValid();

    // Connect D-Bus signals to local slots
    auto bus = QDBusConnection::systemBus();

    bus.connect(ServiceName, ObjectPath, InterfaceName,
                QStringLiteral("JobProgress"),
                this, SLOT(onJobProgress(QString,QString,int,QString)));

    bus.connect(ServiceName, ObjectPath, InterfaceName,
                QStringLiteral("JobLog"),
                this, SLOT(onJobLog(QString,QString,QString)));

    bus.connect(ServiceName, ObjectPath, InterfaceName,
                QStringLiteral("JobFinished"),
                this, SLOT(onJobFinished(QString,bool,QString)));
}

DBusClient::~DBusClient() = default;

bool DBusClient::isAvailable() const
{
    return m_available;
}

// --- Async job-returning methods ---

void DBusClient::backupRun(const QString &configPath, const QString &mode,
                           const QStringList &sources, const QStringList &targets,
                           bool dryRun)
{
    callAsync(QStringLiteral("BackupRun"),
              {configPath, mode, QVariant::fromValue(sources),
               QVariant::fromValue(targets), dryRun},
              QStringLiteral("BackupRun"));
}

void DBusClient::backupSnapshot(const QString &configPath, const QStringList &sources)
{
    callAsync(QStringLiteral("BackupSnapshot"),
              {configPath, QVariant::fromValue(sources)},
              QStringLiteral("BackupSnapshot"));
}

void DBusClient::backupSend(const QString &configPath, const QStringList &targets)
{
    callAsync(QStringLiteral("BackupSend"),
              {configPath, QVariant::fromValue(targets)},
              QStringLiteral("BackupSend"));
}

void DBusClient::backupBootArchive(const QString &configPath)
{
    callAsync(QStringLiteral("BackupBootArchive"),
              {configPath},
              QStringLiteral("BackupBootArchive"));
}

void DBusClient::indexWalk(const QString &configPath, const QString &targetPath,
                           const QString &dbPath)
{
    callAsync(QStringLiteral("IndexWalk"),
              {configPath, targetPath, dbPath},
              QStringLiteral("IndexWalk"));
}

void DBusClient::restoreFiles(const QString &configPath, const QString &snapshot,
                              const QString &dest, const QStringList &files)
{
    callAsync(QStringLiteral("RestoreFiles"),
              {configPath, snapshot, dest, QVariant::fromValue(files)},
              QStringLiteral("RestoreFiles"));
}

void DBusClient::restoreSnapshot(const QString &configPath, const QString &snapshot,
                                 const QString &dest)
{
    callAsync(QStringLiteral("RestoreSnapshot"),
              {configPath, snapshot, dest},
              QStringLiteral("RestoreSnapshot"));
}

// --- Synchronous methods ---

QString DBusClient::configGet(const QString &configPath)
{
    QDBusReply<QString> reply = m_interface->call(QStringLiteral("ConfigGet"), configPath);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ConfigGet"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

bool DBusClient::configSet(const QString &configPath, const QString &tomlContent)
{
    QDBusReply<void> reply = m_interface->call(QStringLiteral("ConfigSet"),
                                               configPath, tomlContent);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ConfigSet"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

QString DBusClient::scheduleGet(const QString &configPath)
{
    QDBusReply<QString> reply = m_interface->call(QStringLiteral("ScheduleGet"), configPath);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ScheduleGet"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

bool DBusClient::scheduleSet(const QString &configPath, const QString &incremental,
                             const QString &full, quint32 delay)
{
    QDBusReply<void> reply = m_interface->call(QStringLiteral("ScheduleSet"),
                                               configPath, incremental, full, delay);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ScheduleSet"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

bool DBusClient::scheduleEnable(const QString &configPath, bool enabled)
{
    QDBusReply<void> reply = m_interface->call(QStringLiteral("ScheduleEnable"),
                                               configPath, enabled);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ScheduleEnable"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

bool DBusClient::subvolAdd(const QString &configPath, const QString &source,
                           const QString &name)
{
    QDBusReply<void> reply = m_interface->call(QStringLiteral("SubvolAdd"),
                                               configPath, source, name);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("SubvolAdd"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

bool DBusClient::subvolRemove(const QString &configPath, const QString &source,
                              const QString &name)
{
    QDBusReply<void> reply = m_interface->call(QStringLiteral("SubvolRemove"),
                                               configPath, source, name);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("SubvolRemove"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

bool DBusClient::subvolSetManual(const QString &configPath, const QString &source,
                                 const QString &name, bool manual)
{
    QDBusReply<void> reply = m_interface->call(QStringLiteral("SubvolSetManual"),
                                               configPath, source, name, manual);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("SubvolSetManual"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

QString DBusClient::healthQuery(const QString &configPath)
{
    QDBusReply<QString> reply = m_interface->call(QStringLiteral("HealthQuery"), configPath);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("HealthQuery"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

bool DBusClient::jobCancel(const QString &jobId)
{
    QDBusReply<void> reply = m_interface->call(QStringLiteral("JobCancel"), jobId);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("JobCancel"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

// --- Index read methods ---

QString DBusClient::indexStats(const QString &dbPath)
{
    QDBusReply<QString> reply = m_interface->call(QStringLiteral("IndexStats"), dbPath);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("IndexStats"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

QString DBusClient::indexListSnapshots(const QString &dbPath)
{
    QDBusReply<QString> reply = m_interface->call(
        QStringLiteral("IndexListSnapshots"), dbPath);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("IndexListSnapshots"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

QString DBusClient::indexListFiles(const QString &dbPath, qint64 snapshotId)
{
    QDBusReply<QString> reply = m_interface->call(
        QStringLiteral("IndexListFiles"), dbPath, snapshotId);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("IndexListFiles"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

QString DBusClient::indexSearch(const QString &dbPath, const QString &query, qint64 limit)
{
    QDBusReply<QString> reply = m_interface->call(
        QStringLiteral("IndexSearch"), dbPath, query, limit);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("IndexSearch"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

QString DBusClient::indexBackupHistory(const QString &dbPath, qint64 limit)
{
    QDBusReply<QString> reply = m_interface->call(
        QStringLiteral("IndexBackupHistory"), dbPath, limit);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("IndexBackupHistory"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

QString DBusClient::indexSnapshotPath(const QString &dbPath, qint64 snapshotId)
{
    QDBusReply<QString> reply = m_interface->call(
        QStringLiteral("IndexSnapshotPath"), dbPath, snapshotId);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("IndexSnapshotPath"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

// --- Private slots ---

void DBusClient::onJobProgress(const QString &jobId, const QString &stage,
                                int percent, const QString &message)
{
    Q_EMIT jobProgress(jobId, stage, percent, message);
}

void DBusClient::onJobLog(const QString &jobId, const QString &level,
                          const QString &message)
{
    Q_EMIT jobLog(jobId, level, message);
}

void DBusClient::onJobFinished(const QString &jobId, bool success,
                                const QString &summary)
{
    Q_EMIT jobFinished(jobId, success, summary);
}

// --- Private helpers ---

void DBusClient::callAsync(const QString &method, const QList<QVariant> &args,
                           const QString &operation)
{
    QDBusPendingCall pending = m_interface->asyncCallWithArgumentList(method, args);
    auto *watcher = new QDBusPendingCallWatcher(pending, this);

    connect(watcher, &QDBusPendingCallWatcher::finished,
            this, [this, operation](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            Q_EMIT errorOccurred(operation,
                                 mapDBusError(reply.error().name(),
                                              reply.error().message()));
        } else {
            Q_EMIT jobStarted(reply.value(), operation);
        }
        w->deleteLater();
    });
}

QString DBusClient::mapDBusError(const QString &errorName,
                                 const QString &errorMessage)
{
    if (errorName == QStringLiteral("org.freedesktop.DBus.Error.ServiceUnknown")) {
        return QStringLiteral("D-Bus helper service is not running. "
                              "Install and enable btrdasd-helper.");
    }
    if (errorName == QStringLiteral("org.freedesktop.DBus.Error.TimedOut")) {
        return QStringLiteral("D-Bus call timed out.");
    }
    if (errorName == QStringLiteral("org.freedesktop.PolicyKit1.Error.NotAuthorized")) {
        return QStringLiteral("Authorization denied by PolicyKit.");
    }
    return errorMessage;
}
