#pragma once

#include <QAbstractTableModel>
#include <QVector>

class DBusClient;

struct SearchResult {
    QString path;
    QString name;
    qint64 size = 0;
    qint64 mtime = 0;
    QString firstSnap;
    QString lastSnap;
};

class SearchModel : public QAbstractTableModel
{
    Q_OBJECT

public:
    enum Column { Path = 0, Name, Size, Modified, FirstSnapshot, LastSnapshot, ColumnCount };

    explicit SearchModel(DBusClient *client, const QString &dbPath, QObject *parent = nullptr);

    void executeSearch(const QString &query, qint64 limit);
    void clear();

    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    [[nodiscard]] QVariant headerData(int section, Qt::Orientation orientation,
                                       int role = Qt::DisplayRole) const override;

private:
    DBusClient *m_client;
    QString m_dbPath;
    QVector<SearchResult> m_results;
};
