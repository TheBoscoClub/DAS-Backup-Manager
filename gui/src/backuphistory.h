#pragma once
#include <QWidget>

class QTableView;
class QSortFilterProxyModel;
class DBusClient;
class BackupHistoryModel;

class BackupHistoryView : public QWidget
{
    Q_OBJECT
public:
    explicit BackupHistoryView(DBusClient *client, const QString &dbPath, QWidget *parent = nullptr);

public Q_SLOTS:
    void refresh();

private:
    DBusClient *m_client;
    QString m_dbPath;
    BackupHistoryModel *m_model = nullptr;
    QSortFilterProxyModel *m_proxy = nullptr;
    QTableView *m_view = nullptr;
};
