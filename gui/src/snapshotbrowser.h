#pragma once
#include <QWidget>

class QAction;
class QFileSystemModel;
class QHBoxLayout;
class QLineEdit;
class QListView;
class QMenu;
class QStackedWidget;
class QToolButton;
class QTreeView;
class DBusClient;

class SnapshotBrowser : public QWidget
{
    Q_OBJECT

public:
    explicit SnapshotBrowser(DBusClient *client, QWidget *parent = nullptr);

    void setSnapshotPath(const QString &path);
    void navigateToPath(const QString &path);

Q_SIGNALS:
    void restoreRequested(const QStringList &sourcePaths);
    void pathChanged(const QString &currentPath);

private Q_SLOTS:
    void goBack();
    void goForward();
    void goUp();
    void onItemActivated(const QModelIndex &index);
    void showContextMenu(const QPoint &pos);
    void toggleFilterBar();
    void applyNameFilter(const QString &pattern);
    void setDetailMode();
    void setIconMode();

private:
    void setupNavigationBar();
    void setupBrowserViews();
    void setupFilterBar();
    void setupActions();
    void updateBreadcrumbs();
    void navigateInternal(const QString &path, bool addToHistory);
    void updateNavigationButtons();
    QStringList selectedPaths() const;

    DBusClient *m_client;

    // Navigation
    QToolButton *m_backButton = nullptr;
    QToolButton *m_forwardButton = nullptr;
    QToolButton *m_upButton = nullptr;
    QHBoxLayout *m_breadcrumbLayout = nullptr;
    QWidget *m_breadcrumbContainer = nullptr;
    QList<QString> m_historyBack;
    QList<QString> m_historyForward;
    QString m_rootPath;
    QString m_currentPath;

    // View mode
    QToolButton *m_detailModeButton = nullptr;
    QToolButton *m_iconModeButton = nullptr;
    QStackedWidget *m_viewStack = nullptr;
    QTreeView *m_treeView = nullptr;
    QListView *m_listView = nullptr;
    QFileSystemModel *m_fsModel = nullptr;

    // Filter
    QWidget *m_filterBar = nullptr;
    QLineEdit *m_filterEdit = nullptr;
    QAction *m_filterAction = nullptr;

    // Context menu
    QMenu *m_contextMenu = nullptr;
};
