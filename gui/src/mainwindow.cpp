#include "mainwindow.h"
#include "database.h"
#include "filemodel.h"
#include "indexrunner.h"
#include "searchmodel.h"
#include "snapshotmodel.h"
#include "snapshotwatcher.h"
#include "snapshottimeline.h"

#include <KActionCollection>
#include <KLocalizedString>
#include <KMessageBox>

#include <QAction>
#include <QHeaderView>
#include <QLabel>
#include <QLineEdit>
#include <QScrollArea>
#include <QSortFilterProxyModel>
#include <QSplitter>
#include <QStatusBar>
#include <QTableView>
#include <QTimer>
#include <QVBoxLayout>

MainWindow::MainWindow(const QString &dbPath, QWidget *parent)
    : KXmlGuiWindow(parent)
    , m_dbPath(dbPath)
{
    m_database = new Database();
    m_indexRunner = new IndexRunner(this);
    m_snapshotWatcher = new SnapshotWatcher(m_indexRunner, this);
    m_snapshotWatcher->setDbPath(m_dbPath);

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
    m_snapshotModel = new SnapshotModel(m_database, this);
    m_fileModel = new FileModel(m_database, this);
    m_searchModel = new SearchModel(m_database, this);

    // Search bar with debounce
    m_searchBar = new QLineEdit(this);
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

    auto *scrollArea = new QScrollArea(this);
    scrollArea->setWidget(m_timeline);
    scrollArea->setWidgetResizable(true);
    scrollArea->setMinimumWidth(200);

    // File list
    m_fileProxy = new QSortFilterProxyModel(this);
    m_fileProxy->setSourceModel(m_fileModel);
    m_fileView = new QTableView(this);
    m_fileView->setModel(m_fileProxy);
    m_fileView->setSortingEnabled(true);
    m_fileView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_fileView->setAlternatingRowColors(true);
    m_fileView->horizontalHeader()->setStretchLastSection(true);

    // Search results
    m_searchView = new QTableView(this);
    m_searchView->setModel(m_searchModel);
    m_searchView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_searchView->setAlternatingRowColors(true);
    m_searchView->horizontalHeader()->setStretchLastSection(true);
    m_searchView->setVisible(false);

    // Right splitter
    auto *rightSplitter = new QSplitter(Qt::Vertical, this);
    rightSplitter->addWidget(m_fileView);
    rightSplitter->addWidget(m_searchView);
    rightSplitter->setStretchFactor(0, 3);
    rightSplitter->setStretchFactor(1, 1);

    // Main splitter
    auto *mainSplitter = new QSplitter(Qt::Horizontal, this);
    mainSplitter->addWidget(scrollArea);
    mainSplitter->addWidget(rightSplitter);
    mainSplitter->setStretchFactor(0, 1);
    mainSplitter->setStretchFactor(1, 3);

    // Central layout
    auto *centralWidget = new QWidget(this);
    auto *layout = new QVBoxLayout(centralWidget);
    layout->setContentsMargins(4, 4, 4, 4);
    layout->addWidget(m_searchBar);
    layout->addWidget(mainSplitter, 1);
    setCentralWidget(centralWidget);

    // Status bar
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
