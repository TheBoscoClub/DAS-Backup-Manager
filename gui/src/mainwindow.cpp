#include "mainwindow.h"
#include "backuphistory.h"
#include "backuppanel.h"
#include "configdialog.h"
#include "dbusclient.h"
#include "filemodel.h"
#include "healthdashboard.h"
#include "indexrunner.h"
#include "progresspanel.h"
#include "restoreaction.h"
#include "searchmodel.h"
#include "settingsdialog.h"
#include "sidebar.h"
#include "snapshotmodel.h"
#include "snapshotwatcher.h"
#include "snapshottimeline.h"

#include <KActionCollection>
#include <KConfigSkeleton>
#include <KLocalizedString>
#include <KMessageBox>
#include <KNotification>
#include <KStandardAction>
#include <KStatusNotifierItem>

#include <QAction>
#include <QFileDialog>
#include <QAction>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeySequence>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QScrollArea>
#include <QSortFilterProxyModel>
#include <QSplitter>
#include <QStackedWidget>
#include <QStatusBar>
#include <QTableView>
#include <QTimer>
#include <QVBoxLayout>

MainWindow::MainWindow(const QString &dbPath, QWidget *parent)
    : KXmlGuiWindow(parent)
    , m_dbPath(dbPath)
{
    m_dbusClient = new DBusClient(this);
    m_indexRunner = new IndexRunner(m_dbusClient, this);
    m_snapshotWatcher = new SnapshotWatcher(m_indexRunner, this);
    m_snapshotWatcher->setDbPath(m_dbPath);
    m_restoreAction = new RestoreAction(this);
    connect(m_restoreAction, &RestoreAction::finished, this, [this](bool success, const QString &error) {
        if (success) {
            statusBar()->showMessage(i18n("Restore complete"), 5000);
        } else {
            KMessageBox::error(this, i18n("Restore failed: %1", error));
        }
    });

    // Backup finished notifications
    connect(m_dbusClient, &DBusClient::jobFinished,
            this, &MainWindow::onBackupFinished);

    // Show D-Bus errors to the user
    connect(m_dbusClient, &DBusClient::errorOccurred,
            this, [this](const QString &operation, const QString &error) {
                KMessageBox::error(this, i18n("%1: %2", operation, error));
            });

    setupUi();
    setupActions();
    setupTrayIcon();
    setupGUI(Default, QStringLiteral("btrdasd-gui.rc"));

    // Load initial data via D-Bus
    m_snapshotModel->reload();
    updateStatusBar();

    // Status bar auto-refresh every 60 seconds
    m_statusTimer = new QTimer(this);
    m_statusTimer->setInterval(60000);
    connect(m_statusTimer, &QTimer::timeout, this, &MainWindow::updateStatusBar);
    m_statusTimer->start();
}

MainWindow::~MainWindow() = default;

