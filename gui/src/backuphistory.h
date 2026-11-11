#pragma once
#include <QWidget>

#include <optional>

class QJsonValue;
class QPushButton;
class QTableView;
class QSortFilterProxyModel;
class DBusClient;
class BackupHistoryModel;

class BackupHistoryView : public QWidget
{
    Q_OBJECT
    // How many of the newest runs the view asks the helper for.
    Q_PROPERTY(qint64 historyLimit READ historyLimit)
public:
    explicit BackupHistoryView(DBusClient *client, QWidget *parent = nullptr);

    // A snapshot count from the helper's history JSON: the number, or no
    // value when the run could not count it (JSON null) or the field is
    // missing — never 0, which would read as nothing done, and never a
    // negative sentinel (bd DAS-Backup-Manager-6wt).
    [[nodiscard]] static std::optional<qint64> countFromJson(const QJsonValue &value);

    // Show a history as the helper returns it (IndexBackupHistory JSON).
    void showHistory(const QString &json);

    [[nodiscard]] qint64 historyLimit() const { return m_limit; }

public Q_SLOTS:
    void refresh();
    // Ask for the next page of older runs (bd DAS-Backup-Manager-5237.5).
    void loadOlder();

private:
    DBusClient *m_client;
    BackupHistoryModel *m_model = nullptr;
    QSortFilterProxyModel *m_proxy = nullptr;
    QTableView *m_view = nullptr;
    QPushButton *m_loadOlder = nullptr;
    qint64 m_limit = PageSize;

    static constexpr qint64 PageSize = 50;
};
