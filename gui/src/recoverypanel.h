#pragma once

#include "recoverystatus.h"

#include <QSet>
#include <QWidget>

#include <optional>

class QCheckBox;
class QComboBox;
class QDateTimeEdit;
class QGroupBox;
class QLabel;
class QPushButton;
class QRadioButton;
class QTimer;
class QVBoxLayout;
class DBusClient;

// Recovery drives: one card per drive from the helper's status document, a
// selector (one drive, or both), attended/unattended, one after the other
// or in parallel for both, Now or a date and time, and Upgrade / Schedule /
// Clear schedule / Open console / End session. The widget only binds: every
// rule lives in recoverystatus.{h,cpp} (bd DAS-Backup-Manager-8249 stage 3).
class RecoveryPanel : public QWidget
{
    Q_OBJECT

public:
    explicit RecoveryPanel(DBusClient *client, QWidget *parent = nullptr);

    // Test seams: the document as the helper would send it; the line under
    // the cards (parse errors, job outcomes); what the window knows.
    void applyDocument(const QByteArray &json);
    [[nodiscard]] QString statusLine() const;
    [[nodiscard]] GuiFacts facts() const;

public Q_SLOTS:
    void refresh();            // one RecoveryOsStatus in flight at a time
    void setShown(bool shown); // starts/stops the 30 s refresh timer
    void onJobStarted(const QString &jobId, const QString &operation);
    void onJobFinished(const QString &jobId, bool success, const QString &summary);

private Q_SLOTS:
    void onStatusResult(const QString &json);
    void onSessionEndResult(const QString &label, bool ok, const QString &lines);
    void onScheduleResult(const QString &unit);
    void onConsoleResult(const QString &label, const QString &path);
    void onUpgrade();
    void onSchedule();
    void onClearSchedule();
    void onConsole();
    void onEndSession();
    void rederive();

private:
    void buildControls();
    void rebuildCards();
    [[nodiscard]] QStringList selectedLabels() const; // one label, or both in document order
    [[nodiscard]] bool selectionIsBoth() const;
    [[nodiscard]] const DriveView *selectedDrive() const; // nullptr for "both"
    [[nodiscard]] QString chosenMode() const;             // sequential | parallel | "" (one drive)

    DBusClient *m_client;
    std::optional<RecoveryDocument> m_doc;
    QString m_parseError;
    QVBoxLayout *m_cards = nullptr;
    QList<QGroupBox *> m_cardWidgets;
    QComboBox *m_selector = nullptr;
    QRadioButton *m_attended = nullptr;
    QRadioButton *m_unattended = nullptr;
    QRadioButton *m_sequential = nullptr;
    QRadioButton *m_parallel = nullptr;
    QCheckBox *m_now = nullptr;
    QDateTimeEdit *m_when = nullptr;
    QPushButton *m_upgrade = nullptr;
    QPushButton *m_schedule = nullptr;
    QPushButton *m_clear = nullptr;
    QPushButton *m_console = nullptr;
    QPushButton *m_end = nullptr;
    QLabel *m_status = nullptr;
    QTimer *m_timer = nullptr;
    bool m_refreshInFlight = false;
    QString m_ownJobId;          // this panel's running session job; empty until jobStarted names it
    bool m_ownJobAttended = false;
    bool m_jobRequested = false; // Upgrade clicked, jobStarted not yet seen
    QSet<QString> m_earlyFinished; // JobFinished ids seen before jobStarted named the job
    bool m_endInFlight = false;
};
