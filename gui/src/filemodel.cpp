#include "filemodel.h"
#include "dbusclient.h"

#include <QDateTime>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>

FileModel::FileModel(DBusClient *client, const QString &dbPath, QObject *parent)
    : QAbstractTableModel(parent)
    , m_client(client)
    , m_dbPath(dbPath)
{
}

void FileModel::loadSnapshot(qint64 snapshotId)
{
    beginResetModel();
    m_files.clear();

    const QString json = m_client->indexListFiles(m_dbPath, snapshotId);
    if (!json.isEmpty()) {
        const QJsonArray arr = QJsonDocument::fromJson(json.toUtf8()).array();
        for (const QJsonValue &v : arr) {
            const QJsonObject obj = v.toObject();
            m_files.append({
                .id = obj.value(QLatin1String("id")).toInteger(),
                .path = obj.value(QLatin1String("path")).toString(),
                .name = obj.value(QLatin1String("name")).toString(),
                .size = obj.value(QLatin1String("size")).toInteger(),
                .mtime = obj.value(QLatin1String("mtime")).toInteger(),
                .type = obj.value(QLatin1String("type")).toInt(),
            });
        }
    }
    endResetModel();
}

void FileModel::clear()
{
    beginResetModel();
    m_files.clear();
    endResetModel();
}

int FileModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_files.size();
}

int FileModel::columnCount(const QModelIndex & /*parent*/) const
{
    return ColumnCount;
}

QVariant FileModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() >= m_files.size())
        return {};

    const auto &file = m_files[index.row()];

    if (role == FileIdRole) return file.id;
    if (role == FilePathRole) return file.path;

    if (role != Qt::DisplayRole)
        return {};

    switch (index.column()) {
    case Name:     return file.name;
    case Path:     return file.path;
    case Size:     return formatSize(file.size);
    case Modified: return QDateTime::fromSecsSinceEpoch(file.mtime).toString(QStringLiteral("yyyy-MM-dd hh:mm"));
    case Type: {
        switch (file.type) {
        case 0: return QStringLiteral("File");
        case 1: return QStringLiteral("Directory");
        case 2: return QStringLiteral("Symlink");
        default: return QStringLiteral("Other");
        }
    }
    default: return {};
    }
}

QVariant FileModel::headerData(int section, Qt::Orientation orientation, int role) const
{
    if (orientation != Qt::Horizontal || role != Qt::DisplayRole)
        return {};

    switch (section) {
    case Name:     return QStringLiteral("Name");
    case Path:     return QStringLiteral("Path");
    case Size:     return QStringLiteral("Size");
    case Modified: return QStringLiteral("Modified");
    case Type:     return QStringLiteral("Type");
    default:       return {};
    }
}

QString FileModel::formatSize(qint64 bytes)
{
    if (bytes < 1024) return QString::number(bytes) + QStringLiteral(" B");
    if (bytes < 1024 * 1024) return QString::number(bytes / 1024.0, 'f', 1) + QStringLiteral(" KiB");
    if (bytes < 1024LL * 1024 * 1024) return QString::number(bytes / (1024.0 * 1024.0), 'f', 1) + QStringLiteral(" MiB");
    return QString::number(bytes / (1024.0 * 1024.0 * 1024.0), 'f', 1) + QStringLiteral(" GiB");
}
