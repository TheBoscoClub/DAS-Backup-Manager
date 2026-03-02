#include "snapshotmodel.h"
#include "dbusclient.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>

SnapshotModel::SnapshotModel(DBusClient *client, const QString &dbPath, QObject *parent)
    : QAbstractItemModel(parent)
    , m_client(client)
    , m_dbPath(dbPath)
{
    connect(m_client, &DBusClient::indexListSnapshotsResult,
            this, &SnapshotModel::onSnapshotsReceived);
}

QString SnapshotModel::tsToDate(const QString &ts)
{
    // ts format: "20260221T0304" -> "2026-02-21"
    if (ts.length() < 8) return ts;
    return ts.left(4) + QLatin1Char('-') + ts.mid(4, 2) + QLatin1Char('-') + ts.mid(6, 2);
}

void SnapshotModel::reload()
{
    m_client->indexListSnapshotsAsync(m_dbPath);
}

void SnapshotModel::onSnapshotsReceived(const QString &json)
{
    beginResetModel();
    m_snapshots.clear();
    m_groups.clear();

    if (!json.isEmpty()) {
        const QJsonArray arr = QJsonDocument::fromJson(json.toUtf8()).array();
        for (const QJsonValue &v : arr) {
            const QJsonObject obj = v.toObject();
            m_snapshots.append({
                .id = obj.value(QLatin1String("id")).toInteger(),
                .name = obj.value(QLatin1String("name")).toString(),
                .ts = obj.value(QLatin1String("ts")).toString(),
                .source = obj.value(QLatin1String("source")).toString(),
                .path = obj.value(QLatin1String("path")).toString(),
                .indexedAt = obj.value(QLatin1String("indexed_at")).toInteger(),
            });
        }
    }

    for (int i = 0; i < m_snapshots.size(); ++i) {
        QString date = tsToDate(m_snapshots[i].ts);
        if (m_groups.isEmpty() || m_groups.last().date != date) {
            m_groups.append({.date = date, .snapIndices = {}});
        }
        m_groups.last().snapIndices.append(i);
    }
    endResetModel();
}

QModelIndex SnapshotModel::index(int row, int column, const QModelIndex &parent) const
{
    if (!hasIndex(row, column, parent))
        return {};

    if (!parent.isValid()) {
        // Top-level: date group. internalId 0 = top-level.
        return createIndex(row, column, quintptr(0));
    }

    // Child: snapshot. Encode parent group index in internal id.
    int groupIdx = parent.row();
    return createIndex(row, column, quintptr(groupIdx + 1));
}

QModelIndex SnapshotModel::parent(const QModelIndex &index) const
{
    if (!index.isValid())
        return {};

    quintptr id = index.internalId();
    if (id == 0) {
        return {};
    }

    int groupIdx = static_cast<int>(id) - 1;
    return createIndex(groupIdx, 0, quintptr(0));
}

int SnapshotModel::rowCount(const QModelIndex &parent) const
{
    if (!parent.isValid()) {
        return m_groups.size();
    }

    if (parent.internalId() == 0 && parent.row() < m_groups.size()) {
        return m_groups[parent.row()].snapIndices.size();
    }

    return 0;
}

int SnapshotModel::columnCount(const QModelIndex & /*parent*/) const
{
    return 1;
}

QVariant SnapshotModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid())
        return {};

    quintptr id = index.internalId();

    if (id == 0) {
        if (index.row() >= m_groups.size()) return {};
        const auto &group = m_groups[index.row()];

        switch (role) {
        case Qt::DisplayRole:
            return group.date;
        case IsDateGroupRole:
            return true;
        default:
            return {};
        }
    }

    int groupIdx = static_cast<int>(id) - 1;
    if (groupIdx >= m_groups.size()) return {};
    const auto &group = m_groups[groupIdx];
    if (index.row() >= group.snapIndices.size()) return {};

    const auto &snap = m_snapshots[group.snapIndices[index.row()]];

    switch (role) {
    case Qt::DisplayRole:
        return QString(snap.source + QLatin1Char('/') + snap.name + QLatin1Char('.') + snap.ts);
    case SnapshotIdRole:
        return snap.id;
    case SnapshotPathRole:
        return snap.path;
    case SnapshotSourceRole:
        return snap.source;
    case IsDateGroupRole:
        return false;
    default:
        return {};
    }
}
