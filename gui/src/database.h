#pragma once

#include <QDateTime>
#include <QSqlDatabase>
#include <QString>
#include <QStringList>
#include <QVector>

struct SnapshotInfo {
    qint64 id = 0;
    QString name;
    QString ts;
    QString source;
    QString path;
    qint64 indexedAt = 0;
};

struct FileInfo {
    qint64 id = 0;
    QString path;
    QString name;
    qint64 size = 0;
    qint64 mtime = 0;
    int type = 0; // 0=regular, 1=dir, 2=symlink, 3=other
};

struct SearchResult {
    QString path;
    QString name;
    qint64 size = 0;
    qint64 mtime = 0;
    QString firstSnap;
    QString lastSnap;
};

struct DbStats {
    qint64 snapshotCount = 0;
    qint64 fileCount = 0;
    qint64 spanCount = 0;
    qint64 dbSizeBytes = 0;
};

struct BackupRunInfo {
    qint64 id = 0;
    qint64 timestamp = 0;
    QString mode;
    bool success = false;
    qint64 durationSecs = 0;
    qint64 snapsCreated = 0;
    qint64 snapsSent = 0;
    qint64 bytesSent = 0;
    QStringList errors;
};

struct TargetUsageInfo {
    qint64 id = 0;
    qint64 timestamp = 0;
    QString label;
    quint64 totalBytes = 0;
    quint64 usedBytes = 0;
    quint64 snapshotCount = 0;
};

class Database
{
public:
    Database();
    ~Database();

    bool open(const QString &path);
    void close();
    [[nodiscard]] bool isOpen() const;

    [[nodiscard]] QVector<SnapshotInfo> listSnapshots() const;
    [[nodiscard]] QVector<FileInfo> filesInSnapshot(qint64 snapshotId) const;
    [[nodiscard]] QVector<SearchResult> search(const QString &query, qint64 limit) const;
    [[nodiscard]] DbStats stats() const;
    [[nodiscard]] QString snapshotPathById(qint64 snapshotId) const;
    [[nodiscard]] QVector<BackupRunInfo> getBackupHistory(int limit = 20) const;
    [[nodiscard]] QVector<TargetUsageInfo> getTargetUsageHistory(
        const QString &label, int days = 30) const;

private:
    QSqlDatabase m_db;
    QString m_connectionName;
};
