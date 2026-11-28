#pragma once
#include "growthmath.h"

#include <QDateTime>
#include <QSet>
#include <QStringList>
#include <QVector>
#include <QWidget>

class QTabWidget;
class QTableView;
class QLabel;
class QSplitter;
class QPushButton;
class QCheckBox;
class DBusClient;
class GrowthChartView;

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
    // Rebuild the growth chart and table from m_growthSeries, then range them.
    void renderGrowth();
    // Range the date axis to m_growthRange and the value axis to what is
    // visible in it.
    void applyGrowthRange();
    void chooseGrowthPreset(GrowthMath::Preset preset);
    // The span the chart may zoom out and pan over: the data shown, or the
    // preset, whichever is wider.
    [[nodiscard]] GrowthMath::Range growthLimit() const;
    // The range `preset` shows over the series shown.
    [[nodiscard]] GrowthMath::Range growthPresetRange(GrowthMath::Preset preset) const;
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

    // Growth tab (bd DAS-Backup-Manager-3gzs.15)
    struct GrowthSeries {
        QString label;
        bool retired = false;
        QVector<GrowthMath::Reading> readings; // date order
    };
    QWidget *m_growthWidget = nullptr;
    QSplitter *m_growthSplitter = nullptr;
    GrowthChartView *m_chartView = nullptr;
    QTableView *m_growthView = nullptr;
    QCheckBox *m_showRetired = nullptr;
    QVector<GrowthSeries> m_growthSeries;
    GrowthMath::Preset m_growthPreset = GrowthMath::Preset::Days90;
    GrowthMath::Range m_growthRange;
    // Zoomed or panned by hand since the preset was chosen: a refresh keeps
    // the view, not the preset's.
    bool m_growthMoved = false;
    bool m_growthHaveRange = false;
    // Backup jobs this GUI started; the tab refreshes when one finishes.
    QSet<QString> m_backupJobs;

    // Status tab
    QLabel *m_btrbkLabel = nullptr;
    QLabel *m_timerLabel = nullptr;
    QLabel *m_lastBackupLabel = nullptr;
    QLabel *m_mountLabel = nullptr;
};
