#pragma once

#include <QByteArray>
#include <QJsonObject>
#include <QList>
#include <QString>
#include <QStringList>

#include <optional>

// The helper's recovery-drive status document (RecoveryOsStatus, schema 1)
// as plain structs, and nothing else: no widgets, no bus. Every `*_error`
// is an optional: present means that part could not be read and its value
// is null (bd DAS-Backup-Manager-8249 stage 3, spec §3.1). The contract is
// gui/tests/fixtures/recovery-status.json, written by a panel.rs test.

struct GuestAgentView {
    QString state;
    std::optional<bool> installed;
    std::optional<bool> enabled;
    QString why;
};

struct RecordView {
    QString os;
    QString installed;
    QString lastFullUpgrade;
    QString kernel;
    QString hostKernel;
    QString btrfsProgs;
    QString hostBtrfsProgs;
    QString btrbk;
    // A null package version is "not installed" only with this true; false
    // means pacman's database could not be read and the version is unknown.
    bool packagesRead = false;
    GuestAgentView guestAgent;
};

struct AssessmentView {
    std::optional<qint64> ageDays;
    QString ageBasis;
    bool stale = false;
    QStringList reasons;
    QStringList warnings;
};

struct ScheduleView {
    QString unit;
    std::optional<qint64> atEpoch;
    QString mode;
    QString state; // pending | missed | fired | running
    QString detail;
};

struct SessionView {
    QString by; // job:<id> | unit:<name> | other:<holder line>
    std::optional<qint64> sinceEpoch;
    QString domainState;
    std::optional<bool> attended; // null for a job session: the document cannot tell
};

struct DriveView {
    QString label;
    QString displayName;
    QStringList serials;
    std::optional<qint64> checkedEpoch;
    std::optional<QString> recordError;
    std::optional<RecordView> record;
    std::optional<AssessmentView> assessment;
    bool due = false;
    QString verdict; // "will" | "may" | "no" | "" (no reading)
    bool unattendedPossible = false;
    QString unattendedWhy;
    std::optional<qint64> cleanRuns;
    std::optional<QString> cleanRunsError;
    QList<QJsonObject> history;
    std::optional<QString> historyError;
    std::optional<ScheduleView> schedule;
    std::optional<QString> scheduleError;
    std::optional<SessionView> session;
    std::optional<QString> sessionError;
};

struct PairView {
    QString modeDefault;
    std::optional<ScheduleView> schedule;
    std::optional<QString> scheduleError;
    std::optional<SessionView> session;
    std::optional<QString> sessionError;
};

struct RecoveryDocument {
    int schema = 0;
    int maxAgeDays = 0;
    QString today;
    PairView pair;
    QList<DriveView> drives;

    // Null on failure; `error` names the problem ("not JSON", "schema 2",
    // "drives[1] lacks unattended"). A document missing a key this schema
    // has is refused whole: parsing on would show "unknown" for what is
    // really "absent".
    static std::optional<RecoveryDocument> parse(const QByteArray &json, QString *error);
};

// --- The enablement rules (spec §4) ----------------------------------------

// What only the window knows.
struct GuiFacts {
    QString ownJobId;             // this window's running session job, empty if none
    bool ownJobAttended = false;  // how this window started that job
    bool viewerInstalled = false; // TigerVNC's vncviewer found on PATH
    qint64 nowEpoch = 0;
    bool chosenNow = true;        // the Now checkbox
    qint64 chosenEpoch = 0;       // the date-time picker
    bool unattended = false;      // the radio
};

// `why` is the tooltip in both states: what the button does when enabled,
// why not when disabled.
struct Action {
    bool enabled = false;
    QString why;
};

struct DriveActions {
    Action upgrade; // for the chosen attended/unattended radio
    Action schedule;
    Action clearSchedule;
    Action console;
    Action endSession;
    bool bannerNeeded = false; // attended on a `will` record: show the banner before the call
};

struct PairActions {
    Action upgrade;
    Action schedule;
    Action clearSchedule;
    QString modeDefault;
};

constexpr qint64 ScheduleMinLeadSeconds = 120; // panel.rs SCHEDULE_MIN_LEAD

// The cautious side wins: the first failing condition's words are the tooltip.
// The document form also refuses Upgrade while ANY drive's session runs or
// cannot be read (one maintenance lock for all); the pair form knows only
// the drive and the pair.
DriveActions deriveActions(const DriveView &d, const RecoveryDocument &doc, const GuiFacts &g);
DriveActions deriveActions(const DriveView &d, const PairView &p, const GuiFacts &g);
PairActions derivePairActions(const RecoveryDocument &doc, const GuiFacts &g);

// Words for the cards.
QString ageWords(std::optional<qint64> epoch, qint64 nowEpoch); // "3 days ago" | "today" | "unknown"
QString verdictWords(const QString &verdict);                   // will | may | no | "" → unknown
QString sessionWords(const SessionView &s, qint64 nowEpoch);
QString scheduleWords(const ScheduleView &s); // "<state>: <detail>"
