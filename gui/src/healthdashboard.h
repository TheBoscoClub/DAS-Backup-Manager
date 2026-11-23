#pragma once
#include <QDateTime>
#include <QWidget>

class QTabWidget;
class QTableView;
class QLabel;
class QSplitter;
class QChartView;
class QPushButton;
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
    void openRaidAccept();
    void onScrubAction(const QString &action, bool ok, const QString &text);
    void onRefreshFailed(const QString &operation, const QString &reason);

private:
    void setupDrivesTab();
    void setupGrowthTab();
    void setupStatusTab();
    void updateDrives(const QString &json);
    void updateGrowth(const QString &json);
    void updateStatus(const QString &json);
    void updateScrubButtons();

    DBusClient *m_client;
    QString m_configPath;
    QTabWidget *m_tabs = nullptr;
    QLabel *m_note = nullptr;
    QDateTime m_lastGood;

    // Drives tab
    QTableView *m_drivesView = nullptr;
    // Opens the RAID-1 accept for the selected row's target (bd
    // DAS-Backup-Manager-su1b.11).
    QPushButton *m_raidAcceptButton = nullptr;
    // Pause / resume the scrub pass (bd DAS-Backup-Manager-su1b.18). Pause is
    // offered while a pass runs, resume while a filesystem is paused; both
    // are off while a call is in flight.
    QPushButton *m_scrubPauseButton = nullptr;
    QPushButton *m_scrubResumeButton = nullptr;
    QLabel *m_scrubResult = nullptr;
    bool m_scrubRunning = false;
    bool m_scrubPaused = false;
    bool m_scrubBusy = false;

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
