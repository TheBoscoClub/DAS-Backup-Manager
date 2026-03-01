#include "backuphistory.h"
#include "database.h"
#include "dbusclient.h"
#include "filemodel.h"

#include <KLocalizedString>

#include <QAbstractTableModel>
#include <QDateTime>
#include <QHeaderView>
#include <QIcon>
#include <QLabel>
#include <QSortFilterProxyModel>
#include <QTableView>
#include <QVBoxLayout>
#include <QVector>

// ---------------------------------------------------------------------------
// BackupHistoryModel — private table model backed by Database::getBackupHistory
// ---------------------------------------------------------------------------

class BackupHistoryModel : public QAbstractTableModel
{
    Q_OBJECT

public:
    enum Column {
        Timestamp = 0,
        Mode,
        Duration,
        Status,
        SnapshotsCreated,
        BytesSent,
        Errors,
        ColumnCount
    };

    explicit BackupHistoryModel(Database *database, QObject *parent = nullptr)
        : QAbstractTableModel(parent)
        , m_database(database)
    {
    }

    void reload()
    {
        beginResetModel();
        m_runs = m_database->getBackupHistory(50);
        endResetModel();
    }

    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override
    {
        return parent.isValid() ? 0 : m_runs.size();
    }

    [[nodiscard]] int columnCount(const QModelIndex & /*parent*/ = {}) const override
    {
        return ColumnCount;
    }

    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override
    {
        if (!index.isValid() || index.row() >= m_runs.size())
            return {};

        const BackupRunInfo &run = m_runs[index.row()];

        if (role == Qt::DecorationRole && index.column() == Status) {
            return run.success
                ? QIcon::fromTheme(QStringLiteral("dialog-ok-apply"))
                : QIcon::fromTheme(QStringLiteral("dialog-error"));
        }

        if (role == Qt::TextAlignmentRole) {
            switch (index.column()) {
            case SnapshotsCreated:
            case BytesSent:
            case Errors:
                return static_cast<int>(Qt::AlignRight | Qt::AlignVCenter);
            default:
                return static_cast<int>(Qt::AlignLeft | Qt::AlignVCenter);
            }
        }

        if (role != Qt::DisplayRole)
            return {};

        switch (index.column()) {
        case Timestamp:
            return QDateTime::fromSecsSinceEpoch(run.timestamp)
                .toString(QStringLiteral("yyyy-MM-dd hh:mm:ss"));

        case Mode:
            return run.mode;

        case Duration:
            return formatDuration(run.durationSecs);

        case Status:
            return run.success ? i18n("Success") : i18n("Failed");

        case SnapshotsCreated:
            return run.snapsCreated;

        case BytesSent:
            return FileModel::formatSize(run.bytesSent);

        case Errors:
            return run.errors.size();

        default:
            return {};
        }
    }

    [[nodiscard]] QVariant headerData(int section, Qt::Orientation orientation,
                                      int role = Qt::DisplayRole) const override
    {
        if (orientation != Qt::Horizontal || role != Qt::DisplayRole)
            return {};

        switch (section) {
        case Timestamp:        return i18n("Timestamp");
        case Mode:             return i18n("Mode");
        case Duration:         return i18n("Duration");
        case Status:           return i18n("Status");
        case SnapshotsCreated: return i18n("Snapshots");
        case BytesSent:        return i18n("Bytes Sent");
        case Errors:           return i18n("Errors");
        default:               return {};
        }
    }

private:
    static QString formatDuration(qint64 secs)
    {
        if (secs < 0)
            return QStringLiteral("—");

        const qint64 hours = secs / 3600;
        const qint64 minutes = (secs % 3600) / 60;
        const qint64 seconds = secs % 60;

        if (hours > 0)
            return QStringLiteral("%1h %2m").arg(hours).arg(minutes);
        if (minutes > 0)
            return QStringLiteral("%1m %2s").arg(minutes).arg(seconds);
        return QStringLiteral("%1s").arg(seconds);
    }

    Database *m_database;
    QVector<BackupRunInfo> m_runs;
};

// BackupHistoryModel uses Q_OBJECT — include the moc output inline since the
// class is defined in a .cpp file (not exposed via a header).
#include "backuphistory.moc"

// ---------------------------------------------------------------------------
// BackupHistoryView
// ---------------------------------------------------------------------------

BackupHistoryView::BackupHistoryView(Database *db, DBusClient *client, QWidget *parent)
    : QWidget(parent)
    , m_database(db)
    , m_client(client)
{
    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(8, 8, 8, 8);
    layout->setSpacing(6);

    auto *title = new QLabel(i18n("Backup History"), this);
    QFont titleFont = title->font();
    titleFont.setPointSize(titleFont.pointSize() + 2);
    titleFont.setBold(true);
    title->setFont(titleFont);
    layout->addWidget(title);

    m_model = new BackupHistoryModel(m_database, this);
    m_model->reload();

    m_proxy = new QSortFilterProxyModel(this);
    m_proxy->setSourceModel(m_model);
    m_proxy->setSortRole(Qt::DisplayRole);

    m_view = new QTableView(this);
    m_view->setModel(m_proxy);
    m_view->setSortingEnabled(true);
    m_view->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_view->setSelectionMode(QAbstractItemView::SingleSelection);
    m_view->setAlternatingRowColors(true);
    m_view->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_view->setShowGrid(false);
    m_view->verticalHeader()->setVisible(false);

    QHeaderView *hh = m_view->horizontalHeader();
    hh->setStretchLastSection(true);
    hh->setSectionsClickable(true);
    hh->setSortIndicatorShown(true);

    // Reasonable initial column widths
    m_view->setColumnWidth(BackupHistoryModel::Timestamp,        160);
    m_view->setColumnWidth(BackupHistoryModel::Mode,              80);
    m_view->setColumnWidth(BackupHistoryModel::Duration,          80);
    m_view->setColumnWidth(BackupHistoryModel::Status,            80);
    m_view->setColumnWidth(BackupHistoryModel::SnapshotsCreated,  80);
    m_view->setColumnWidth(BackupHistoryModel::BytesSent,         90);
    // Errors column stretches to fill remaining space

    // Sort by timestamp descending by default (most recent first)
    m_view->sortByColumn(BackupHistoryModel::Timestamp, Qt::DescendingOrder);

    layout->addWidget(m_view, 1);

    // Auto-refresh when a backup job completes
    connect(m_client, &DBusClient::jobFinished,
            this, &BackupHistoryView::refresh);
}

void BackupHistoryView::refresh()
{
    m_model->reload();
}
