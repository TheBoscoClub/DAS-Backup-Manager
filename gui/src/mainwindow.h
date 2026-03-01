#pragma once

#include <KXmlGuiWindow>

#include "sidebar.h"

class QStackedWidget;
class QSplitter;
class QTableView;
class QLineEdit;
class QSortFilterProxyModel;
class QLabel;
class QTimer;

class Database;
class SnapshotModel;
class SnapshotTimeline;
class FileModel;
class SearchModel;
class IndexRunner;
class RestoreAction;
class SettingsDialog;
class SnapshotWatcher;
class DBusClient;
class ProgressPanel;
class BackupHistoryView;
class BackupPanel;
class HealthDashboard;
class ConfigDialog;

class MainWindow : public KXmlGuiWindow
{
    Q_OBJECT

public:
    explicit MainWindow(const QString &dbPath, QWidget *parent = nullptr);
    ~MainWindow() override;

private Q_SLOTS:
    void onSectionChanged(SidebarSection section);
    void onSnapshotSelected(qint64 snapshotId);
    void onSearchTextChanged(const QString &text);
    void executeSearch();
    void triggerReindex();
    void showStats();
    void updateStatusBar();
    void restoreSelectedFiles();
    void showSettings();

private:
    void setupActions();
    void setupUi();
    void setupBrowsePage();
    void openDatabase(const QString &path);

    // Core services
    Database *m_database = nullptr;
    DBusClient *m_dbusClient = nullptr;
    IndexRunner *m_indexRunner = nullptr;
    SnapshotWatcher *m_snapshotWatcher = nullptr;
    RestoreAction *m_restoreAction = nullptr;

    // Sidebar + stack
    Sidebar *m_sidebar = nullptr;
    QStackedWidget *m_stack = nullptr;
    ProgressPanel *m_progressPanel = nullptr;

    // Browse page widgets
    SnapshotModel *m_snapshotModel = nullptr;
    SnapshotTimeline *m_timeline = nullptr;
    FileModel *m_fileModel = nullptr;
    SearchModel *m_searchModel = nullptr;
    QTableView *m_fileView = nullptr;
    QTableView *m_searchView = nullptr;
    QLineEdit *m_searchBar = nullptr;
    QTimer *m_searchTimer = nullptr;
    QSortFilterProxyModel *m_fileProxy = nullptr;

    // Status bar
    QLabel *m_statusLabel = nullptr;

    // Pages
    QWidget *m_browsePage = nullptr;
    BackupPanel *m_backupRunPage = nullptr;
    BackupHistoryView *m_backupHistoryPage = nullptr;
    QWidget *m_configPage = nullptr;
    HealthDashboard *m_healthDashboard = nullptr;

    QString m_dbPath;
    qint64 m_currentSnapshotId = -1;
};
