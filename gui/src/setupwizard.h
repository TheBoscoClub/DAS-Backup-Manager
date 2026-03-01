#pragma once
#include <QWizard>

class QListWidget;
class QRadioButton;
class QTimeEdit;
class QLabel;
class DBusClient;

class SetupWizard : public QWizard
{
    Q_OBJECT
public:
    explicit SetupWizard(DBusClient *client, QWidget *parent = nullptr);

    /// Returns true when no btrbk configuration exists (or the file is empty).
    [[nodiscard]] static bool needsSetup();

Q_SIGNALS:
    void setupComplete();

private:
    void buildWelcomePage();
    void buildSourcePage();
    void buildTargetPage();
    void buildSchedulePage();
    void buildSummaryPage();

    void applyConfiguration();

    /// Scan /proc/mounts for btrfs mount points.
    [[nodiscard]] static QStringList detectBtrfsMounts();
    /// Scan /proc/mounts for non-root block device mount points.
    [[nodiscard]] static QStringList detectTargetMounts();

    /// Build the btrbk.conf text from the current wizard selections.
    [[nodiscard]] QString generateConfig() const;

    DBusClient *m_client;

    // Page 2 — Source Selection
    QListWidget *m_sourceList = nullptr;

    // Page 3 — Target Selection
    QListWidget *m_targetList = nullptr;

    // Page 4 — Schedule
    QRadioButton *m_dailyRadio = nullptr;
    QRadioButton *m_weeklyRadio = nullptr;
    QRadioButton *m_manualRadio = nullptr;
    QTimeEdit *m_timeEdit = nullptr;

    // Page 5 — Summary
    QLabel *m_summaryLabel = nullptr;

    // Page IDs for initializePage()
    enum PageId {
        Page_Welcome = 0,
        Page_Source,
        Page_Target,
        Page_Schedule,
        Page_Summary
    };
};
