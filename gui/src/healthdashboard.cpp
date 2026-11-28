#include "healthdashboard.h"
#include "statustext.h"
#include "dbusclient.h"
#include "filemodel.h"
#include "raidacceptdialog.h"

#include <KLocalizedString>

#include <QColor>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QIcon>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QItemSelectionModel>
#include <QLocale>
#include <QLabel>
#include <QPushButton>
#include <QSplitter>
#include <QStandardItem>
#include <QSortFilterProxyModel>
#include <QStandardItemModel>
#include <QTabWidget>
#include <QTableView>
#include <QVBoxLayout>

#include <QChart>
#include <QChartView>
#include <QDateTimeAxis>
#include <QLineSeries>
#include <QValueAxis>

#include <QButtonGroup>
#include <QCheckBox>
#include <QMouseEvent>
#include <QWheelEvent>

#include <algorithm>
#include <cmath>
#include <functional>

// ---------------------------------------------------------------------------
// The growth chart's view: the wheel zooms the date axis about the pointer,
// a left drag pans it (bd DAS-Backup-Manager-3gzs.15). It only reports where
// across the plot area the gesture was; the dashboard moves the range.
// ---------------------------------------------------------------------------

class GrowthChartView : public QChartView
{
public:
    using QChartView::QChartView;

    // `fraction` of the way across the plot (0 left, 1 right); `factor` below
    // 1 zooms in.
    std::function<void(double fraction, double factor)> onZoom;
    // The drag so far as a fraction of the plot's width (right is positive).
    std::function<void(double fraction)> onPan;

protected:
    void wheelEvent(QWheelEvent *event) override
    {
        const QRectF plot = chart()->plotArea();
        const int delta = event->angleDelta().y();
        if (!onZoom || plot.width() <= 0 || delta == 0) {
            QChartView::wheelEvent(event);
            return;
        }
        const QPointF at = chart()->mapFromScene(mapToScene(event->position().toPoint()));
        const double fraction = std::clamp((at.x() - plot.left()) / plot.width(), 0.0, 1.0);
        onZoom(fraction, std::pow(0.8, delta / 120.0));
        event->accept();
    }

    void mousePressEvent(QMouseEvent *event) override
    {
        if (event->button() == Qt::LeftButton && onPan) {
            m_dragging = true;
            m_lastX = event->position().x();
            setCursor(Qt::ClosedHandCursor);
            event->accept();
            return;
        }
        QChartView::mousePressEvent(event);
    }

    void mouseMoveEvent(QMouseEvent *event) override
    {
        const QRectF plot = chart()->plotArea();
        if (m_dragging && plot.width() > 0) {
            const double x = event->position().x();
            onPan((x - m_lastX) / plot.width());
            m_lastX = x;
            event->accept();
            return;
        }
        QChartView::mouseMoveEvent(event);
    }

    void mouseReleaseEvent(QMouseEvent *event) override
    {
        if (m_dragging && event->button() == Qt::LeftButton) {
            m_dragging = false;
            unsetCursor();
            event->accept();
            return;
        }
        QChartView::mouseReleaseEvent(event);
    }

private:
    bool m_dragging = false;
    double m_lastX = 0;
};

namespace {

constexpr double kGiB = 1024.0 * 1024.0 * 1024.0;

qint64 dayMs(const QDate &date)
{
    return QDateTime(date, QTime(0, 0)).toMSecsSinceEpoch();
}

qint64 todayMs()
{
    return dayMs(QDate::currentDate());
}

// A capacity the helper sends: a positive whole number, else none. null (it
// could not tell) and a missing field are no capacity, never the 0
// toInteger() would make of them.
std::optional<qint64> capacityFromJson(const QJsonValue &value)
{
    if (!value.isDouble())
        return std::nullopt;
    const qint64 total = value.toInteger(-1);
    if (total <= 0)
        return std::nullopt;
    return total;
}

} // namespace

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
        ScrubStatus,
        ScrubAge,
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
        Free,
        EtaFull,
        Count
    };
}

// ---------------------------------------------------------------------------
// HealthDashboard
// ---------------------------------------------------------------------------

