#pragma once

#include <KXmlGuiWindow>

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

class MainWindow : public KXmlGuiWindow
{
    Q_OBJECT

public:
    explicit MainWindow(const QString &dbPath, QWidget *parent = nullptr);
    ~MainWindow() override;

private Q_SLOTS:
    void onSnapshotSelected(qint64 snapshotId);
    void onSearchTextChanged(const QString &text);
    void executeSearch();
    void triggerReindex();
    void showStats();
    void updateStatusBar();

private:
    void setupActions();
    void setupUi();
    void openDatabase(const QString &path);

    Database *m_database = nullptr;
    SnapshotModel *m_snapshotModel = nullptr;
    SnapshotTimeline *m_timeline = nullptr;
    FileModel *m_fileModel = nullptr;
    SearchModel *m_searchModel = nullptr;
    IndexRunner *m_indexRunner = nullptr;

    QTableView *m_fileView = nullptr;
    QTableView *m_searchView = nullptr;
    QLineEdit *m_searchBar = nullptr;
    QTimer *m_searchTimer = nullptr;
    QSortFilterProxyModel *m_fileProxy = nullptr;
    QLabel *m_statusLabel = nullptr;

    QString m_dbPath;
};
