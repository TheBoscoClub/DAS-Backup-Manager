#include "dbusclient.h"

#include <QDBusConnection>
#include <QDBusInterface>
#include <QDBusMessage>
#include <QDBusPendingCall>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusReply>
#include <QTimer>

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
    // QDBusInterface::isValid() only verifies the proxy object was constructed
    // — it does NOT verify the remote service can be activated. To know whether
    // org.dasbackup.Helper1 is actually reachable, we have to round-trip a real
    // method call. Use Peer.Ping (a standard no-op on every D-Bus object) which
    // triggers normal activation and returns quickly when the helper is healthy.
    //
    // Doing this ONCE here lets every other call site assume m_available is the
    // truth. Without it, each of the GUI's startup views (status bar, snapshot
    // list, backup history, health, config) discovers the activation failure
    // independently and fires its own modal dialog — see bd DAS-Backup-Manager-mw0
    // for the cascade we are eliminating.
    QDBusMessage ping = QDBusMessage::createMethodCall(
        ServiceName, ObjectPath,
        QStringLiteral("org.freedesktop.DBus.Peer"),
        QStringLiteral("Ping"));
    QDBusMessage reply = QDBusConnection::systemBus().call(ping);
    m_available = (reply.type() != QDBusMessage::ErrorMessage);

    if (!m_available) {
        m_unavailableReason = mapDBusError(reply.errorName(), reply.errorMessage());
        // Defer signal emission until after the event loop starts so consumers
        // that connect after DBusClient construction (e.g. MainWindow's slot
        // for helperUnavailable) can still receive it.
        QTimer::singleShot(0, this, [this] {
            Q_EMIT helperUnavailable(m_unavailableReason);
        });
        return;
    }

    // Connect D-Bus signals to local slots — only if the helper actually
    // exists; binding signals on an unreachable service is pointless and
    // causes spurious noise in journals on some D-Bus implementations.
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

QString DBusClient::unavailableReason() const
{
    return m_unavailableReason;
}

// --- Async job-returning methods ---

void DBusClient::backupRun(const QString &mode,
                           const QStringList &sources, const QStringList &targets,
                           bool dryRun)
{
    callAsync(QStringLiteral("BackupRun"),
              {mode, QVariant::fromValue(sources),
               QVariant::fromValue(targets), dryRun},
              QStringLiteral("BackupRun"));
}

void DBusClient::backupSnapshot(const QStringList &sources)
{
    callAsync(QStringLiteral("BackupSnapshot"),
              {QVariant::fromValue(sources)},
              QStringLiteral("BackupSnapshot"));
}

void DBusClient::backupSend(const QStringList &targets)
{
    callAsync(QStringLiteral("BackupSend"),
              {QVariant::fromValue(targets)},
              QStringLiteral("BackupSend"));
}

void DBusClient::backupBootArchive()
{
    callAsync(QStringLiteral("BackupBootArchive"),
              {},
              QStringLiteral("BackupBootArchive"));
}

void DBusClient::indexWalk(const QString &targetPath,
                           const QString &dbPath)
{
    callAsync(QStringLiteral("IndexWalk"),
              {targetPath, dbPath},
              QStringLiteral("IndexWalk"));
}

void DBusClient::restoreFiles(const QString &snapshot,
                              const QString &dest, const QStringList &files)
{
    callAsync(QStringLiteral("RestoreFiles"),
              {snapshot, dest, QVariant::fromValue(files)},
              QStringLiteral("RestoreFiles"));
}

void DBusClient::restoreSnapshot(const QString &snapshot,
                                 const QString &dest)
{
    callAsync(QStringLiteral("RestoreSnapshot"),
              {snapshot, dest},
              QStringLiteral("RestoreSnapshot"));
}

// --- Synchronous methods ---

