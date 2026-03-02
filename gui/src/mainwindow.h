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

class KStatusNotifierItem;

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
    void onBackupFinished(const QString &jobId, bool success, const QString &summary);

    // Async status bar result handlers
    void onIndexStatsResult(const QString &json);
    void onScheduleGetResult(const QString &json);
    void onHealthQueryResult(const QString &json);

private:
    void setupActions();
    void setupUi();
    void setupBrowsePage();
    void setupTrayIcon();
    void assembleStatusBar();

    // Core services
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
    QTimer *m_statusTimer = nullptr;

    // Tray icon
    KStatusNotifierItem *m_trayIcon = nullptr;

    // Pages
    QWidget *m_browsePage = nullptr;
    BackupPanel *m_backupRunPage = nullptr;
    BackupHistoryView *m_backupHistoryPage = nullptr;
    QWidget *m_configPage = nullptr;
    HealthDashboard *m_healthDashboard = nullptr;

    QString m_dbPath;
    qint64 m_currentSnapshotId = -1;

    // Async status bar state (collected as D-Bus results arrive)
    struct StatusBarState {
        QString statsJson;
        QString scheduleJson;
        QString healthJson;
        int pending = 0;
    } m_statusState;
};
