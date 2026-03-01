#include "searchmodel.h"
#include "dbusclient.h"
#include "filemodel.h"

#include <QDateTime>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>

SearchModel::SearchModel(DBusClient *client, const QString &dbPath, QObject *parent)
    : QAbstractTableModel(parent)
    , m_client(client)
    , m_dbPath(dbPath)
{
}

void SearchModel::executeSearch(const QString &query, qint64 limit)
{
    beginResetModel();
    m_results.clear();

    const QString json = m_client->indexSearch(m_dbPath, query, limit);
    if (!json.isEmpty()) {
        const QJsonArray arr = QJsonDocument::fromJson(json.toUtf8()).array();
        for (const QJsonValue &v : arr) {
            const QJsonObject obj = v.toObject();
            m_results.append({
                .path = obj.value(QLatin1String("path")).toString(),
                .name = obj.value(QLatin1String("name")).toString(),
                .size = obj.value(QLatin1String("size")).toInteger(),
                .mtime = obj.value(QLatin1String("mtime")).toInteger(),
                .firstSnap = obj.value(QLatin1String("first_snap")).toString(),
                .lastSnap = obj.value(QLatin1String("last_snap")).toString(),
            });
        }
    }
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
