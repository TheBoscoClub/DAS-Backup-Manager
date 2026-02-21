#include "searchmodel.h"
#include "filemodel.h"

#include <QDateTime>

SearchModel::SearchModel(Database *database, QObject *parent)
    : QAbstractTableModel(parent)
    , m_database(database)
{
}

void SearchModel::executeSearch(const QString &query, qint64 limit)
{
    beginResetModel();
    m_results = m_database->search(query, limit);
    endResetModel();
}

void SearchModel::clear()
{
    beginResetModel();
    m_results.clear();
    endResetModel();
}

int SearchModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_results.size();
}

int SearchModel::columnCount(const QModelIndex & /*parent*/) const
{
    return ColumnCount;
}

QVariant SearchModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() >= m_results.size())
        return {};
    if (role != Qt::DisplayRole)
        return {};

    const auto &r = m_results[index.row()];
    switch (index.column()) {
    case Path:          return r.path;
    case Name:          return r.name;
    case Size:          return FileModel::formatSize(r.size);
    case Modified:      return QDateTime::fromSecsSinceEpoch(r.mtime).toString(QStringLiteral("yyyy-MM-dd hh:mm"));
    case FirstSnapshot: return r.firstSnap;
    case LastSnapshot:  return r.lastSnap;
    default:            return {};
    }
}

QVariant SearchModel::headerData(int section, Qt::Orientation orientation, int role) const
{
    if (orientation != Qt::Horizontal || role != Qt::DisplayRole)
        return {};

    switch (section) {
    case Path:          return QStringLiteral("Path");
    case Name:          return QStringLiteral("Name");
    case Size:          return QStringLiteral("Size");
    case Modified:      return QStringLiteral("Modified");
    case FirstSnapshot: return QStringLiteral("First Snapshot");
    case LastSnapshot:  return QStringLiteral("Last Snapshot");
    default:            return {};
    }
}