HealthDashboard::HealthDashboard(DBusClient *client, QWidget *parent)
    : QWidget(parent)
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

    m_note = new QLabel(this);
    m_note->setObjectName(QStringLiteral("healthNote"));
    m_note->setWordWrap(true);
    m_note->setToolTip(i18n("Why the last health refresh failed; the drives shown are from the time named"));
    m_note->hide();
    layout->addWidget(m_note);

    m_tabs = new QTabWidget(this);
    layout->addWidget(m_tabs, 1);

    setupDrivesTab();
    setupGrowthTab();
    setupStatusTab();

    connect(m_client, &DBusClient::healthQueryResult,
            this, &HealthDashboard::onHealthResult);
    connect(m_client, &DBusClient::refreshFailed,
            this, &HealthDashboard::onRefreshFailed);

    // A backup this GUI started has moved the growth log on: read it again
    // when the job ends (bd DAS-Backup-Manager-3gzs.15). The nightly timer's
    // run sends no job signal; the tab reads it when next opened.
    connect(m_client, &DBusClient::jobStarted, this,
            [this](const QString &jobId, const QString &operation) {
                if (operation == QLatin1String("BackupRun"))
                    m_backupJobs.insert(jobId);
            });
    connect(m_client, &DBusClient::jobFinished, this,
            [this](const QString &jobId, bool, const QString &) {
                if (m_backupJobs.remove(jobId))
                    refresh();
            });

    refresh();
}

// ---------------------------------------------------------------------------
// Tab setup
// ---------------------------------------------------------------------------

void HealthDashboard::setupDrivesTab()
{
    m_drivesView = new QTableView(this);
    m_drivesView->setObjectName(QStringLiteral("healthDrivesView"));
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
        i18n("Scrub"),
        i18n("Scrub Age"),
    });
    auto *drivesProxy = new QSortFilterProxyModel(m_drivesView);
    drivesProxy->setSourceModel(model);
    m_drivesView->setModel(drivesProxy);
    m_drivesView->setSortingEnabled(true);

    m_drivesView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_drivesView->setSelectionMode(QAbstractItemView::SingleSelection);
    m_drivesView->setAlternatingRowColors(true);
    m_drivesView->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_drivesView->setShowGrid(false);
    m_drivesView->verticalHeader()->setVisible(false);

    QHeaderView *hh = m_drivesView->horizontalHeader();
    hh->setStretchLastSection(true);
    hh->setSectionsClickable(true);
    hh->setSortIndicatorShown(true);
    hh->setSectionResizeMode(QHeaderView::ResizeToContents);
    hh->setSectionResizeMode(DrivesCol::Device, QHeaderView::Interactive);
    hh->setSectionResizeMode(DrivesCol::Label,  QHeaderView::Interactive);

    auto *drivesPage = new QWidget(this);
    auto *drivesLayout = new QVBoxLayout(drivesPage);
    drivesLayout->setContentsMargins(0, 0, 0, 0);
    drivesLayout->addWidget(m_drivesView, 1);
    auto *drivesButtons = new QHBoxLayout();
    m_raidAcceptButton = new QPushButton(QIcon::fromTheme(QStringLiteral("dialog-warning")),
                                         i18n("Accept Surviving RAID-1 Drive…"), drivesPage);
    m_raidAcceptButton->setObjectName(QStringLiteral("healthRaidAcceptButton"));
    m_raidAcceptButton->setToolTip(
        i18n("For the selected target: when the RAID-1 guard refuses it for split brain "
             "because the other drive is dead, see what the drive present holds and accept it "
             "as the target's history ('btrdasd raid accept')"));
    m_raidAcceptButton->setEnabled(false);
    m_scrubPauseButton = new QPushButton(QIcon::fromTheme(QStringLiteral("media-playback-pause")),
                                         i18n("Pause Scrub"), drivesPage);
    m_scrubPauseButton->setObjectName(QStringLiteral("healthScrubPauseButton"));
    m_scrubPauseButton->setToolTip(
        i18n("Ask the running scrub pass to stop where it is and end as paused ('btrdasd scrub "
             "pause'). It can take a few minutes; nothing is killed. A paused scrub stays "
             "paused until it is resumed"));
    m_scrubResumeButton = new QPushButton(QIcon::fromTheme(QStringLiteral("media-playback-start")),
                                          i18n("Resume Scrub"), drivesPage);
    m_scrubResumeButton->setObjectName(QStringLiteral("healthScrubResumeButton"));
    m_scrubResumeButton->setToolTip(
        i18n("Lift the pause and start the scrub again ('btrdasd scrub resume'); it continues "
             "from where it stopped"));
    m_scrubResult = new QLabel(drivesPage);
    m_scrubResult->setObjectName(QStringLiteral("healthScrubResult"));
    m_scrubResult->setWordWrap(true);
    m_scrubResult->setToolTip(i18n("What the last Pause Scrub or Resume Scrub did"));
    m_scrubPauseButton->setEnabled(false);
    m_scrubResumeButton->setEnabled(false);
    drivesButtons->addWidget(m_scrubPauseButton);
    drivesButtons->addWidget(m_scrubResumeButton);
    drivesButtons->addWidget(m_scrubResult, 1);
    drivesButtons->addWidget(m_raidAcceptButton);
    connect(m_scrubPauseButton, &QPushButton::clicked, this, [this]() {
        m_scrubBusy = true;
        updateScrubButtons();
        m_scrubResult->setText(i18n("Asking the scrub pass to pause…"));
        m_client->scrubPauseAsync();
    });
    connect(m_scrubResumeButton, &QPushButton::clicked, this, [this]() {
        m_scrubBusy = true;
        updateScrubButtons();
        m_scrubResult->setText(i18n("Resuming the scrub…"));
        m_client->scrubResumeAsync();
    });
    connect(m_client, &DBusClient::scrubActionFinished,
            this, &HealthDashboard::onScrubAction);
    drivesLayout->addLayout(drivesButtons);
    connect(m_drivesView->selectionModel(), &QItemSelectionModel::selectionChanged, this,
            [this]() {
                m_raidAcceptButton->setEnabled(
                    m_drivesView->selectionModel()->hasSelection());
            });
    connect(m_raidAcceptButton, &QPushButton::clicked, this, &HealthDashboard::openRaidAccept);

    m_tabs->addTab(drivesPage, QIcon::fromTheme(QStringLiteral("drive-harddisk")),
                   i18n("Drives"));
}

