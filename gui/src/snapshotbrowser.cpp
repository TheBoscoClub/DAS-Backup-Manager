#include "snapshotbrowser.h"
#include "dbusclient.h"

#include <KLocalizedString>

#include <QAction>
#include <QApplication>
#include <QClipboard>
#include <QDateTime>
#include <QDir>
#include <QFileInfo>
#include <QFileSystemModel>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QIcon>
#include <QKeySequence>
#include <QLineEdit>
#include <QListView>
#include <QLocale>
#include <QMenu>
#include <QMessageBox>
#include <QPushButton>
#include <QScrollArea>
#include <QStackedWidget>
#include <QToolButton>
#include <QTreeView>
#include <QVBoxLayout>

// ---------------------------------------------------------------------------
// SnapshotBrowser
// ---------------------------------------------------------------------------

SnapshotBrowser::SnapshotBrowser(DBusClient *client, QWidget *parent)
    : QWidget(parent)
    , m_client(client)
{
    auto *layout = new QVBoxLayout(this);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);

    setupActions();
    setupNavigationBar();
    setupBrowserViews();
    setupFilterBar();

    layout->addWidget(m_breadcrumbContainer);
    layout->addWidget(m_viewStack, 1);
    layout->addWidget(m_filterBar);

    m_filterBar->setVisible(false);
    updateNavigationButtons();
}

// ---------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------

void SnapshotBrowser::setupActions()
{
    m_filterAction = new QAction(i18n("Toggle Filter Bar"), this);
    m_filterAction->setShortcut(QKeySequence(Qt::CTRL | Qt::Key_I));
    m_filterAction->setToolTip(i18n("Show or hide the filename filter bar (Ctrl+I)"));
    connect(m_filterAction, &QAction::triggered, this, &SnapshotBrowser::toggleFilterBar);
    addAction(m_filterAction);
}

void SnapshotBrowser::setupNavigationBar()
{
    m_breadcrumbContainer = new QWidget(this);
    auto *navLayout = new QHBoxLayout(m_breadcrumbContainer);
    navLayout->setContentsMargins(4, 4, 4, 4);
    navLayout->setSpacing(2);

    // Back button
    m_backButton = new QToolButton(m_breadcrumbContainer);
    m_backButton->setIcon(QIcon::fromTheme(QStringLiteral("go-previous")));
    m_backButton->setToolTip(i18n("Go back to the previous directory"));
    m_backButton->setAutoRaise(true);
    connect(m_backButton, &QToolButton::clicked, this, &SnapshotBrowser::goBack);
    navLayout->addWidget(m_backButton);

    // Forward button
    m_forwardButton = new QToolButton(m_breadcrumbContainer);
    m_forwardButton->setIcon(QIcon::fromTheme(QStringLiteral("go-next")));
    m_forwardButton->setToolTip(i18n("Go forward to the next directory"));
    m_forwardButton->setAutoRaise(true);
    connect(m_forwardButton, &QToolButton::clicked, this, &SnapshotBrowser::goForward);
    navLayout->addWidget(m_forwardButton);

    // Up button
    m_upButton = new QToolButton(m_breadcrumbContainer);
    m_upButton->setIcon(QIcon::fromTheme(QStringLiteral("go-up")));
    m_upButton->setToolTip(i18n("Navigate to the parent directory"));
    m_upButton->setAutoRaise(true);
    connect(m_upButton, &QToolButton::clicked, this, &SnapshotBrowser::goUp);
    navLayout->addWidget(m_upButton);

    // Separator
    navLayout->addSpacing(6);

    // Breadcrumb area in a scroll area for long paths
    auto *scrollArea = new QScrollArea(m_breadcrumbContainer);
    scrollArea->setFrameShape(QFrame::NoFrame);
    scrollArea->setWidgetResizable(true);
    scrollArea->setVerticalScrollBarPolicy(Qt::ScrollBarAlwaysOff);
    scrollArea->setHorizontalScrollBarPolicy(Qt::ScrollBarAsNeeded);
    scrollArea->setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Fixed);
    scrollArea->setMaximumHeight(32);

    auto *breadcrumbWidget = new QWidget(scrollArea);
    m_breadcrumbLayout = new QHBoxLayout(breadcrumbWidget);
    m_breadcrumbLayout->setContentsMargins(0, 0, 0, 0);
    m_breadcrumbLayout->setSpacing(0);
    m_breadcrumbLayout->addStretch(1);
    scrollArea->setWidget(breadcrumbWidget);

    navLayout->addWidget(scrollArea, 1);

    // Separator before view mode buttons
    navLayout->addSpacing(6);

    // Detail mode button
    m_detailModeButton = new QToolButton(m_breadcrumbContainer);
    m_detailModeButton->setIcon(QIcon::fromTheme(QStringLiteral("view-list-details")));
    m_detailModeButton->setToolTip(i18n("Switch to detail view with columns"));
    m_detailModeButton->setAutoRaise(true);
    m_detailModeButton->setCheckable(true);
    m_detailModeButton->setChecked(true);
    connect(m_detailModeButton, &QToolButton::clicked, this, &SnapshotBrowser::setDetailMode);
    navLayout->addWidget(m_detailModeButton);

    // Icon mode button
    m_iconModeButton = new QToolButton(m_breadcrumbContainer);
    m_iconModeButton->setIcon(QIcon::fromTheme(QStringLiteral("view-list-icons")));
    m_iconModeButton->setToolTip(i18n("Switch to icon view"));
    m_iconModeButton->setAutoRaise(true);
    m_iconModeButton->setCheckable(true);
    m_iconModeButton->setChecked(false);
    connect(m_iconModeButton, &QToolButton::clicked, this, &SnapshotBrowser::setIconMode);
    navLayout->addWidget(m_iconModeButton);
}