void MainWindow::setupUi()
{
    // --- Sidebar ---
    m_sidebar = new Sidebar(this);
    connect(m_sidebar, &Sidebar::sectionChanged,
            this, &MainWindow::onSectionChanged);

    // --- Stacked widget for main content area ---
    m_stack = new QStackedWidget(this);

    // Page 0: Browse (snapshots + files + search — existing functionality)
    setupBrowsePage();
    m_stack->addWidget(m_browsePage);    // index 0

    // Page 1: Backup Run Now
    m_backupRunPage = new BackupPanel(m_dbusClient, this);
    m_stack->addWidget(m_backupRunPage); // index 1

    // Page 2: Backup History
    m_backupHistoryPage = new BackupHistoryView(m_dbusClient, m_dbPath, this);
    m_stack->addWidget(m_backupHistoryPage); // index 2

    // Page 3: Config (opens ConfigDialog on selection)
    m_configPage = new QWidget(this);
    {
        auto *cfgLayout = new QVBoxLayout(m_configPage);
        cfgLayout->setAlignment(Qt::AlignCenter);
        auto *cfgLabel = new QLabel(i18n("Click the button below or use the menu to open "
                                         "the configuration editor."), m_configPage);
        cfgLabel->setAlignment(Qt::AlignCenter);
        cfgLabel->setWordWrap(true);
        cfgLayout->addWidget(cfgLabel);

        auto *cfgButton = new QPushButton(
            QIcon::fromTheme(QStringLiteral("configure")),
            i18n("Open Config Editor"), m_configPage);
        cfgButton->setToolTip(i18n("Edit DAS backup configuration"));
        cfgButton->setSizePolicy(QSizePolicy::Fixed, QSizePolicy::Fixed);
        cfgLayout->addWidget(cfgButton, 0, Qt::AlignCenter);

        connect(cfgButton, &QPushButton::clicked, this, [this]() {
            auto *dlg = new ConfigDialog(m_dbusClient, this);
            dlg->setAttribute(Qt::WA_DeleteOnClose);
            dlg->show();
        });
    }
    m_stack->addWidget(m_configPage); // index 3

    // Page 4: Health Dashboard (tabs: Drives, Growth, Status)
    m_healthDashboard = new HealthDashboard(m_dbusClient, this);
    m_stack->addWidget(m_healthDashboard); // index 4

    // --- Main layout: sidebar | stack ---
    auto *mainSplitter = new QSplitter(Qt::Horizontal, this);
    mainSplitter->addWidget(m_sidebar);
    mainSplitter->addWidget(m_stack);
    mainSplitter->setStretchFactor(0, 0); // sidebar: fixed width
    mainSplitter->setStretchFactor(1, 1); // stack: expands
    mainSplitter->setChildrenCollapsible(false);

    setCentralWidget(mainSplitter);

    // --- Progress dock panel ---
    m_progressPanel = new ProgressPanel(m_dbusClient, this);
    addDockWidget(Qt::BottomDockWidgetArea, m_progressPanel);

    // --- Status bar ---
    m_statusLabel = new QLabel(this);
    statusBar()->addPermanentWidget(m_statusLabel);

    // Index runner signals
    connect(m_indexRunner, &IndexRunner::finished, this, [this](bool success, const QString &error) {
        if (success) {
            m_snapshotModel->reload();
            updateStatusBar();
            statusBar()->showMessage(i18n("Re-indexing complete"), 5000);
        } else {
            KMessageBox::error(this, i18n("Indexing failed: %1", error));
        }
    });

    // Start on Browse > Snapshots
    m_stack->setCurrentIndex(0);
}

void MainWindow::setupBrowsePage()
{
    m_browsePage = new QWidget(this);

    m_snapshotModel = new SnapshotModel(m_dbusClient, m_dbPath, this);
    m_fileModel = new FileModel(m_dbusClient, m_dbPath, this);
    m_searchModel = new SearchModel(m_dbusClient, m_dbPath, this);

    // Search bar with debounce
    m_searchBar = new QLineEdit(m_browsePage);
    m_searchBar->setPlaceholderText(i18n("Search files (FTS5)..."));
    m_searchBar->setClearButtonEnabled(true);

    m_searchTimer = new QTimer(this);
    m_searchTimer->setSingleShot(true);
    m_searchTimer->setInterval(300);
    connect(m_searchBar, &QLineEdit::textChanged, this, &MainWindow::onSearchTextChanged);
    connect(m_searchTimer, &QTimer::timeout, this, &MainWindow::executeSearch);

    // Timeline in scroll area
    m_timeline = new SnapshotTimeline(m_snapshotModel, this);
    connect(m_timeline, &SnapshotTimeline::snapshotSelected, this, &MainWindow::onSnapshotSelected);

    auto *scrollArea = new QScrollArea(m_browsePage);
    scrollArea->setWidget(m_timeline);
    scrollArea->setWidgetResizable(true);
    scrollArea->setMinimumWidth(200);

    // File list
    m_fileProxy = new QSortFilterProxyModel(this);
    m_fileProxy->setSourceModel(m_fileModel);
    m_fileView = new QTableView(m_browsePage);
    m_fileView->setModel(m_fileProxy);
    m_fileView->setSortingEnabled(true);
    m_fileView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_fileView->setAlternatingRowColors(true);
    m_fileView->horizontalHeader()->setStretchLastSection(true);

    // Search results
    m_searchView = new QTableView(m_browsePage);
    m_searchView->setModel(m_searchModel);
    m_searchView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_searchView->setAlternatingRowColors(true);
    m_searchView->horizontalHeader()->setStretchLastSection(true);
    m_searchView->setVisible(false);

    // Right splitter (files above, search below)
    auto *rightSplitter = new QSplitter(Qt::Vertical, m_browsePage);
    rightSplitter->addWidget(m_fileView);
    rightSplitter->addWidget(m_searchView);
    rightSplitter->setStretchFactor(0, 3);
    rightSplitter->setStretchFactor(1, 1);

    // Browse content splitter (timeline left, files right)
    auto *browseSplitter = new QSplitter(Qt::Horizontal, m_browsePage);
    browseSplitter->addWidget(scrollArea);
    browseSplitter->addWidget(rightSplitter);
    browseSplitter->setStretchFactor(0, 1);
    browseSplitter->setStretchFactor(1, 3);

    // Page layout
    auto *layout = new QVBoxLayout(m_browsePage);
    layout->setContentsMargins(4, 4, 4, 4);
    layout->addWidget(m_searchBar);
    layout->addWidget(browseSplitter, 1);
}

