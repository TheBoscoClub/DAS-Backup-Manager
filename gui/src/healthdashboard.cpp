#include "healthdashboard.h"
#include "database.h"
#include "dbusclient.h"
#include "filemodel.h"

#include <KLocalizedString>

#include <QColor>
#include <QFormLayout>
#include <QHeaderView>
#include <QIcon>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QStandardItem>
#include <QStandardItemModel>
#include <QTabWidget>
#include <QTableView>
#include <QVBoxLayout>

// ---------------------------------------------------------------------------
// Column indices for the drives table
// ---------------------------------------------------------------------------

namespace DrivesCol {
    enum {
        Device = 0,
        Label,
        Status,
        Total,
        Used,
        Free,
        Smart,
        Temp,
        PowerHours,
        Count
    };
}

// ---------------------------------------------------------------------------
// Column indices for the growth table
// ---------------------------------------------------------------------------

namespace GrowthCol {
    enum {
        Date = 0,
        Label,
        Used,
        Count
    };
}

// ---------------------------------------------------------------------------
// HealthDashboard
// ---------------------------------------------------------------------------

HealthDashboard::HealthDashboard(Database *db, DBusClient *client, QWidget *parent)
    : QWidget(parent)
    , m_database(db)
    , m_client(client)
    , m_configPath(QStringLiteral("/etc/das-backup/config.toml"))
{
    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(8, 8, 8, 8);
    layout->setSpacing(6);

    auto *title = new QLabel(i18n("Health Dashboard"), this);
    QFont titleFont = title->font();
    titleFont.setPointSize(titleFont.pointSize() + 2);
    titleFont.setBold(true);
    title->setFont(titleFont);
    layout->addWidget(title);

    m_tabs = new QTabWidget(this);
    layout->addWidget(m_tabs, 1);

    setupDrivesTab();
    setupGrowthTab();
    setupStatusTab();

    refresh();
}

// ---------------------------------------------------------------------------
// Tab setup
// ---------------------------------------------------------------------------

void HealthDashboard::setupDrivesTab()
{
    m_drivesView = new QTableView(this);
    m_drivesView->setToolTip(i18n("Physical drive status and SMART health information"));

    auto *model = new QStandardItemModel(0, DrivesCol::Count, m_drivesView);
    model->setHorizontalHeaderLabels({
        i18n("Device"),
        i18n("Label"),
        i18n("Status"),
        i18n("Total"),
        i18n("Used"),
        i18n("Free"),
        i18n("SMART"),
        i18n("Temp"),
        i18n("Power Hours"),
    });
    m_drivesView->setModel(model);

    m_drivesView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_drivesView->setSelectionMode(QAbstractItemView::SingleSelection);
    m_drivesView->setAlternatingRowColors(true);
    m_drivesView->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_drivesView->setShowGrid(false);
    m_drivesView->verticalHeader()->setVisible(false);

    QHeaderView *hh = m_drivesView->horizontalHeader();
    hh->setStretchLastSection(true);
    hh->setSectionResizeMode(QHeaderView::ResizeToContents);
    hh->setSectionResizeMode(DrivesCol::Device, QHeaderView::Interactive);
    hh->setSectionResizeMode(DrivesCol::Label,  QHeaderView::Interactive);

    m_tabs->addTab(m_drivesView, QIcon::fromTheme(QStringLiteral("drive-harddisk")),
                   i18n("Drives"));
}

void HealthDashboard::setupGrowthTab()
{
    m_growthWidget = new QWidget(this);
    auto *layout = new QVBoxLayout(m_growthWidget);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(4);

    auto *info = new QLabel(i18n("Growth data loaded from health query"), m_growthWidget);
    info->setToolTip(i18n("Storage usage over time for each backup target"));
    layout->addWidget(info);

    m_growthView = new QTableView(m_growthWidget);

    auto *model = new QStandardItemModel(0, GrowthCol::Count, m_growthView);
    model->setHorizontalHeaderLabels({
        i18n("Date"),
        i18n("Label"),
        i18n("Used"),
    });
    m_growthView->setModel(model);

    m_growthView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_growthView->setSelectionMode(QAbstractItemView::SingleSelection);
    m_growthView->setAlternatingRowColors(true);
    m_growthView->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_growthView->setShowGrid(false);
    m_growthView->verticalHeader()->setVisible(false);

    QHeaderView *hh = m_growthView->horizontalHeader();
    hh->setStretchLastSection(true);
    hh->setSectionResizeMode(QHeaderView::ResizeToContents);
    hh->setSectionResizeMode(GrowthCol::Date,  QHeaderView::Interactive);
    hh->setSectionResizeMode(GrowthCol::Label, QHeaderView::Interactive);

    layout->addWidget(m_growthView, 1);

    m_tabs->addTab(m_growthWidget, QIcon::fromTheme(QStringLiteral("office-chart-line")),
                   i18n("Growth"));
}