void SnapshotBrowser::setupBrowserViews()
{
    m_fsModel = new QFileSystemModel(this);
    m_fsModel->setReadOnly(true);
    m_fsModel->setOption(QFileSystemModel::DontWatchForChanges, true);

    m_viewStack = new QStackedWidget(this);

    // Detail view (QTreeView) — index 0
    m_treeView = new QTreeView(m_viewStack);
    m_treeView->setModel(m_fsModel);
    m_treeView->setSelectionMode(QAbstractItemView::ExtendedSelection);
    m_treeView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_treeView->setAlternatingRowColors(true);
    m_treeView->setSortingEnabled(true);
    m_treeView->setRootIsDecorated(false);
    m_treeView->setUniformRowHeights(true);
    m_treeView->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_treeView->setContextMenuPolicy(Qt::CustomContextMenu);
    m_treeView->setToolTip(i18n("Snapshot file listing — detail view"));

    QHeaderView *header = m_treeView->header();
    header->setStretchLastSection(true);
    header->setSectionResizeMode(QHeaderView::ResizeToContents);
    header->setSectionResizeMode(0, QHeaderView::Interactive);

    connect(m_treeView, &QTreeView::activated,
            this, &SnapshotBrowser::onItemActivated);
    connect(m_treeView, &QTreeView::customContextMenuRequested,
            this, &SnapshotBrowser::showContextMenu);

    m_viewStack->addWidget(m_treeView);

    // Icon view (QListView) — index 1
    m_listView = new QListView(m_viewStack);
    m_listView->setModel(m_fsModel);
    m_listView->setViewMode(QListView::IconMode);
    m_listView->setSelectionMode(QAbstractItemView::ExtendedSelection);
    m_listView->setResizeMode(QListView::Adjust);
    m_listView->setWordWrap(true);
    m_listView->setSpacing(8);
    m_listView->setIconSize(QSize(48, 48));
    m_listView->setGridSize(QSize(96, 80));
    m_listView->setUniformItemSizes(false);
    m_listView->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_listView->setContextMenuPolicy(Qt::CustomContextMenu);
    m_listView->setToolTip(i18n("Snapshot file listing — icon view"));

    connect(m_listView, &QListView::activated,
            this, &SnapshotBrowser::onItemActivated);
    connect(m_listView, &QListView::customContextMenuRequested,
            this, &SnapshotBrowser::showContextMenu);

    m_viewStack->addWidget(m_listView);

    // Start in detail mode
    m_viewStack->setCurrentIndex(0);
}