void MainWindow::setupActions()
{
    auto *reindexAction = new QAction(QIcon::fromTheme(QStringLiteral("view-refresh")),
                                       i18n("Re-index"), this);
    reindexAction->setToolTip(i18n("Run btrdasd walk to index new snapshots"));
    actionCollection()->addAction(QStringLiteral("reindex"), reindexAction);
    connect(reindexAction, &QAction::triggered, this, &MainWindow::triggerReindex);

    auto *statsAction = new QAction(QIcon::fromTheme(QStringLiteral("document-properties")),
                                     i18n("Statistics"), this);
    statsAction->setToolTip(i18n("Show database statistics"));
    actionCollection()->addAction(QStringLiteral("stats"), statsAction);
    connect(statsAction, &QAction::triggered, this, &MainWindow::showStats);

    auto *restoreAction = new QAction(QIcon::fromTheme(QStringLiteral("document-save")),
                                       i18n("Restore to..."), this);
    restoreAction->setToolTip(i18n("Restore selected files to a destination"));
    actionCollection()->addAction(QStringLiteral("restore"), restoreAction);
    connect(restoreAction, &QAction::triggered, this, &MainWindow::restoreSelectedFiles);

    auto *backupRunAction = new QAction(QIcon::fromTheme(QStringLiteral("media-playback-start")),
                                          i18n("Run Backup"), this);
    backupRunAction->setToolTip(i18n("Open backup operations panel"));
    actionCollection()->addAction(QStringLiteral("backup_run"), backupRunAction);
    connect(backupRunAction, &QAction::triggered, this, [this]() {
        m_sidebar->setCurrentSection(SidebarSection::BackupRunNow);
    });

    auto *backupHistAction = new QAction(QIcon::fromTheme(QStringLiteral("view-history")),
                                           i18n("Backup History"), this);
    backupHistAction->setToolTip(i18n("View backup run history"));
    actionCollection()->addAction(QStringLiteral("backup_history"), backupHistAction);
    connect(backupHistAction, &QAction::triggered, this, [this]() {
        m_sidebar->setCurrentSection(SidebarSection::BackupHistory);
    });

    auto *healthAction = new QAction(QIcon::fromTheme(QStringLiteral("dialog-information")),
                                       i18n("Health"), this);
    healthAction->setToolTip(i18n("View drive health and system status"));
    actionCollection()->addAction(QStringLiteral("health"), healthAction);
    connect(healthAction, &QAction::triggered, this, [this]() {
        m_sidebar->setCurrentSection(SidebarSection::HealthStatus);
    });

    auto *configAction = new QAction(QIcon::fromTheme(QStringLiteral("configure")),
                                       i18n("Config Editor"), this);
    configAction->setToolTip(i18n("Edit backup configuration"));
    actionCollection()->addAction(QStringLiteral("config_editor"), configAction);
    connect(configAction, &QAction::triggered, this, [this]() {
        m_sidebar->setCurrentSection(SidebarSection::Config);
    });

    auto *searchAction = new QAction(QIcon::fromTheme(QStringLiteral("edit-find")),
                                      i18n("Find Files"), this);
    searchAction->setToolTip(i18n("Search for files across all snapshots"));
    actionCollection()->addAction(QStringLiteral("find_files"), searchAction);
    actionCollection()->setDefaultShortcut(searchAction, QKeySequence::Find);
    connect(searchAction, &QAction::triggered, this, [this]() {
        m_sidebar->setCurrentSection(SidebarSection::BrowseSearch);
    });

    auto *settingsAction = KStandardAction::preferences(this, &MainWindow::showSettings, actionCollection());
    Q_UNUSED(settingsAction);
}

