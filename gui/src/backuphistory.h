#pragma once
#include <QWidget>

#include <optional>

class QJsonValue;
class QTableView;
class QSortFilterProxyModel;
class DBusClient;
class BackupHistoryModel;

class BackupHistoryView : public QWidget
{
    Q_OBJECT
public:
    explicit BackupHistoryView(DBusClient *client, QWidget *parent = nullptr);

    // A snapshot count from the helper's history JSON: the number, or no
    // value when the run could not count it (JSON null) or the field is
    // missing — never 0, which would read as nothing done, and never a
    // negative sentinel (bd DAS-Backup-Manager-6wt).
    [[nodiscard]] static std::optional<qint64> countFromJson(const QJsonValue &value);

    // Show a history as the helper returns it (IndexBackupHistory JSON).
    void showHistory(const QString &json);

public Q_SLOTS:
    void refresh();

private:
    DBusClient *m_client;
    BackupHistoryModel *m_model = nullptr;
    QSortFilterProxyModel *m_proxy = nullptr;
    QTableView *m_view = nullptr;
};