void SnapshotBrowser::setupFilterBar()
{
    m_filterBar = new QWidget(this);
    auto *filterLayout = new QHBoxLayout(m_filterBar);
    filterLayout->setContentsMargins(4, 2, 4, 2);
    filterLayout->setSpacing(4);

    auto *filterLabel = new QToolButton(m_filterBar);
    filterLabel->setIcon(QIcon::fromTheme(QStringLiteral("view-filter")));
    filterLabel->setToolTip(i18n("Filter files by name pattern"));
    filterLabel->setAutoRaise(true);
    filterLabel->setEnabled(false);
    filterLayout->addWidget(filterLabel);

    m_filterEdit = new QLineEdit(m_filterBar);
    m_filterEdit->setPlaceholderText(i18n("Filter by name (e.g., *.txt, photo*)"));
    m_filterEdit->setToolTip(i18n("Enter a filename pattern to filter the displayed files"));
    m_filterEdit->setClearButtonEnabled(true);
    filterLayout->addWidget(m_filterEdit, 1);

    auto *closeButton = new QToolButton(m_filterBar);
    closeButton->setIcon(QIcon::fromTheme(QStringLiteral("dialog-close")));
    closeButton->setToolTip(i18n("Close the filter bar"));
    closeButton->setAutoRaise(true);
    connect(closeButton, &QToolButton::clicked, this, &SnapshotBrowser::toggleFilterBar);
    filterLayout->addWidget(closeButton);

    connect(m_filterEdit, &QLineEdit::textChanged,
            this, &SnapshotBrowser::applyNameFilter);
}

// ---------------------------------------------------------------------------
// Public methods
// ---------------------------------------------------------------------------

void SnapshotBrowser::setSnapshotPath(const QString &path)
{
    m_rootPath = QDir::cleanPath(path);
    m_historyBack.clear();
    m_historyForward.clear();

    const QModelIndex rootIndex = m_fsModel->setRootPath(m_rootPath);
    m_treeView->setRootIndex(rootIndex);
    m_listView->setRootIndex(rootIndex);

    m_currentPath = m_rootPath;
    updateBreadcrumbs();
    updateNavigationButtons();

    Q_EMIT pathChanged(m_currentPath);
}

void SnapshotBrowser::navigateToPath(const QString &path)
{
    navigateInternal(path, true);
}

// ---------------------------------------------------------------------------
// Navigation slots
// ---------------------------------------------------------------------------

void SnapshotBrowser::goBack()
{
    if (m_historyBack.isEmpty())
        return;

    m_historyForward.prepend(m_currentPath);
    const QString target = m_historyBack.takeLast();
    navigateInternal(target, false);
}

void SnapshotBrowser::goForward()
{
    if (m_historyForward.isEmpty())
        return;

    m_historyBack.append(m_currentPath);
    const QString target = m_historyForward.takeFirst();
    navigateInternal(target, false);
}

void SnapshotBrowser::goUp()
{
    if (m_currentPath == m_rootPath || m_currentPath.isEmpty())
        return;

    QDir dir(m_currentPath);
    if (!dir.cdUp())
        return;

    const QString parentPath = dir.absolutePath();

    // Don't go above the snapshot root
    if (!parentPath.startsWith(m_rootPath))
        return;

    navigateInternal(parentPath, true);
}

void SnapshotBrowser::onItemActivated(const QModelIndex &index)
{
    if (!index.isValid())
        return;

    const QFileInfo info = m_fsModel->fileInfo(index);
    if (info.isDir()) {
        navigateInternal(info.absoluteFilePath(), true);
    }
}

// ---------------------------------------------------------------------------
// View mode
// ---------------------------------------------------------------------------

void SnapshotBrowser::setDetailMode()
{
    m_viewStack->setCurrentIndex(0);
    m_detailModeButton->setChecked(true);
    m_iconModeButton->setChecked(false);
}

void SnapshotBrowser::setIconMode()
{
    m_viewStack->setCurrentIndex(1);
    m_detailModeButton->setChecked(false);
    m_iconModeButton->setChecked(true);
}

// ---------------------------------------------------------------------------
// Filter bar
// ---------------------------------------------------------------------------