void HealthDashboard::updateScrubButtons()
{
    m_scrubPauseButton->setEnabled(m_scrubRunning && !m_scrubBusy);
    m_scrubResumeButton->setEnabled(m_scrubPaused && !m_scrubRunning && !m_scrubBusy);
}

void HealthDashboard::onScrubAction(const QString &action, bool ok, const QString &text)
{
    Q_UNUSED(action);
    m_scrubBusy = false;
    m_scrubResult->setText(text);
    m_scrubResult->setStyleSheet(ok ? QString()
                                    : QStringLiteral("color: #cc0000;"));
    updateScrubButtons();
    refresh();
}

void HealthDashboard::openRaidAccept()
{
    const QModelIndexList rows = m_drivesView->selectionModel()->selectedRows(DrivesCol::Label);
    if (rows.isEmpty())
        return;
    const QString label = rows.first().data().toString();
    if (label.isEmpty())
        return;
    auto *dlg = new RaidAcceptDialog(m_client, label, this);
    dlg->setAttribute(Qt::WA_DeleteOnClose);
    dlg->show();
}

void HealthDashboard::setupGrowthTab()
{
    m_growthWidget = new QWidget(this);
    auto *layout = new QVBoxLayout(m_growthWidget);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(4);

    auto *info = new QLabel(
        i18n("Storage usage over time — chart + table with a full-by projection for every row. "
             "Mouse wheel: zoom the dates; drag: move along them."),
        m_growthWidget);
    info->setWordWrap(true);
    info->setToolTip(i18n("Storage usage over time for each backup target"));
    layout->addWidget(info);

    // Range buttons above the chart (bd DAS-Backup-Manager-3gzs.15).
    auto *controls = new QHBoxLayout();
    auto *presets = new QButtonGroup(m_growthWidget);
    presets->setExclusive(true);
    const struct {
        const char *name;
        QString text;
        QString tip;
        GrowthMath::Preset preset;
    } buttons[] = {
        {"growthRange30", i18n("30 days"), i18n("Show the 30 days ending today"), GrowthMath::Preset::Days30},
        {"growthRange90", i18n("90 days"), i18n("Show the 90 days ending today"), GrowthMath::Preset::Days90},
        {"growthRange365", i18n("1 year"), i18n("Show the year ending today"), GrowthMath::Preset::Year},
        {"growthRangeAll", i18n("All"),
         i18n("Show every reading of the targets shown, from the earliest to the latest"),
         GrowthMath::Preset::All},
    };
    for (const auto &b : buttons) {
        auto *button = new QPushButton(b.text, m_growthWidget);
        button->setObjectName(QLatin1String(b.name));
        button->setToolTip(b.tip);
        button->setCheckable(true);
        button->setChecked(b.preset == m_growthPreset);
        presets->addButton(button);
        const GrowthMath::Preset preset = b.preset;
        connect(button, &QPushButton::clicked, this, [this, preset] { chooseGrowthPreset(preset); });
        controls->addWidget(button);
    }
    auto *reset = new QPushButton(i18n("Reset"), m_growthWidget);
    reset->setObjectName(QStringLiteral("growthRangeReset"));
    reset->setToolTip(i18n("Undo any zoom or drag: back to the range chosen above"));
    connect(reset, &QPushButton::clicked, this, [this] { chooseGrowthPreset(m_growthPreset); });
    controls->addWidget(reset);
    controls->addStretch(1);
    m_showRetired = new QCheckBox(i18n("Show retired targets"), m_growthWidget);
    m_showRetired->setObjectName(QStringLiteral("growthShowRetired"));
    m_showRetired->setToolTip(
        i18n("Also show the growth-log series of targets no longer in the configuration, "
             "as grey dashed lines and in the table"));
    connect(m_showRetired, &QCheckBox::toggled, this, [this] { renderGrowth(); });
    controls->addWidget(m_showRetired);
    layout->addLayout(controls);

    // Splitter: chart on top, table on bottom
    m_growthSplitter = new QSplitter(Qt::Vertical, m_growthWidget);

    // Chart view. No series animation: it lags the wheel and a grab can catch
    // a line half drawn.
    auto *chart = new QChart();
    chart->setTitle(i18n("Growth Trend"));
    chart->setAnimationOptions(QChart::NoAnimation);
    chart->legend()->setAlignment(Qt::AlignBottom);

    m_chartView = new GrowthChartView(chart, m_growthSplitter);
    m_chartView->setObjectName(QStringLiteral("growthChartView"));
    m_chartView->setToolTip(i18n("Mouse wheel: zoom the dates about the pointer; drag: move along them; "
                                 "Reset: back to the chosen range"));
    m_chartView->setRenderHint(QPainter::Antialiasing);
    m_chartView->setMinimumHeight(320);
    m_chartView->onZoom = [this](double fraction, double factor) {
        if (!m_growthHaveRange)
            return;
        const qint64 anchor =
            m_growthRange.startMs + static_cast<qint64>(fraction * static_cast<double>(m_growthRange.span()));
        m_growthRange = GrowthMath::zoomRange(m_growthRange, anchor, factor, growthLimit());
        m_growthMoved = true;
        applyGrowthRange();
    };
    m_chartView->onPan = [this](double fraction) {
        if (!m_growthHaveRange)
            return;
        // Dragging right brings earlier dates into view.
        const auto delta = static_cast<qint64>(-fraction * static_cast<double>(m_growthRange.span()));
        m_growthRange = GrowthMath::panRange(m_growthRange, delta, growthLimit());
        m_growthMoved = true;
        applyGrowthRange();
    };

    // Table view
    m_growthView = new QTableView(m_growthSplitter);
    m_growthView->setObjectName(QStringLiteral("healthGrowthView"));
    m_growthView->setToolTip(
        i18n("Each reading of each target shown: used, free and the projected full-by date "
             "from the 14 days up to that row"));

    auto *model = new QStandardItemModel(0, GrowthCol::Count, m_growthView);
    model->setHorizontalHeaderLabels({
        i18n("Date"),
        i18n("Label"),
        i18n("Used"),
        i18n("Free"),
        i18n("ETA Full"),
    });
    auto *growthProxy = new QSortFilterProxyModel(m_growthView);
    growthProxy->setSourceModel(model);
    m_growthView->setModel(growthProxy);
    m_growthView->setSortingEnabled(true);

    m_growthView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_growthView->setSelectionMode(QAbstractItemView::SingleSelection);
    m_growthView->setAlternatingRowColors(true);
    m_growthView->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_growthView->setShowGrid(false);
    m_growthView->verticalHeader()->setVisible(false);

    QHeaderView *hh = m_growthView->horizontalHeader();
    hh->setStretchLastSection(true);
    hh->setSectionsClickable(true);
    hh->setSortIndicatorShown(true);
    hh->setSectionResizeMode(QHeaderView::ResizeToContents);
    hh->setSectionResizeMode(GrowthCol::Date,  QHeaderView::Interactive);
    hh->setSectionResizeMode(GrowthCol::Label, QHeaderView::Interactive);

    m_growthSplitter->addWidget(m_chartView);
    m_growthSplitter->addWidget(m_growthView);
    m_growthSplitter->setStretchFactor(0, 2);  // chart gets more space
    m_growthSplitter->setStretchFactor(1, 1);
    // Two thirds to the chart at first: the table's size hint would
    // otherwise squeeze it to a strip.
    m_growthSplitter->setSizes({600, 300});

    layout->addWidget(m_growthSplitter, 1);

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
    m_client->healthQueryAsync();
}

