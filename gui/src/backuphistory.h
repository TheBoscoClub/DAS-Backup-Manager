#pragma once
#include <QWidget>

class QTableView;
class QSortFilterProxyModel;
class Database;
class DBusClient;
class BackupHistoryModel;

class BackupHistoryView : public QWidget
{
    Q_OBJECT
public:
    explicit BackupHistoryView(Database *db, DBusClient *client, QWidget *parent = nullptr);

public Q_SLOTS:
    void refresh();

private:
    Database *m_database;
    DBusClient *m_client;
    BackupHistoryModel *m_model = nullptr;
    QSortFilterProxyModel *m_proxy = nullptr;
    QTableView *m_view = nullptr;
};
