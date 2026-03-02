#pragma once

#include <QAbstractTableModel>
#include <QVector>

class DBusClient;

struct FileInfo {
    qint64 id = 0;
    QString path;
    QString name;
    qint64 size = 0;
    qint64 mtime = 0;
    int type = 0; // 0=regular, 1=dir, 2=symlink, 3=other
};

class FileModel : public QAbstractTableModel
{
    Q_OBJECT

public:
    enum Column { Name = 0, Path, Size, Modified, Type, ColumnCount };
    enum Roles { FileIdRole = Qt::UserRole + 1, FilePathRole };

    explicit FileModel(DBusClient *client, const QString &dbPath, QObject *parent = nullptr);

    void loadSnapshot(qint64 snapshotId);
    void loadMore();
    void clear();

    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    [[nodiscard]] QVariant headerData(int section, Qt::Orientation orientation,
                                       int role = Qt::DisplayRole) const override;

    [[nodiscard]] qint64 totalFiles() const { return m_totalFiles; }
    [[nodiscard]] bool hasMore() const { return m_files.size() < m_totalFiles; }

    static QString formatSize(qint64 bytes);

private:
    static constexpr qint64 PageSize = 10000;

    DBusClient *m_client;
    QString m_dbPath;
    QVector<FileInfo> m_files;
    qint64 m_currentSnapshotId = -1;
    qint64 m_totalFiles = 0;
};
