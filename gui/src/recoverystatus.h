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