void HealthDashboard::onHealthResult(const QString &json)
{
    if (json.isEmpty())
        return;

    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    if (doc.isNull() || !doc.isObject())
        return;

    updateDrives(json);
    updateGrowth(json);
    updateStatus(json);
    m_lastGood = QDateTime::currentDateTime();
    m_note->clear();
    m_note->hide();
}

void HealthDashboard::onRefreshFailed(const QString &operation, const QString &reason)
{
    if (operation != QLatin1String("HealthQuery"))
        return;
    // The tables keep the last good reading; this says how old it is.
    m_note->setText(m_lastGood.isValid()
        ? i18n("Health unavailable: %1 — showing the reading from %2",
               reason, m_lastGood.toString(QStringLiteral("HH:mm:ss")))
        : i18n("Health unavailable: %1 — no reading yet", reason));
    m_note->show();
}

QString HealthDashboard::statusNote() const
{
    return m_note->text();
}

// ---------------------------------------------------------------------------
// Private update helpers
// ---------------------------------------------------------------------------

void HealthDashboard::updateDrives(const QString &json)
{
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    const QJsonArray drives = doc.object().value(QLatin1String("targets")).toArray();

    auto *proxy = qobject_cast<QSortFilterProxyModel *>(m_drivesView->model());
    auto *model = proxy
        ? qobject_cast<QStandardItemModel *>(proxy->sourceModel())
        : qobject_cast<QStandardItemModel *>(m_drivesView->model());
    if (!model)
        return;

    model->removeRows(0, model->rowCount());
    m_scrubRunning = false;
    m_scrubPaused = false;

    for (const QJsonValue &val : drives) {
        const QJsonObject drv = val.toObject();

        const QString device   = drv.value(QLatin1String("serial")).toString();
        const QString label    = drv.value(QLatin1String("label")).toString();
        const bool    mounted  = drv.value(QLatin1String("mounted")).toBool();
        const qint64  total    = drv.value(QLatin1String("total_bytes")).toInteger();
        const qint64  used     = drv.value(QLatin1String("used_bytes")).toInteger();
        const qint64  free     = total - used;
        const QString smart    = drv.value(QLatin1String("smart_status")).toString();
        const int     tempC    = drv.value(QLatin1String("temperature_c")).toInt();
        const int     pwrHours = drv.value(QLatin1String("power_on_hours")).toInt();
        const int     errors   = drv.value(QLatin1String("errors")).toInt();

        const QJsonObject scrub       = drv.value(QLatin1String("scrub")).toObject();
        const QString     scrubStatus = scrub.value(QLatin1String("status")).toString();
        const QJsonValue  scrubAgeVal = scrub.value(QLatin1String("age_days"));
        const QString     scrubOutcome = scrub.value(QLatin1String("last_outcome")).toString();
        const bool        scrubIsPaused = scrubOutcome == QLatin1String("paused");
        // A pass is going (or has stepped aside for a backup) while the
        // latest attempt reads running or yielded.
        if (scrubOutcome == QLatin1String("running") || scrubOutcome == QLatin1String("yielded"))
            m_scrubRunning = true;
        if (scrubIsPaused)
            m_scrubPaused = true;

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

        // Scrub status — mirrors the SMART column's color convention:
        // green for OK, red for anything the operator needs to act on,
        // default color for states that are informational, not alarming
        // (not-applicable, or the target simply predates the scrub feature).
        auto *scrubStatusItem = new QStandardItem();
        if (scrubStatus == QLatin1String("ok")) {
            scrubStatusItem->setText(i18n("OK"));
            scrubStatusItem->setForeground(QColor(0x22, 0x8B, 0x22));  // forest green
        } else if (scrubStatus == QLatin1String("warn") && scrubIsPaused) {
            // Paused by hand: resumable, not a fault (bd DAS-Backup-Manager-su1b.15).
            scrubStatusItem->setText(i18n("Paused"));
            scrubStatusItem->setForeground(QColor(0xCC, 0x88, 0x00));  // amber
        } else if (scrubStatus == QLatin1String("warn")) {
            scrubStatusItem->setText(i18n("WARN"));
            scrubStatusItem->setForeground(QColor(0xCC, 0x88, 0x00));  // amber
        } else if (scrubStatus == QLatin1String("fail")) {
            scrubStatusItem->setText(i18n("FAIL"));
            scrubStatusItem->setForeground(QColor(0xCC, 0x00, 0x00));  // red
        } else if (scrubStatus == QLatin1String("never_scrubbed")) {
            scrubStatusItem->setText(i18n("Never scrubbed"));
            scrubStatusItem->setForeground(QColor(0xCC, 0x88, 0x00));  // amber
        } else if (scrubStatus == QLatin1String("unresolved")) {
            scrubStatusItem->setText(i18n("Unresolved"));
        } else {
            // "not_applicable" (scrub disabled, or this target isn't a
            // configured scrub target) — nothing to flag.
            scrubStatusItem->setText(QStringLiteral("—"));
        }
        QString scrubTip = i18n("BTRFS scrub health from the scrub engine's persisted state "
                                "(see btrdasd scrub status for raw per-device history)");
        if (scrubIsPaused) {
            const QJsonValue since = scrub.value(QLatin1String("paused_since_epoch"));
            const QString by = scrub.value(QLatin1String("paused_by")).toString();
            QString detail = i18n("Scrub paused by hand; Resume Scrub continues it");
            if (since.isDouble())
                detail += i18n(" — since %1",
                               QLocale().toString(QDateTime::fromSecsSinceEpoch(
                                                      static_cast<qint64>(since.toDouble())),
                                                  QLocale::ShortFormat));
            if (!by.isEmpty())
                detail += i18n(", asked by %1", by);
            scrubTip = detail + QLatin1Char('\n') + scrubTip;
        }
        scrubStatusItem->setToolTip(scrubTip);
        row.append(scrubStatusItem);

        // Scrub age
        const QString scrubAgeStr = scrubAgeVal.isDouble()
            ? i18n("%1 d", static_cast<int>(scrubAgeVal.toDouble()))
            : QStringLiteral("—");
        row.append(new QStandardItem(scrubAgeStr));

        for (QStandardItem *item : row)
            item->setEditable(false);

        model->appendRow(row);
    }
    updateScrubButtons();
}

