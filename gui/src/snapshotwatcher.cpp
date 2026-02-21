#include "snapshotwatcher.h"
#include "indexrunner.h"

#include <QDir>

SnapshotWatcher::SnapshotWatcher(IndexRunner *runner, QObject *parent)
    : QObject(parent)
    , m_runner(runner)
{
    m_watcher = new QFileSystemWatcher(this);
    m_delayTimer = new QTimer(this);
    m_delayTimer->setSingleShot(true);
    m_delayTimer->setInterval(IndexDelayMs);

    connect(m_watcher, &QFileSystemWatcher::directoryChanged,
            this, &SnapshotWatcher::onDirectoryChanged);
    connect(m_delayTimer, &QTimer::timeout,
            this, &SnapshotWatcher::triggerIndex);
}

void SnapshotWatcher::setWatchPath(const QString &path)
{
    if (!m_watchPath.isEmpty()) {
        m_watcher->removePath(m_watchPath);
    }
    m_watchPath = path;
    if (m_enabled && !path.isEmpty() && QDir(path).exists()) {
        m_watcher->addPath(path);
    }
}

void SnapshotWatcher::setDbPath(const QString &dbPath)
{
    m_dbPath = dbPath;
}

void SnapshotWatcher::setEnabled(bool enabled)
{
    m_enabled = enabled;
    if (enabled && !m_watchPath.isEmpty() && QDir(m_watchPath).exists()) {
        if (!m_watcher->directories().contains(m_watchPath)) {
            m_watcher->addPath(m_watchPath);
        }
    } else {
        if (!m_watchPath.isEmpty()) {
            m_watcher->removePath(m_watchPath);
        }
        m_delayTimer->stop();
    }
}

bool SnapshotWatcher::isEnabled() const
{
    return m_enabled;
}

void SnapshotWatcher::onDirectoryChanged(const QString &path)
{
    Q_EMIT newSnapshotDetected(path);
    // Reset delay timer — gives btrbk time to finish writing
    m_delayTimer->start();
}

void SnapshotWatcher::triggerIndex()
{
    if (m_runner && !m_runner->isRunning()) {
        m_runner->run(m_watchPath, m_dbPath);
        Q_EMIT indexingTriggered();
    }
}
