#pragma once
#include <QWidget>

class QTabWidget;
class QTableView;
class QLabel;
class DBusClient;

class HealthDashboard : public QWidget
{
    Q_OBJECT
public:
    explicit HealthDashboard(DBusClient *client, QWidget *parent = nullptr);

    void setActiveTab(int index);

public Q_SLOTS:
    void refresh();

private Q_SLOTS:
    void onHealthResult(const QString &json);

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

    // Drives tab
    QTableView *m_drivesView = nullptr;

    // Growth tab
    QWidget *m_growthWidget = nullptr;
    QTableView *m_growthView = nullptr;

    // Status tab
    QLabel *m_btrbkLabel = nullptr;
    QLabel *m_timerLabel = nullptr;
    QLabel *m_lastBackupLabel = nullptr;
    QLabel *m_mountLabel = nullptr;
};
