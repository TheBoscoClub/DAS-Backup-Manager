#pragma once

#include <QObject>
#include <QFileSystemWatcher>
#include <QTimer>

class IndexRunner;

class SnapshotWatcher : public QObject
{
    Q_OBJECT

public:
    explicit SnapshotWatcher(IndexRunner *runner, QObject *parent = nullptr);

    void setWatchPath(const QString &path);
    void setDbPath(const QString &dbPath);
    void setEnabled(bool enabled);
    [[nodiscard]] bool isEnabled() const;

Q_SIGNALS:
    void newSnapshotDetected(const QString &path);
    void indexingTriggered();

private Q_SLOTS:
    void onDirectoryChanged(const QString &path);
    void triggerIndex();

private:
    QFileSystemWatcher *m_watcher = nullptr;
    IndexRunner *m_runner = nullptr;
    QTimer *m_delayTimer = nullptr;
    QString m_watchPath;
    QString m_dbPath;
    bool m_enabled = false;

    static constexpr int IndexDelayMs = 30000;
};