void SnapshotBrowser::toggleFilterBar()
{
    const bool show = !m_filterBar->isVisible();
    m_filterBar->setVisible(show);

    if (show) {
        m_filterEdit->setFocus();
        m_filterEdit->selectAll();
    } else {
        m_filterEdit->clear();
        m_fsModel->setNameFilters({});
        m_fsModel->setNameFilterDisables(false);
    }
}

void SnapshotBrowser::applyNameFilter(const QString &pattern)
{
    if (pattern.isEmpty()) {
        m_fsModel->setNameFilters({});
        m_fsModel->setNameFilterDisables(false);
        return;
    }

    // Support multiple patterns separated by spaces
    QStringList patterns;
    const QStringList parts = pattern.split(QLatin1Char(' '), Qt::SkipEmptyParts);
    for (const QString &part : parts) {
        // If the user didn't include a wildcard, wrap with wildcards
        if (!part.contains(QLatin1Char('*')) && !part.contains(QLatin1Char('?'))) {
            patterns.append(QLatin1Char('*') + part + QLatin1Char('*'));
        } else {
            patterns.append(part);
        }
    }

    m_fsModel->setNameFilters(patterns);
    m_fsModel->setNameFilterDisables(false);
}

// ---------------------------------------------------------------------------
// Context menu
// ---------------------------------------------------------------------------

