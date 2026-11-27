#pragma once

#include "recoverystatus.h"

#include <QHash>
#include <QWidget>

#include <functional>
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
    // JobFinished results held until the method's reply names their job.
    [[nodiscard]] int earlyFinishedCount() const;
    // The confirmation a `will` banner asks for: (title, text) -> yes. A
    // QMessageBox by default; tests replace it, since a modal cannot be
    // answered headlessly.
    using Confirmer = std::function<bool(const QString &title, const QString &text)>;
    void setConfirmer(Confirmer confirmer);

public Q_SLOTS:
    void refresh();            // one RecoveryOsStatus in flight at a time
    void setShown(bool shown); // starts/stops the 30 s refresh timer
    void onJobStarted(const QString &jobId, const QString &operation);
    void onJobFinished(const QString &jobId, bool success, const QString &summary);

private Q_SLOTS:
    void onStatusResult(const QString &json);
    void onStatusError(const QString &reason);
    void onSessionEndResult(const QString &label, bool ok, const QString &lines);
    void onScheduleResult(const QString &unit);
    void onConsoleResult(const QString &label, const QString &path);
    void onUpgrade();
    void onSchedule();
    void onClearSchedule();
    void onConsole();
    void onEndSession();
    void onRecord();
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
    QString m_staleReason; // the last status refresh failed: the cards stand, the buttons do not
    Confirmer m_confirm;
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
    QPushButton *m_record = nullptr;
    QLabel *m_status = nullptr;
    // What the status line says, as plain text: what it was before it was
    // structured, kept for the tests and for the comparisons that clear it.
    QString m_statusPlain;
    void setStatus(const QString &plain);
    void setOutcome(const OutcomeText &outcome);
    QTimer *m_timer = nullptr;
    bool m_refreshInFlight = false;
    bool m_scheduleClearing = false; // the last RecoveryOsScheduleSet sent was a clear (time 0)
    QString m_ownJobId;          // this panel's running session job; empty until jobStarted names it
    bool m_ownJobAttended = false;
    QStringList m_ownJobLabels;  // the drives this panel's session job was started for
    QStringList m_keptOwnLabels; // of a job that ended "kept": its session runs on (uk3v.10)
    bool m_keptOwnAttended = false;
    bool m_jobRequested = false; // Upgrade clicked, jobStarted not yet seen
    struct EarlyFinish {
        bool success = false;
        QString summary;
    };
    // JobFinished seen while a call's reply is pending, by job id, until the reply
    // names the job; emptied when the request resolves (reply, refusal, drop).
    QHash<QString, EarlyFinish> m_earlyFinished;
    void showRecordOutcome(bool success, const QString &summary);
    void showSessionOutcome(bool success, const QString &summary);
    bool m_endInFlight = false;
    QString m_ownRecordJobId;       // this panel's running record job; empty until jobStarted names it
    bool m_recordRequested = false; // Record clicked, jobStarted not yet seen
};