void MainWindow::onSectionChanged(SidebarSection section)
{
    switch (section) {
    case SidebarSection::BrowseSnapshots:
        m_stack->setCurrentIndex(0);
        m_searchBar->setVisible(false);
        m_searchView->setVisible(false);
        break;
    case SidebarSection::BrowseSearch:
        m_stack->setCurrentIndex(0);
        m_searchBar->setVisible(true);
        m_searchBar->setFocus();
        break;
    case SidebarSection::BackupRunNow:
        m_stack->setCurrentIndex(1);
        break;
    case SidebarSection::BackupHistory:
        m_stack->setCurrentIndex(2);
        m_backupHistoryPage->refresh();
        break;
    case SidebarSection::Config:
        m_stack->setCurrentIndex(3);
        break;
    case SidebarSection::HealthDrives:
        m_stack->setCurrentIndex(4);
        m_healthDashboard->setActiveTab(0);
        m_healthDashboard->refresh();
        break;
    case SidebarSection::HealthGrowth:
        m_stack->setCurrentIndex(4);
        m_healthDashboard->setActiveTab(1);
        break;
    case SidebarSection::HealthStatus:
        m_stack->setCurrentIndex(4);
        m_healthDashboard->setActiveTab(2);
        break;
    }
}

void MainWindow::onSnapshotSelected(qint64 snapshotId)
{
    m_currentSnapshotId = snapshotId;
    m_fileModel->loadSnapshot(snapshotId);
}

void MainWindow::onSearchTextChanged(const QString &text)
{
    m_searchTimer->stop();
    if (text.trimmed().isEmpty()) {
        m_searchModel->clear();
        m_searchView->setVisible(false);
        return;
    }
    m_searchTimer->start();
}

void MainWindow::executeSearch()
{
    const QString query = m_searchBar->text().trimmed();
    if (query.isEmpty()) {
        return;
    }

    m_searchModel->executeSearch(query, 50);
    m_searchView->setVisible(true);
    statusBar()->showMessage(i18n("%1 results", m_searchModel->rowCount()), 3000);
}

void MainWindow::triggerReindex()
{
    if (m_indexRunner->isRunning()) {
        KMessageBox::information(this, i18n("Indexing is already running."));
        return;
    }

    // Default target path — the config specifies actual targets
    QString targetPath = QStringLiteral("/mnt/backup-hdd");

    statusBar()->showMessage(i18n("Re-indexing %1...", targetPath));
    m_indexRunner->run(targetPath, m_dbPath);
}

void MainWindow::showStats()
{
    const QString json = m_dbusClient->indexStats(m_dbPath);
    if (json.isEmpty()) {
        KMessageBox::error(this, i18n("Failed to load statistics."));
        return;
    }

    const QJsonObject s = QJsonDocument::fromJson(json.toUtf8()).object();
    KMessageBox::information(this, i18n(
        "Snapshots: %1\nFiles: %2\nSpans: %3\nDatabase size: %4",
        s.value(QLatin1String("snapshots")).toInteger(),
        s.value(QLatin1String("files")).toInteger(),
        s.value(QLatin1String("spans")).toInteger(),
        FileModel::formatSize(s.value(QLatin1String("db_size_bytes")).toInteger())));
}

