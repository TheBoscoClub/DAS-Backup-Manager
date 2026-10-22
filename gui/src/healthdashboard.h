#pragma once
#include <QDateTime>
#include <QWidget>

class QTabWidget;
class QTableView;
class QLabel;
class QSplitter;
class QChartView;
class DBusClient;

class HealthDashboard : public QWidget
{
    Q_OBJECT
public:
    explicit HealthDashboard(DBusClient *client, QWidget *parent = nullptr);

    void setActiveTab(int index);

    // Empty while the last refresh succeeded; otherwise why it failed and
    // when the reading on screen was taken (bd DAS-Backup-Manager-amsg).
    [[nodiscard]] QString statusNote() const;

public Q_SLOTS:
    void refresh();

private Q_SLOTS:
    void onHealthResult(const QString &json);
    void onRefreshFailed(const QString &operation, const QString &reason);

private:
    void setupDrivesTab();
    void setupGrowthTab();
    void setupStatusTab();
    void updateDrives(const QString &json);
    void updateGrowth(const QString &json);
    void updateStatus(const QString &json);

    DBusClient *m_client;
    QString m_configPath;
    QTabWidget *m_tabs = nullptr;
    QLabel *m_note = nullptr;
    QDateTime m_lastGood;

    // Drives tab
    QTableView *m_drivesView = nullptr;

    // Growth tab
    QWidget *m_growthWidget = nullptr;
    QSplitter *m_growthSplitter = nullptr;
    QChartView *m_chartView = nullptr;
    QTableView *m_growthView = nullptr;

    // Status tab
    QLabel *m_btrbkLabel = nullptr;
    QLabel *m_timerLabel = nullptr;
    QLabel *m_lastBackupLabel = nullptr;
    QLabel *m_mountLabel = nullptr;
};