void SnapshotBrowser::showContextMenu(const QPoint &pos)
{
    // Determine which view triggered the menu
    QAbstractItemView *view = (m_viewStack->currentIndex() == 0)
        ? static_cast<QAbstractItemView *>(m_treeView)
        : static_cast<QAbstractItemView *>(m_listView);

    const QModelIndex index = view->indexAt(pos);
    const QStringList paths = selectedPaths();

    if (!m_contextMenu) {
        m_contextMenu = new QMenu(this);
    }
    m_contextMenu->clear();

    if (!paths.isEmpty()) {
        // Restore action
        auto *restoreAct = m_contextMenu->addAction(
            QIcon::fromTheme(QStringLiteral("edit-undo")),
            i18n("Restore to..."));
        restoreAct->setToolTip(i18n("Restore selected files to a chosen destination"));
        connect(restoreAct, &QAction::triggered, this, [this, paths]() {
            Q_EMIT restoreRequested(paths);
        });

        m_contextMenu->addSeparator();

        // Copy path
        auto *copyPathAct = m_contextMenu->addAction(
            QIcon::fromTheme(QStringLiteral("edit-copy-path")),
            i18n("Copy Path"));
        copyPathAct->setToolTip(i18n("Copy the full file path to the clipboard"));
        connect(copyPathAct, &QAction::triggered, this, [paths]() {
            QApplication::clipboard()->setText(paths.join(QLatin1Char('\n')));
        });

        m_contextMenu->addSeparator();

        // Properties (only for single selection)
        if (paths.size() == 1 && index.isValid()) {
            auto *propsAct = m_contextMenu->addAction(
                QIcon::fromTheme(QStringLiteral("document-properties")),
                i18n("Properties"));
            propsAct->setToolTip(i18n("Show file size, permissions, and modification date"));
            connect(propsAct, &QAction::triggered, this, [this, index]() {
                const QFileInfo fi = m_fsModel->fileInfo(index);

                const QString sizeStr = QLocale().formattedDataSize(fi.size());
                const QString modified = QLocale().toString(fi.lastModified(), QLocale::LongFormat);
                const QString permsStr = QStringLiteral("%1").arg(
                    static_cast<uint>(fi.permissions()), 4, 8, QLatin1Char('0'));

                const QString details = i18n(
                    "Name: %1\n"
                    "Size: %2\n"
                    "Permissions: 0%3\n"
                    "Modified: %4\n"
                    "Type: %5",
                    fi.fileName(),
                    sizeStr,
                    permsStr,
                    modified,
                    fi.isDir() ? i18n("Directory") :
                    fi.isSymLink() ? i18n("Symbolic Link") : i18n("File"));

                QMessageBox::information(
                    this,
                    i18n("Properties — %1", fi.fileName()),
                    details);
            });
        }
    }

    m_contextMenu->popup(view->viewport()->mapToGlobal(pos));
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

void SnapshotBrowser::navigateInternal(const QString &path, bool addToHistory)
{
    const QString cleanPath = QDir::cleanPath(path);

    // Don't navigate above the snapshot root
    if (!cleanPath.startsWith(m_rootPath) && !m_rootPath.isEmpty())
        return;

    if (addToHistory && !m_currentPath.isEmpty()) {
        m_historyBack.append(m_currentPath);
        m_historyForward.clear();
    }

    m_currentPath = cleanPath;

    const QModelIndex idx = m_fsModel->index(cleanPath);
    m_treeView->setRootIndex(idx);
    m_listView->setRootIndex(idx);

    updateBreadcrumbs();
    updateNavigationButtons();

    Q_EMIT pathChanged(m_currentPath);
}

void SnapshotBrowser::updateBreadcrumbs()
{
    // Clear existing breadcrumb buttons
    QLayoutItem *item = nullptr;
    while ((item = m_breadcrumbLayout->takeAt(0)) != nullptr) {
        delete item->widget();
        delete item;
    }

    if (m_rootPath.isEmpty() || m_currentPath.isEmpty()) {
        m_breadcrumbLayout->addStretch(1);
        return;
    }

    // Build path segments from root to current
    QString relativePath = m_currentPath;
    if (relativePath.startsWith(m_rootPath)) {
        relativePath = relativePath.mid(m_rootPath.length());
    }
    if (relativePath.startsWith(QLatin1Char('/'))) {
        relativePath = relativePath.mid(1);
    }

    // Root segment
    auto *rootButton = new QPushButton(
        QIcon::fromTheme(QStringLiteral("folder-root")),
        QDir(m_rootPath).dirName(),
        m_breadcrumbContainer);
    rootButton->setFlat(true);
    rootButton->setToolTip(i18n("Navigate to snapshot root: %1", m_rootPath));
    rootButton->setCursor(Qt::PointingHandCursor);
    connect(rootButton, &QPushButton::clicked, this, [this]() {
        navigateInternal(m_rootPath, true);
    });
    m_breadcrumbLayout->addWidget(rootButton);

    if (!relativePath.isEmpty()) {
        const QStringList segments = relativePath.split(QLatin1Char('/'), Qt::SkipEmptyParts);
        QString accumulatedPath = m_rootPath;

        for (const QString &segment : segments) {
            // Separator
            auto *sepLabel = new QPushButton(QStringLiteral(">"), m_breadcrumbContainer);
            sepLabel->setFlat(true);
            sepLabel->setEnabled(false);
            sepLabel->setFixedWidth(20);
            m_breadcrumbLayout->addWidget(sepLabel);

            accumulatedPath += QLatin1Char('/') + segment;
            const QString targetPath = accumulatedPath;

            auto *segButton = new QPushButton(segment, m_breadcrumbContainer);
            segButton->setFlat(true);
            segButton->setToolTip(i18n("Navigate to: %1", targetPath));
            segButton->setCursor(Qt::PointingHandCursor);
            connect(segButton, &QPushButton::clicked, this, [this, targetPath]() {
                navigateInternal(targetPath, true);
            });
            m_breadcrumbLayout->addWidget(segButton);
        }
    }

    m_breadcrumbLayout->addStretch(1);
}

void SnapshotBrowser::updateNavigationButtons()
{
    m_backButton->setEnabled(!m_historyBack.isEmpty());
    m_forwardButton->setEnabled(!m_historyForward.isEmpty());
    m_upButton->setEnabled(!m_currentPath.isEmpty() && m_currentPath != m_rootPath);
}

QStringList SnapshotBrowser::selectedPaths() const
{
    QStringList paths;

    const QAbstractItemView *view = (m_viewStack->currentIndex() == 0)
        ? static_cast<const QAbstractItemView *>(m_treeView)
        : static_cast<const QAbstractItemView *>(m_listView);

    const QModelIndexList selection = view->selectionModel()->selectedIndexes();
    QSet<int> seenRows;

    for (const QModelIndex &idx : selection) {
        // Only process column 0 to avoid duplicates from multi-column selection
        if (idx.column() != 0)
            continue;
        if (seenRows.contains(idx.row()))
            continue;
        seenRows.insert(idx.row());

        const QString filePath = m_fsModel->filePath(idx);
        if (!filePath.isEmpty()) {
            paths.append(filePath);
        }
    }

    return paths;
}