void HealthDashboard::updateGrowth(const QString &json)
{
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    const QJsonObject root = doc.object();

    // The current targets: a growth series under any other name is retired
    // (bd DAS-Backup-Manager-3gzs.15).
    QStringList current;
    for (const QJsonValue &t : root.value(QLatin1String("targets")).toArray())
        current.append(t.toObject().value(QLatin1String("label")).toString());

    m_growthSeries.clear();
    for (const QJsonValue &gval : root.value(QLatin1String("growth")).toArray()) {
        const QJsonObject gobj = gval.toObject();
        GrowthSeries series;
        series.label = gobj.value(QLatin1String("label")).toString();
        series.retired = GrowthMath::isRetired(series.label, current);
        for (const QJsonValue &eval : gobj.value(QLatin1String("entries")).toArray()) {
            const QJsonObject entry = eval.toObject();
            const QDate date =
                QDate::fromString(entry.value(QLatin1String("date")).toString(), Qt::ISODate);
            if (!date.isValid())
                continue;
            series.readings.append({date, entry.value(QLatin1String("used_bytes")).toInteger(),
                                    capacityFromJson(entry.value(QLatin1String("total_bytes")))});
        }
        std::stable_sort(series.readings.begin(), series.readings.end(),
                         [](const GrowthMath::Reading &a, const GrowthMath::Reading &b) {
                             return a.date < b.date;
                         });
        if (!series.readings.isEmpty())
            m_growthSeries.append(series);
    }
    renderGrowth();
}

