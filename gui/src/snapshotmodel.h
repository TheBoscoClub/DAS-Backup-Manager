#pragma once

#include <QAbstractItemModel>
#include <QVector>

class DBusClient;

struct SnapshotInfo {
    qint64 id = 0;
    QString name;
    QString ts;
    QString source;
    QString path;
    qint64 indexedAt = 0;
};

class SnapshotModel : public QAbstractItemModel
{
    Q_OBJECT

public:
    enum Roles {
        SnapshotIdRole = Qt::UserRole + 1,
        SnapshotPathRole,
        SnapshotSourceRole,
        IsDateGroupRole,
    };

    explicit SnapshotModel(DBusClient *client, const QString &dbPath, QObject *parent = nullptr);

    void reload();

    [[nodiscard]] QModelIndex index(int row, int column,
                                     const QModelIndex &parent = {}) const override;
    [[nodiscard]] QModelIndex parent(const QModelIndex &index) const override;
    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;

private:
    struct DateGroup {
        QString date;
        QVector<int> snapIndices;
    };

    DBusClient *m_client;
    QString m_dbPath;
    QVector<SnapshotInfo> m_snapshots;
    QVector<DateGroup> m_groups;

    static QString tsToDate(const QString &ts);
};