void HealthDashboard::setupStatusTab()
{
    auto *page = new QWidget(this);
    auto *form = new QFormLayout(page);
    form->setContentsMargins(12, 12, 12, 12);
    form->setSpacing(8);
    form->setLabelAlignment(Qt::AlignRight | Qt::AlignVCenter);

    m_btrbkLabel = new QLabel(page);
    m_btrbkLabel->setToolTip(i18n("Whether the btrbk binary is available on this system"));
    form->addRow(i18n("btrbk:"), m_btrbkLabel);

    m_timerLabel = new QLabel(page);
    m_timerLabel->setToolTip(i18n("Systemd timer status and next scheduled run"));
    form->addRow(i18n("Timer:"), m_timerLabel);

    m_lastBackupLabel = new QLabel(page);
    m_lastBackupLabel->setToolTip(i18n("Time elapsed since the last successful backup run"));
    form->addRow(i18n("Last Backup:"), m_lastBackupLabel);

    m_mountLabel = new QLabel(page);
    m_mountLabel->setToolTip(i18n("Number of backup target drives currently mounted"));
    form->addRow(i18n("Drives Mounted:"), m_mountLabel);

    m_tabs->addTab(page, QIcon::fromTheme(QStringLiteral("dialog-information")),
                   i18n("Status"));
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

void HealthDashboard::setActiveTab(int index)
{
    if (index >= 0 && index < m_tabs->count())
        m_tabs->setCurrentIndex(index);
}

void HealthDashboard::refresh()
{
    const QString json = m_client->healthQuery(m_configPath);

    if (json.isEmpty())
        return;

    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    if (doc.isNull() || !doc.isObject())
        return;

    updateDrives(json);
    updateGrowth(json);
    updateStatus(json);
}

// ---------------------------------------------------------------------------
// Private update helpers
// ---------------------------------------------------------------------------

void HealthDashboard::updateDrives(const QString &json)
{
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    const QJsonArray drives = doc.object().value(QLatin1String("drives")).toArray();

    auto *model = qobject_cast<QStandardItemModel *>(m_drivesView->model());
    if (!model)
        return;

    model->removeRows(0, model->rowCount());

    for (const QJsonValue &val : drives) {
        const QJsonObject drv = val.toObject();

        const QString device   = drv.value(QLatin1String("device")).toString();
        const QString label    = drv.value(QLatin1String("label")).toString();
        const bool    mounted  = drv.value(QLatin1String("mounted")).toBool();
        const qint64  total    = drv.value(QLatin1String("total_bytes")).toInteger();
        const qint64  used     = drv.value(QLatin1String("used_bytes")).toInteger();
        const qint64  free     = total - used;
        const QString smart    = drv.value(QLatin1String("smart_status")).toString();
        const int     tempC    = drv.value(QLatin1String("temperature_c")).toInt();
        const int     pwrHours = drv.value(QLatin1String("power_on_hours")).toInt();
        const int     errors   = drv.value(QLatin1String("errors")).toInt();

        QList<QStandardItem *> row;
        row.reserve(DrivesCol::Count);

        // Device
        auto *devItem = new QStandardItem(device);
        devItem->setToolTip(device);
        row.append(devItem);

        // Label
        row.append(new QStandardItem(label));

        // Status (mounted icon + text)
        auto *statusItem = new QStandardItem();
        if (mounted) {
            statusItem->setText(i18n("Mounted"));
            statusItem->setIcon(QIcon::fromTheme(QStringLiteral("drive-harddisk")));
        } else {
            statusItem->setText(i18n("Not mounted"));
            statusItem->setIcon(QIcon::fromTheme(QStringLiteral("drive-harddisk-symbolic")));
        }
        row.append(statusItem);

        // Total / Used / Free
        row.append(new QStandardItem(FileModel::formatSize(total)));
        row.append(new QStandardItem(FileModel::formatSize(used)));
        row.append(new QStandardItem(FileModel::formatSize(free < 0 ? 0 : free)));

        // SMART
        auto *smartItem = new QStandardItem(smart);
        if (smart == QLatin1String("PASSED")) {
            smartItem->setForeground(QColor(0x22, 0x8B, 0x22));  // forest green
        } else if (!smart.isEmpty() && smart != QLatin1String("UNKNOWN")) {
            smartItem->setForeground(QColor(0xCC, 0x00, 0x00));  // red
        }
        if (errors > 0) {
            smartItem->setText(QStringLiteral("%1 (%2 err)").arg(smart).arg(errors));
        }
        row.append(smartItem);

        // Temperature
        const QString tempStr = tempC > 0
            ? QStringLiteral("%1 °C").arg(tempC)
            : QStringLiteral("—");
        row.append(new QStandardItem(tempStr));

        // Power-on hours
        const QString pwrStr = pwrHours > 0
            ? QStringLiteral("%1 h").arg(pwrHours)
            : QStringLiteral("—");
        row.append(new QStandardItem(pwrStr));

        for (QStandardItem *item : row)
            item->setEditable(false);

        model->appendRow(row);
    }
}

void HealthDashboard::updateGrowth(const QString &json)
{
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    const QJsonArray growthArr = doc.object().value(QLatin1String("growth")).toArray();

    auto *model = qobject_cast<QStandardItemModel *>(m_growthView->model());
    if (!model)
        return;

    model->removeRows(0, model->rowCount());

    for (const QJsonValue &gval : growthArr) {
        const QJsonObject gobj   = gval.toObject();
        const QString     glabel = gobj.value(QLatin1String("label")).toString();
        const QJsonArray  entries = gobj.value(QLatin1String("entries")).toArray();

        for (const QJsonValue &eval : entries) {
            const QJsonObject entry = eval.toObject();
            const QString     date  = entry.value(QLatin1String("date")).toString();
            const qint64      used  = entry.value(QLatin1String("used_bytes")).toInteger();

            QList<QStandardItem *> row;
            row.reserve(GrowthCol::Count);

            row.append(new QStandardItem(date));
            row.append(new QStandardItem(glabel));
            row.append(new QStandardItem(FileModel::formatSize(used)));

            for (QStandardItem *item : row)
                item->setEditable(false);

            model->appendRow(row);
        }
    }
}

void HealthDashboard::updateStatus(const QString &json)
{
    const QJsonDocument doc      = QJsonDocument::fromJson(json.toUtf8());
    const QJsonObject   root     = doc.object();
    const QJsonObject   services = root.value(QLatin1String("services")).toObject();
    const QJsonArray    drives   = root.value(QLatin1String("drives")).toArray();

    // btrbk availability
    const bool btrbkAvail = services.value(QLatin1String("btrbk_available")).toBool();
    if (btrbkAvail) {
        m_btrbkLabel->setText(
            QStringLiteral("<span style=\"color:#228B22;\">%1</span>")
                .arg(i18n("Available")));
    } else {
        m_btrbkLabel->setText(
            QStringLiteral("<span style=\"color:#CC0000;\">%1</span>")
                .arg(i18n("Not found")));
    }
    m_btrbkLabel->setTextFormat(Qt::RichText);

    // Timer
    const bool   timerEnabled = services.value(QLatin1String("timer_enabled")).toBool();
    const QString timerNext   = services.value(QLatin1String("timer_next")).toString();
    if (timerEnabled && !timerNext.isEmpty()) {
        m_timerLabel->setText(i18n("Enabled — next: %1", timerNext));
    } else if (timerEnabled) {
        m_timerLabel->setText(i18n("Enabled"));
    } else {
        m_timerLabel->setText(
            QStringLiteral("<span style=\"color:#CC0000;\">%1</span>")
                .arg(i18n("Disabled")));
        m_timerLabel->setTextFormat(Qt::RichText);
    }

    // Last backup age
    const qint64 ageSecs = services.value(QLatin1String("last_backup_age_secs")).toInteger(-1);
    if (ageSecs < 0) {
        m_lastBackupLabel->setText(i18n("Unknown"));
    } else {
        const qint64 hours = ageSecs / 3600;
        const qint64 mins  = (ageSecs % 3600) / 60;
        QString ageStr;
        if (hours > 0) {
            ageStr = i18n("%1 hours ago", hours);
        } else {
            ageStr = i18n("%1 minutes ago", mins);
        }

        if (ageSecs > 48 * 3600) {
            m_lastBackupLabel->setText(
                QStringLiteral("<span style=\"color:#CC0000;\">%1</span>").arg(ageStr));
            m_lastBackupLabel->setTextFormat(Qt::RichText);
        } else {
            m_lastBackupLabel->setText(ageStr);
            m_lastBackupLabel->setTextFormat(Qt::AutoText);
        }
    }

    // Drives mounted count
    int mountedCount = 0;
    for (const QJsonValue &dval : drives) {
        if (dval.toObject().value(QLatin1String("mounted")).toBool())
            ++mountedCount;
    }
    const int totalCount = drives.count();
    m_mountLabel->setText(i18n("%1/%2 mounted", mountedCount, totalCount));
}