QString DBusClient::configGet()
{
    if (!m_available) return {};
    QDBusReply<QString> reply = m_interface->call(QStringLiteral("ConfigGet"));
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ConfigGet"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

bool DBusClient::configSet(const QString &tomlContent)
{
    if (!m_available) return false;
    QDBusReply<void> reply = m_interface->call(QStringLiteral("ConfigSet"),
                                               tomlContent);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ConfigSet"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

QString DBusClient::scheduleGet()
{
    if (!m_available) return {};
    QDBusReply<QString> reply = m_interface->call(QStringLiteral("ScheduleGet"));
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ScheduleGet"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

bool DBusClient::scheduleSet(const QString &incremental,
                             const QString &full, quint32 delay)
{
    if (!m_available) return false;
    QDBusReply<void> reply = m_interface->call(QStringLiteral("ScheduleSet"),
                                               incremental, full, delay);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ScheduleSet"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

bool DBusClient::scheduleEnable(bool enabled)
{
    if (!m_available) return false;
    QDBusReply<void> reply = m_interface->call(QStringLiteral("ScheduleEnable"),
                                               enabled);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("ScheduleEnable"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

bool DBusClient::subvolAdd(const QString &source,
                           const QString &name)
{
    if (!m_available) return false;
    QDBusReply<void> reply = m_interface->call(QStringLiteral("SubvolAdd"),
                                               source, name);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("SubvolAdd"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

bool DBusClient::subvolRemove(const QString &source,
                              const QString &name)
{
    if (!m_available) return false;
    QDBusReply<void> reply = m_interface->call(QStringLiteral("SubvolRemove"),
                                               source, name);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("SubvolRemove"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

bool DBusClient::subvolSetManual(const QString &source,
                                 const QString &name, bool manual)
{
    if (!m_available) return false;
    QDBusReply<void> reply = m_interface->call(QStringLiteral("SubvolSetManual"),
                                               source, name, manual);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("SubvolSetManual"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

QString DBusClient::healthQuery()
{
    if (!m_available) return {};
    QDBusReply<QString> reply = m_interface->call(QStringLiteral("HealthQuery"));
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("HealthQuery"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

bool DBusClient::jobCancel(const QString &jobId)
{
    if (!m_available) return false;
    QDBusReply<void> reply = m_interface->call(QStringLiteral("JobCancel"), jobId);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("JobCancel"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return false;
    }
    return true;
}

// --- Async non-blocking methods ---

void DBusClient::healthQueryAsync()
{
    if (!m_available) {
        Q_EMIT healthQueryResult({});
        return;
    }
    QDBusPendingCall pending = m_interface->asyncCall(
        QStringLiteral("HealthQuery"));
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished,
            this, [this](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            Q_EMIT errorOccurred(QStringLiteral("HealthQuery"),
                                 mapDBusError(reply.error().name(),
                                              reply.error().message()));
            Q_EMIT healthQueryResult({});
        } else {
            Q_EMIT healthQueryResult(reply.value());
        }
        w->deleteLater();
    });
}

void DBusClient::scheduleGetAsync()
{
    if (!m_available) {
        Q_EMIT scheduleGetResult({});
        return;
    }
    QDBusPendingCall pending = m_interface->asyncCall(
        QStringLiteral("ScheduleGet"));
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished,
            this, [this](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            Q_EMIT errorOccurred(QStringLiteral("ScheduleGet"),
                                 mapDBusError(reply.error().name(),
                                              reply.error().message()));
            Q_EMIT scheduleGetResult({});
        } else {
            Q_EMIT scheduleGetResult(reply.value());
        }
        w->deleteLater();
    });
}

void DBusClient::indexStatsAsync(const QString &dbPath)
{
    if (!m_available) {
        Q_EMIT indexStatsResult({});
        return;
    }
    QDBusPendingCall pending = m_interface->asyncCall(
        QStringLiteral("IndexStats"), dbPath);
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished,
            this, [this](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            Q_EMIT errorOccurred(QStringLiteral("IndexStats"),
                                 mapDBusError(reply.error().name(),
                                              reply.error().message()));
            Q_EMIT indexStatsResult({});
        } else {
            Q_EMIT indexStatsResult(reply.value());
        }
        w->deleteLater();
    });
}

void DBusClient::indexListSnapshotsAsync(const QString &dbPath)
{
    if (!m_available) {
        Q_EMIT indexListSnapshotsResult({});
        return;
    }
    QDBusPendingCall pending = m_interface->asyncCall(
        QStringLiteral("IndexListSnapshots"), dbPath);
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished,
            this, [this](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            Q_EMIT errorOccurred(QStringLiteral("IndexListSnapshots"),
                                 mapDBusError(reply.error().name(),
                                              reply.error().message()));
            Q_EMIT indexListSnapshotsResult({});
        } else {
            Q_EMIT indexListSnapshotsResult(reply.value());
        }
        w->deleteLater();
    });
}

// --- Index read methods ---

QString DBusClient::indexStats(const QString &dbPath)
{
    if (!m_available) return {};
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
    if (!m_available) return {};
    QDBusReply<QString> reply = m_interface->call(
        QStringLiteral("IndexListSnapshots"), dbPath);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("IndexListSnapshots"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

QString DBusClient::indexListFiles(const QString &dbPath, qint64 snapshotId,
                                   qint64 limit, qint64 offset)
{
    if (!m_available) return {};
    QDBusReply<QString> reply = m_interface->call(
        QStringLiteral("IndexListFiles"), dbPath, snapshotId, limit, offset);
    if (!reply.isValid()) {
        Q_EMIT errorOccurred(QStringLiteral("IndexListFiles"),
                             mapDBusError(reply.error().name(), reply.error().message()));
        return {};
    }
    return reply.value();
}

QString DBusClient::indexSearch(const QString &dbPath, const QString &query, qint64 limit)
{
    if (!m_available) return {};
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
    if (!m_available) return {};
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
    if (!m_available) return {};
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
    // When the helper isn't reachable we silently no-op every job-starting
    // call. The single helperUnavailable signal emitted from the constructor
    // is the user's one notification; per-action errorOccurred firings here
    // would just rebuild the cascade we're trying to eliminate.
    if (!m_available) return;
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