GrowthMath::Range HealthDashboard::growthPresetRange(GrowthMath::Preset preset) const
{
    std::optional<qint64> earliest;
    std::optional<qint64> latest;
    const bool showRetired = m_showRetired && m_showRetired->isChecked();
    for (const GrowthSeries &s : m_growthSeries) {
        if (s.retired && !showRetired)
            continue;
        const qint64 first = dayMs(s.readings.first().date);
        const qint64 last = dayMs(s.readings.last().date);
        earliest = earliest ? std::min(*earliest, first) : first;
        latest = latest ? std::max(*latest, last) : last;
    }
    return GrowthMath::presetRange(preset, earliest, latest, todayMs());
}

GrowthMath::Range HealthDashboard::growthLimit() const
{
    const GrowthMath::Range all = growthPresetRange(GrowthMath::Preset::All);
    const GrowthMath::Range preset = growthPresetRange(m_growthPreset);
    return {std::min(all.startMs, preset.startMs), std::max(all.endMs, preset.endMs)};
}

void HealthDashboard::chooseGrowthPreset(GrowthMath::Preset preset)
{
    m_growthPreset = preset;
    m_growthMoved = false;
    m_growthRange = growthPresetRange(m_growthPreset);
    m_growthHaveRange = true;
    applyGrowthRange();
}

