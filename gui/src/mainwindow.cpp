#include "mainwindow.h"
#include "database.h"
#include "dbusclient.h"
#include "filemodel.h"
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
#include <KStandardAction>

#include <QAction>
#include <QFileDialog>
#include <QHeaderView>
#include <QLabel>
#include <QLineEdit>
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
    m_database = new Database();
    m_dbusClient = new DBusClient(this);
    m_indexRunner = new IndexRunner(this);
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

    setupUi();
    setupActions();
    setupGUI(Default, QStringLiteral("btrdasd-gui.rc"));

    openDatabase(m_dbPath);
}

MainWindow::~MainWindow()
{
    delete m_database;
}

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

    // Page 1: Backup Run Now (placeholder — M5 will implement)
    m_backupRunPage = new QWidget(this);
    auto *runLayout = new QVBoxLayout(m_backupRunPage);
    auto *runLabel = new QLabel(i18n("Backup operations will appear here."), m_backupRunPage);
    runLabel->setAlignment(Qt::AlignCenter);
    runLayout->addWidget(runLabel);
    m_stack->addWidget(m_backupRunPage); // index 1

    // Page 2: Backup History (placeholder)
    m_backupHistoryPage = new QWidget(this);
    auto *histLayout = new QVBoxLayout(m_backupHistoryPage);
    auto *histLabel = new QLabel(i18n("Backup history will appear here."), m_backupHistoryPage);
    histLabel->setAlignment(Qt::AlignCenter);
    histLayout->addWidget(histLabel);
    m_stack->addWidget(m_backupHistoryPage); // index 2

    // Page 3: Config (placeholder)
    m_configPage = new QWidget(this);
    auto *cfgLayout = new QVBoxLayout(m_configPage);
    auto *cfgLabel = new QLabel(i18n("Configuration editor will appear here."), m_configPage);
    cfgLabel->setAlignment(Qt::AlignCenter);
    cfgLayout->addWidget(cfgLabel);
    m_stack->addWidget(m_configPage); // index 3

    // Page 4: Health — Drives (placeholder)
    m_healthDrivesPage = new QWidget(this);
    auto *drvLayout = new QVBoxLayout(m_healthDrivesPage);
    auto *drvLabel = new QLabel(i18n("Drive health information will appear here."), m_healthDrivesPage);
    drvLabel->setAlignment(Qt::AlignCenter);
    drvLayout->addWidget(drvLabel);
    m_stack->addWidget(m_healthDrivesPage); // index 4

    // Page 5: Health — Growth (placeholder)
    m_healthGrowthPage = new QWidget(this);
    auto *growLayout = new QVBoxLayout(m_healthGrowthPage);
    auto *growLabel = new QLabel(i18n("Disk usage growth charts will appear here."), m_healthGrowthPage);
    growLabel->setAlignment(Qt::AlignCenter);
    growLayout->addWidget(growLabel);
    m_stack->addWidget(m_healthGrowthPage); // index 5

    // Page 6: Health — Status (placeholder)
    m_healthStatusPage = new QWidget(this);
    auto *stLayout = new QVBoxLayout(m_healthStatusPage);
    auto *stLabel = new QLabel(i18n("System status overview will appear here."), m_healthStatusPage);
    stLabel->setAlignment(Qt::AlignCenter);
    stLayout->addWidget(stLabel);
    m_stack->addWidget(m_healthStatusPage); // index 6

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

    m_snapshotModel = new SnapshotModel(m_database, this);
    m_fileModel = new FileModel(m_database, this);
    m_searchModel = new SearchModel(m_database, this);

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
        break;
    case SidebarSection::Config:
        m_stack->setCurrentIndex(3);
        break;
    case SidebarSection::HealthDrives:
        m_stack->setCurrentIndex(4);
        break;
    case SidebarSection::HealthGrowth:
        m_stack->setCurrentIndex(5);
        break;
    case SidebarSection::HealthStatus:
        m_stack->setCurrentIndex(6);
        break;
    }
}

void MainWindow::openDatabase(const QString &path)
{
    if (!m_database->open(path)) {
        KMessageBox::error(this, i18n("Failed to open database: %1", path));
        return;
    }

    m_snapshotModel->reload();
    updateStatusBar();

    // Set watcher to backup target root derived from snapshot paths
    auto snapshots = m_database->listSnapshots();
    if (!snapshots.isEmpty()) {
        QString snapPath = snapshots.first().path;
        int lastSlash = snapPath.lastIndexOf(QLatin1Char('/'));
        if (lastSlash > 0) {
            int secondLastSlash = snapPath.lastIndexOf(QLatin1Char('/'), lastSlash - 1);
            if (secondLastSlash > 0) {
                m_snapshotWatcher->setWatchPath(snapPath.left(secondLastSlash));
            }
        }
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

    auto snapshots = m_database->listSnapshots();
    QString targetPath = QStringLiteral("/mnt/backup-hdd");
    if (!snapshots.isEmpty()) {
        const QString &snapPath = snapshots.first().path;
        const int lastSlash = snapPath.lastIndexOf(QLatin1Char('/'));
        if (lastSlash > 0) {
            const int secondLastSlash = snapPath.lastIndexOf(QLatin1Char('/'), lastSlash - 1);
            if (secondLastSlash > 0) {
                targetPath = snapPath.left(secondLastSlash);
            }
        }
    }

    statusBar()->showMessage(i18n("Re-indexing %1...", targetPath));
    m_indexRunner->run(targetPath, m_dbPath);
}

void MainWindow::showStats()
{
    const auto s = m_database->stats();
    KMessageBox::information(this, i18n(
        "Snapshots: %1\nFiles: %2\nSpans: %3\nDatabase size: %4 bytes",
        s.snapshotCount, s.fileCount, s.spanCount, s.dbSizeBytes));
}

void MainWindow::updateStatusBar()
{
    const auto s = m_database->stats();
    m_statusLabel->setText(i18n("%1 snapshots | %2 files | DB: %3",
        s.snapshotCount, s.fileCount,
        FileModel::formatSize(s.dbSizeBytes)));
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

    QString snapshotPath = m_database->snapshotPathById(m_currentSnapshotId);
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
