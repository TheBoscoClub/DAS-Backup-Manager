#pragma once

#include <QAbstractTableModel>
#include <QVector>
#include "database.h"

class SearchModel : public QAbstractTableModel
{
    Q_OBJECT

public:
    enum Column { Path = 0, Name, Size, Modified, FirstSnapshot, LastSnapshot, ColumnCount };

    explicit SearchModel(Database *database, QObject *parent = nullptr);

    void executeSearch(const QString &query, qint64 limit);
    void clear();

    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    [[nodiscard]] QVariant headerData(int section, Qt::Orientation orientation,
                                       int role = Qt::DisplayRole) const override;

private:
    Database *m_database;
    QVector<SearchResult> m_results;
};