void HealthDashboard::renderGrowth()
{
    auto *growthProxyModel = qobject_cast<QSortFilterProxyModel *>(m_growthView->model());
    auto *model = growthProxyModel
        ? qobject_cast<QStandardItemModel *>(growthProxyModel->sourceModel())
        : qobject_cast<QStandardItemModel *>(m_growthView->model());
    if (!model)
        return;

    model->removeRows(0, model->rowCount());

    auto *chart = m_chartView->chart();
    chart->removeAllSeries();
    // One pair of axes for the chart's life: an axis removed and replaced on
    // every refresh left its labels drawn under the new one's.
    QDateTimeAxis *axisX = nullptr;
    QValueAxis *axisY = nullptr;
    for (auto *a : chart->axes()) {
        if (!axisX)
            axisX = qobject_cast<QDateTimeAxis *>(a);
        if (!axisY)
            axisY = qobject_cast<QValueAxis *>(a);
    }
    if (!axisX) {
        axisX = new QDateTimeAxis();
        axisX->setTitleText(i18n("Date"));
        axisX->setTickCount(GrowthMath::kDateTicks);
        chart->addAxis(axisX, Qt::AlignBottom);
    }
    if (!axisY) {
        axisY = new QValueAxis();
        axisY->setTitleText(i18n("Used (GiB)"));
        axisY->setLabelFormat(QStringLiteral("%.0f"));
        chart->addAxis(axisY, Qt::AlignLeft);
    }

    const bool showRetired = m_showRetired->isChecked();
    int currentIdx = 0;
    int retiredIdx = 0;

    for (const GrowthSeries &gs : std::as_const(m_growthSeries)) {
        if (gs.retired && !showRetired)
            continue;

        auto *series = new QLineSeries();
        QPen pen;
        pen.setWidth(2);
        if (gs.retired) {
            series->setName(i18n("%1 (retired)", gs.label));
            pen.setColor(GrowthMath::retiredColor(retiredIdx++));
            pen.setStyle(Qt::DashLine);
        } else {
            series->setName(gs.label);
            pen.setColor(GrowthMath::seriesColor(currentIdx++));
        }
        series->setPen(pen);

        for (int i = 0; i < gs.readings.size(); ++i) {
            const GrowthMath::Reading &r = gs.readings[i];
            series->append(static_cast<qreal>(dayMs(r.date)), static_cast<qreal>(r.used) / kGiB);

            QList<QStandardItem *> row;
            row.reserve(GrowthCol::Count);
            row.append(new QStandardItem(r.date.toString(Qt::ISODate)));
            auto *labelItem = new QStandardItem(gs.label);
            if (gs.retired)
                labelItem->setToolTip(i18n("Retired: no target in the configuration has this name"));
            row.append(labelItem);
            row.append(new QStandardItem(FileModel::formatSize(r.used)));

            auto *freeItem = new QStandardItem();
            if (const auto free = GrowthMath::freeOf(r)) {
                freeItem->setText(FileModel::formatSize(*free));
                freeItem->setToolTip(i18n("Capacity %1 less used %2", FileModel::formatSize(*r.total),
                                          FileModel::formatSize(r.used)));
            } else {
                freeItem->setText(i18n("unknown"));
                freeItem->setToolTip(
                    i18n("No capacity is known for this reading: its growth-log line carries none, no "
                         "other line for this target records one, and the target is not mounted now"));
            }
            row.append(freeItem);

            const GrowthMath::Eta eta = GrowthMath::etaAt(gs.readings, i);
            auto *etaItem = new QStandardItem();
            const QString window = i18n("%1 reading(s), %2 to %3", eta.readings,
                                        eta.windowStart.toString(Qt::ISODate),
                                        eta.windowEnd.toString(Qt::ISODate));
            const QString rate = FileModel::formatSize(static_cast<qint64>(std::llround(eta.bytesPerDay)));
            switch (eta.kind) {
            case GrowthMath::EtaKind::Date:
                etaItem->setText(eta.date.toString(Qt::ISODate));
                etaItem->setToolTip(i18n("Projected from %1: %2 a day", window, rate));
                break;
            case GrowthMath::EtaKind::BeyondTenYears:
                etaItem->setText(i18n("> 10 years"));
                etaItem->setToolTip(i18n("Projected from %1: %2 a day", window, rate));
                break;
            case GrowthMath::EtaKind::NotEnoughHistory:
                etaItem->setText(i18n("not enough history"));
                etaItem->setToolTip(i18n("Fewer than two days of readings in the %1 days up to this date",
                                         GrowthMath::kEtaWindowDays));
                break;
            case GrowthMath::EtaKind::CapacityUnknown:
                etaItem->setText(i18n("capacity unknown"));
                etaItem->setToolTip(i18n("No capacity is known for this reading, so there is nothing "
                                         "to project to"));
                break;
            case GrowthMath::EtaKind::Flat:
                etaItem->setText(i18n("flat"));
                etaItem->setToolTip(i18n("No change in %1", window));
                break;
            case GrowthMath::EtaKind::Shrinking: {
                const QString loss = FileModel::formatSize(static_cast<qint64>(std::llround(-eta.bytesPerDay)));
                etaItem->setText(i18n("shrinking (%1/day)", loss));
                etaItem->setToolTip(i18n("Used fell by %1 a day over %2: no full-by date while it shrinks",
                                         loss, window));
                break;
            }
            case GrowthMath::EtaKind::Full:
                etaItem->setText(i18n("full"));
                etaItem->setToolTip(i18n("Used is at or past the capacity"));
                break;
            }
            row.append(etaItem);

            for (QStandardItem *item : row)
                item->setEditable(false);
            model->appendRow(row);
        }

        chart->addSeries(series);
        series->attachAxis(axisX);
        series->attachAxis(axisY);
    }

    // The label column fits its longest label (it stays resizable).
    m_growthView->resizeColumnToContents(GrowthCol::Label);

    // The view the user had survives a refresh; otherwise the chosen range,
    // which a new day or a newly shown series moves.
    if (!m_growthHaveRange || !m_growthMoved) {
        m_growthRange = growthPresetRange(m_growthPreset);
        m_growthHaveRange = true;
    }
    applyGrowthRange();
}