void MainWindow::updateStatusBar()
{
    QStringList parts;

    // DB stats via D-Bus
    const QString statsJson = m_dbusClient->indexStats(m_dbPath);
    qint64 dbSize = 0;
    qint64 snapshotCount = 0;
    if (!statsJson.isEmpty()) {
        const QJsonObject s = QJsonDocument::fromJson(statsJson.toUtf8()).object();
        dbSize = s.value(QLatin1String("db_size_bytes")).toInteger();
        snapshotCount = s.value(QLatin1String("snapshots")).toInteger();
    }

    // Next backup schedule (from D-Bus)
    const QString scheduleJson = m_dbusClient->scheduleGet(
        QStringLiteral("/etc/das-backup/config.toml"));
    if (!scheduleJson.isEmpty()) {
        const QJsonDocument doc = QJsonDocument::fromJson(scheduleJson.toUtf8());
        const QJsonObject obj = doc.object();
        const QString next = obj.value(QLatin1String("next_incremental")).toString();
        if (!next.isEmpty()) {
            parts.append(i18n("Next: %1", next));
        }
    }

    // Targets online (from health)
    const QString healthJson = m_dbusClient->healthQuery(
        QStringLiteral("/etc/das-backup/config.toml"));
    if (!healthJson.isEmpty()) {
        const QJsonDocument doc = QJsonDocument::fromJson(healthJson.toUtf8());
        const QJsonArray targets = doc.object().value(QLatin1String("targets")).toArray();
        int mounted = 0;
        for (const QJsonValue &v : targets) {
            if (v.toObject().value(QLatin1String("mounted")).toBool())
                ++mounted;
        }
        parts.append(i18n("%1 targets online", mounted));
    }

    parts.append(i18n("DB: %1", FileModel::formatSize(dbSize)));
    parts.append(i18n("%1 snapshots", snapshotCount));

    m_statusLabel->setText(parts.join(QStringLiteral(" | ")));
}

void MainWindow::restoreSelectedFiles()
{
    auto selection = m_fileView->selectionModel()->selectedRows();
    if (selection.isEmpty()) {
        KMessageBox::information(this, i18n("No files selected."));
        return;
    }

    if (m_currentSnapshotId < 0) {
        KMessageBox::information(this, i18n("No snapshot selected."));
        return;
    }

    QString snapshotPath = m_dbusClient->indexSnapshotPath(m_dbPath, m_currentSnapshotId);
    if (snapshotPath.isEmpty()) {
        KMessageBox::error(this, i18n("Could not resolve snapshot path."));
        return;
    }

    QString destDir = QFileDialog::getExistingDirectory(this, i18n("Restore to..."));
    if (destDir.isEmpty()) return;

    // Restore each selected file via KIO::copy
    int fileCount = 0;
    for (const auto &idx : selection) {
        QModelIndex sourceIdx = m_fileProxy->mapToSource(idx);
        QString filePath = m_fileModel->data(
            m_fileModel->index(sourceIdx.row(), 1), Qt::DisplayRole).toString();
        QString fullPath = snapshotPath + QLatin1Char('/') + filePath;
        m_restoreAction->restore(fullPath, destDir);
        ++fileCount;
    }
    statusBar()->showMessage(i18n("Restoring %1 file(s)...", fileCount));
}

void MainWindow::setupTrayIcon()
{
    m_trayIcon = new KStatusNotifierItem(this);
    m_trayIcon->setIconByName(QStringLiteral("btrdasd-gui"));
    m_trayIcon->setToolTipTitle(i18n("DAS Backup Manager"));
    m_trayIcon->setToolTipSubTitle(i18n("Backup management and monitoring"));
    m_trayIcon->setCategory(KStatusNotifierItem::SystemServices);
    m_trayIcon->setStandardActionsEnabled(true);
}

void MainWindow::onBackupFinished(const QString &jobId, bool success, const QString &summary)
{
    Q_UNUSED(jobId);

    auto *notification = new KNotification(
        success ? QStringLiteral("backupComplete") : QStringLiteral("backupFailed"),
        KNotification::CloseOnTimeout, this);
    notification->setTitle(success ? i18n("Backup Complete") : i18n("Backup Failed"));
    notification->setText(summary);
    notification->setIconName(success
        ? QStringLiteral("dialog-positive")
        : QStringLiteral("dialog-error"));
    notification->sendEvent();

    // Update tray tooltip with result
    if (m_trayIcon) {
        m_trayIcon->setToolTipSubTitle(
            success ? i18n("Last backup: success") : i18n("Last backup: FAILED"));
    }

    // Refresh status bar to pick up new schedule info
    updateStatusBar();
}

void MainWindow::showSettings()
{
    if (KConfigDialog::showDialog(QStringLiteral("settings"))) {
        return;
    }

    auto *config = new KConfigSkeleton(QString(), this);
    config->addItemString(QStringLiteral("DatabasePath"), m_dbPath, m_dbPath);

    auto *dialog = new SettingsDialog(this, QStringLiteral("settings"), config);
    dialog->show();
}