void HealthDashboard::applyGrowthRange()
{
    auto *chart = m_chartView->chart();
    QDateTimeAxis *axisX = nullptr;
    QValueAxis *axisY = nullptr;
    for (auto *a : chart->axes()) {
        if (!axisX)
            axisX = qobject_cast<QDateTimeAxis *>(a);
        if (!axisY)
            axisY = qobject_cast<QValueAxis *>(a);
    }
    if (!axisX || !axisY)
        return;

    // The date axis is always ranged here: left alone, it keeps the span of
    // the first series attached (the chart stopped at 05-17).
    axisX->setFormat(GrowthMath::dateFormatFor(m_growthRange.span()));
    axisX->setRange(QDateTime::fromMSecsSinceEpoch(m_growthRange.startMs),
                    QDateTime::fromMSecsSinceEpoch(m_growthRange.endMs));

    // The value axis fits what is visible in that window.
    std::optional<double> lo;
    std::optional<double> hi;
    const bool showRetired = m_showRetired->isChecked();
    for (const GrowthSeries &gs : std::as_const(m_growthSeries)) {
        if (gs.retired && !showRetired)
            continue;
        for (const GrowthMath::Reading &r : gs.readings) {
            const qint64 at = dayMs(r.date);
            if (at < m_growthRange.startMs || at > m_growthRange.endMs)
                continue;
            const double v = static_cast<double>(r.used) / kGiB;
            lo = lo ? std::min(*lo, v) : v;
            hi = hi ? std::max(*hi, v) : v;
        }
    }
    const GrowthMath::Axis y = GrowthMath::niceAxis(lo.value_or(0), hi.value_or(1));
    axisY->setRange(y.lo, y.hi);
    axisY->setTickCount(y.ticks);
}

void HealthDashboard::updateStatus(const QString &json)
{
    const QJsonDocument doc      = QJsonDocument::fromJson(json.toUtf8());
    const QJsonObject   root     = doc.object();
    const QJsonObject   services = root.value(QLatin1String("services")).toObject();
    const QJsonArray    drives   = root.value(QLatin1String("targets")).toArray();

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
    const QString ageStr = StatusText::lastBackupLine(ageSecs);
    if (ageSecs > qint64{48} * 3600) {
        m_lastBackupLabel->setText(
            QStringLiteral("<span style=\"color:#CC0000;\">%1</span>").arg(ageStr.toHtmlEscaped()));
        m_lastBackupLabel->setTextFormat(Qt::RichText);
    } else {
        m_lastBackupLabel->setText(ageStr);
        m_lastBackupLabel->setTextFormat(Qt::PlainText);
    }

    // Drives mounted count
    int mountedCount = 0;
    for (const QJsonValue &dval : drives) {
        if (dval.toObject().value(QLatin1String("mounted")).toBool())
            ++mountedCount;
    }
    const int totalCount = static_cast<int>(drives.count());
    m_mountLabel->setText(i18n("%1/%2 mounted", mountedCount, totalCount));
}
