#include "recoverystatus.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonParseError>
#include <QJsonValue>

namespace {

// A key that must exist (null allowed) — a document without it is another
// helper's.
bool need(const QJsonObject &o, const char *key, const QString &where, QString *error)
{
    if (o.contains(QLatin1String(key)))
        return true;
    if (error)
        *error = QStringLiteral("%1 lacks %2").arg(where, QLatin1String(key));
    return false;
}

std::optional<QString> optString(const QJsonValue &v)
{
    return v.isString() ? std::optional<QString>(v.toString()) : std::nullopt;
}

std::optional<qint64> optInt(const QJsonValue &v)
{
    return v.isDouble() ? std::optional<qint64>(v.toInteger()) : std::nullopt;
}

std::optional<bool> optBool(const QJsonValue &v)
{
    return v.isBool() ? std::optional<bool>(v.toBool()) : std::nullopt;
}

QStringList strings(const QJsonValue &v)
{
    QStringList out;
    const QJsonArray a = v.toArray();
    for (const QJsonValue &s : a)
        out << s.toString();
    return out;
}

std::optional<ScheduleView> schedule(const QJsonValue &v)
{
    if (!v.isObject())
        return std::nullopt;
    const QJsonObject o = v.toObject();
    return ScheduleView{o[QStringLiteral("unit")].toString(), optInt(o[QStringLiteral("at_epoch")]),
                        o[QStringLiteral("mode")].toString(), o[QStringLiteral("state")].toString(),
                        o[QStringLiteral("detail")].toString()};
}

std::optional<SessionView> session(const QJsonValue &v)
{
    if (!v.isObject())
        return std::nullopt;
    const QJsonObject o = v.toObject();
    return SessionView{o[QStringLiteral("by")].toString(), optInt(o[QStringLiteral("since_epoch")]),
                       o[QStringLiteral("domain_state")].toString(), optBool(o[QStringLiteral("attended")])};
}

std::optional<RecordView> record(const QJsonValue &v)
{
    if (!v.isObject())
        return std::nullopt;
    const QJsonObject o = v.toObject();
    RecordView r;
    r.os = o[QStringLiteral("os")].toString();
    r.installed = o[QStringLiteral("installed")].toString();
    r.lastFullUpgrade = o[QStringLiteral("last_full_upgrade")].toString();
    r.kernel = o[QStringLiteral("kernel")].toString();
    r.hostKernel = o[QStringLiteral("host_kernel")].toString();
    r.btrfsProgs = o[QStringLiteral("btrfs_progs")].toString();
    r.hostBtrfsProgs = o[QStringLiteral("host_btrfs_progs")].toString();
    r.btrbk = o[QStringLiteral("btrbk")].toString();
    r.packagesRead = o[QStringLiteral("packages_read")].toBool();
    const QJsonObject g = o[QStringLiteral("guest_agent")].toObject();
    r.guestAgent = GuestAgentView{g[QStringLiteral("state")].toString(), optBool(g[QStringLiteral("installed")]),
                                  optBool(g[QStringLiteral("enabled")]), g[QStringLiteral("why")].toString()};
    return r;
}

std::optional<AssessmentView> assessment(const QJsonValue &v)
{
    if (!v.isObject())
        return std::nullopt;
    const QJsonObject o = v.toObject();
    return AssessmentView{optInt(o[QStringLiteral("age_days")]), o[QStringLiteral("age_basis")].toString(),
                          o[QStringLiteral("stale")].toBool(), strings(o[QStringLiteral("reasons")]),
                          strings(o[QStringLiteral("warnings")])};
}

const char *const DriveKeys[] = {
    "label",        "display_name",     "serials",       "checked_epoch", "record_error",
    "record",       "assessment",       "due",           "verdict",       "unattended",
    "clean_runs",   "clean_runs_error", "history",       "history_error", "schedule",
    "schedule_error", "session",        "session_error",
};

std::optional<DriveView> drive(const QJsonValue &v, qsizetype index, QString *error)
{
    const QString where = QStringLiteral("drives[%1]").arg(index);
    if (!v.isObject()) {
        if (error)
            *error = where + QStringLiteral(" is not an object");
        return std::nullopt;
    }
    const QJsonObject o = v.toObject();
    for (const char *key : DriveKeys)
        if (!need(o, key, where, error))
            return std::nullopt;
    DriveView d;
    d.label = o[QStringLiteral("label")].toString();
    d.displayName = o[QStringLiteral("display_name")].toString();
    d.serials = strings(o[QStringLiteral("serials")]);
    d.checkedEpoch = optInt(o[QStringLiteral("checked_epoch")]);
    d.recordError = optString(o[QStringLiteral("record_error")]);
    d.record = record(o[QStringLiteral("record")]);
    d.assessment = assessment(o[QStringLiteral("assessment")]);
    d.due = o[QStringLiteral("due")].toBool();
    d.verdict = o[QStringLiteral("verdict")].toString();
    const QJsonObject u = o[QStringLiteral("unattended")].toObject();
    d.unattendedPossible = u[QStringLiteral("possible")].toBool();
    d.unattendedWhy = u[QStringLiteral("why")].toString();
    d.cleanRuns = optInt(o[QStringLiteral("clean_runs")]);
    d.cleanRunsError = optString(o[QStringLiteral("clean_runs_error")]);
    const QJsonArray history = o[QStringLiteral("history")].toArray();
    for (const QJsonValue &h : history)
        d.history << h.toObject();
    d.historyError = optString(o[QStringLiteral("history_error")]);
    d.schedule = schedule(o[QStringLiteral("schedule")]);
    d.scheduleError = optString(o[QStringLiteral("schedule_error")]);
    d.session = session(o[QStringLiteral("session")]);
    d.sessionError = optString(o[QStringLiteral("session_error")]);
    return d;
}

} // namespace

std::optional<RecoveryDocument> RecoveryDocument::parse(const QByteArray &json, QString *error)
{
    QJsonParseError pe;
    const QJsonDocument doc = QJsonDocument::fromJson(json, &pe);
    if (doc.isNull() || !doc.isObject()) {
        if (error)
            *error = QStringLiteral("the status document is not JSON: %1").arg(pe.errorString());
        return std::nullopt;
    }
    const QJsonObject o = doc.object();
    const QString where = QStringLiteral("the status document");
    // The schema first: another schema's document is refused by its number,
    // not by the first key this schema has and it lacks.
    if (!need(o, "schema", where, error))
        return std::nullopt;
    RecoveryDocument d;
    d.schema = o[QStringLiteral("schema")].toInt();
    if (d.schema != 1) {
        if (error)
            *error = QStringLiteral("the status document is schema %1, this GUI reads schema 1").arg(d.schema);
        return std::nullopt;
    }
    for (const char *key : {"max_age_days", "today", "pair", "drives"})
        if (!need(o, key, where, error))
            return std::nullopt;
    d.maxAgeDays = o[QStringLiteral("max_age_days")].toInt();
    d.today = o[QStringLiteral("today")].toString();
    const QJsonObject p = o[QStringLiteral("pair")].toObject();
    for (const char *key : {"mode_default", "schedule", "schedule_error", "session", "session_error"})
        if (!need(p, key, QStringLiteral("pair"), error))
            return std::nullopt;
    d.pair.modeDefault = p[QStringLiteral("mode_default")].toString();
    d.pair.schedule = schedule(p[QStringLiteral("schedule")]);
    d.pair.scheduleError = optString(p[QStringLiteral("schedule_error")]);
    d.pair.session = session(p[QStringLiteral("session")]);
    d.pair.sessionError = optString(p[QStringLiteral("session_error")]);
    const QJsonArray drives = o[QStringLiteral("drives")].toArray();
    for (qsizetype i = 0; i < drives.size(); ++i) {
        auto dv = drive(drives[i], i, error);
        if (!dv)
            return std::nullopt;
        d.drives << *dv;
    }
    return d;
}

// --- The enablement rules ---------------------------------------------------

namespace {

Action allowed(const QString &does)
{
    return {true, does};
}

Action refused(const QString &why)
{
    return {false, why};
}

// The first failing condition's words, or empty when all pass. Order is the
// spec's §4 table: the cautious side wins, the most specific reason first.
QString sessionBlocker(const DriveView &d, const PairView &p, const GuiFacts &g)
{
    if (d.sessionError)
        return *d.sessionError;
    if (p.sessionError)
        return *p.sessionError;
    if (d.session)
        return QStringLiteral("a session holds %1 (%2)").arg(d.displayName, sessionWords(*d.session, g.nowEpoch));
    if (p.session)
        return QStringLiteral("a session holds both drives (%1)").arg(sessionWords(*p.session, g.nowEpoch));
    if (!g.ownJobId.isEmpty())
        return QStringLiteral("this window's session %1 is running").arg(g.ownJobId);
    return {};
}

QString unattendedBlocker(const DriveView &d)
{
    if (!d.unattendedPossible)
        return d.unattendedWhy.isEmpty() ? QStringLiteral("the record does not admit an unattended session")
                                         : d.unattendedWhy;
    return {};
}

QString timeBlocker(const GuiFacts &g)
{
    if (g.chosenNow)
        return QStringLiteral("untick Now to schedule a time");
    if (g.chosenEpoch < g.nowEpoch + ScheduleMinLeadSeconds)
        return QStringLiteral("pick a time at least 2 minutes ahead");
    return {};
}

bool clearable(const std::optional<ScheduleView> &s)
{
    return s && (s->state == QLatin1String("pending") || s->state == QLatin1String("missed"));
}

Action clearAction(const std::optional<QString> &error, const std::optional<ScheduleView> &schedule)
{
    if (error)
        return refused(*error);
    if (clearable(schedule))
        return allowed(QStringLiteral("Remove the schedule %1").arg(schedule->unit));
    return refused(QStringLiteral("no schedule to clear"));
}

} // namespace

DriveActions deriveActions(const DriveView &d, const PairView &p, const GuiFacts &g)
{
    DriveActions a;

    // Upgrade
    QString why = d.recordError ? *d.recordError : sessionBlocker(d, p, g);
    if (why.isEmpty() && !d.record)
        why = QStringLiteral("no record of this drive yet");
    if (why.isEmpty() && g.unattended)
        why = unattendedBlocker(d);
    if (!why.isEmpty()) {
        a.upgrade = refused(why);
    } else if (g.unattended) {
        a.upgrade = allowed(QStringLiteral("Update %1 now, unattended: the recovery OS boots in its VM and "
                                           "upgrades itself through its guest agent, then powers off")
                                .arg(d.displayName));
    } else {
        a.upgrade = allowed(QStringLiteral("Update %1 now, attended: the recovery OS boots in its VM and the "
                                           "console opens for you to log in and run the checklist")
                                .arg(d.displayName));
        a.bannerNeeded = d.verdict == QLatin1String("will");
    }

    // Schedule: a running session does not block it, nor does the radio
    why = d.scheduleError ? *d.scheduleError : (d.recordError ? *d.recordError : unattendedBlocker(d));
    if (why.isEmpty())
        why = timeBlocker(g);
    a.schedule = why.isEmpty()
        ? allowed(QStringLiteral("Schedule an unattended update of %1 at the chosen time").arg(d.displayName))
        : refused(why);

    a.clearSchedule = clearAction(d.scheduleError, d.schedule);

    // Console
    if (d.sessionError) {
        a.console = refused(*d.sessionError);
    } else if (!d.session) {
        a.console = refused(QStringLiteral("no session is running"));
    } else if (d.session->domainState != QLatin1String("running")) {
        a.console = refused(QStringLiteral("the VM is not running (%1)")
                                .arg(d.session->domainState.isEmpty() ? QStringLiteral("state unknown")
                                                                       : d.session->domainState));
    } else {
        const bool ownJob = !g.ownJobId.isEmpty() && d.session->by == QStringLiteral("job:") + g.ownJobId;
        const bool attended = d.session->attended.value_or(false) || (ownJob && g.ownJobAttended);
        if (!attended && !d.session->attended.has_value() && !ownJob)
            a.console = refused(QStringLiteral("cannot tell whether this session is attended "
                                               "(it was not started by this window)"));
        else if (!attended)
            a.console = refused(QStringLiteral("an unattended session has no console to attend"));
        else if (g.viewerInstalled)
            a.console = allowed(QStringLiteral("Open the recovery OS's console in remote-viewer"));
        else
            a.console = allowed(QStringLiteral("virt-viewer is not installed: shows the console socket's "
                                               "path to connect a VNC viewer by hand"));
    }

    // End session
    if (d.sessionError)
        a.endSession = refused(*d.sessionError);
    else if (!d.session)
        a.endSession = refused(QStringLiteral("no session to end"));
    else if (d.session->by.startsWith(QLatin1String("job:")))
        a.endSession = refused(QStringLiteral("the session is a running job — use Cancel in the progress panel"));
    else if (d.session->by.startsWith(QLatin1String("unit:")))
        a.endSession = refused(QStringLiteral("a scheduled session is running; it gives the drive back itself"));
    else
        a.endSession = allowed(QStringLiteral("Give %1 back: power off its VM if it still runs, remove the "
                                              "guard and the console bridge, release the lock")
                                   .arg(d.displayName));

    return a;
}

PairActions derivePairActions(const RecoveryDocument &doc, const GuiFacts &g)
{
    PairActions p;
    p.modeDefault = doc.pair.modeDefault;
    if (doc.drives.size() != 2) {
        const Action no = refused(
            QStringLiteral("both drives: the configuration has %1 recovery drive(s), not 2").arg(doc.drives.size()));
        p.upgrade = no;
        p.schedule = no;
        p.clearSchedule = no;
        return p;
    }
    // The first drive that refuses names the pair's reason.
    QString upgradeWhy;
    QString scheduleWhy;
    for (const DriveView &d : doc.drives) {
        const DriveActions a = deriveActions(d, doc.pair, g);
        if (!a.upgrade.enabled && upgradeWhy.isEmpty())
            upgradeWhy = QStringLiteral("%1: %2").arg(d.label, a.upgrade.why);
        if (!a.schedule.enabled && scheduleWhy.isEmpty())
            scheduleWhy = QStringLiteral("%1: %2").arg(d.label, a.schedule.why);
    }
    if (doc.pair.sessionError)
        p.upgrade = refused(*doc.pair.sessionError);
    else if (!upgradeWhy.isEmpty())
        p.upgrade = refused(upgradeWhy);
    else
        p.upgrade = allowed(QStringLiteral("Update both drives now, %1")
                                .arg(g.unattended ? QStringLiteral("unattended") : QStringLiteral("attended")));
    if (doc.pair.scheduleError)
        p.schedule = refused(*doc.pair.scheduleError);
    else if (!scheduleWhy.isEmpty())
        p.schedule = refused(scheduleWhy);
    else
        p.schedule = allowed(QStringLiteral("Schedule an unattended update of both drives at the chosen time"));
    p.clearSchedule = clearAction(doc.pair.scheduleError, doc.pair.schedule);
    return p;
}

QString ageWords(std::optional<qint64> epoch, qint64 nowEpoch)
{
    if (!epoch)
        return QStringLiteral("unknown");
    const qint64 days = (nowEpoch - *epoch) / 86400;
    if (days <= 0)
        return QStringLiteral("today");
    return days == 1 ? QStringLiteral("1 day ago") : QStringLiteral("%1 days ago").arg(days);
}

QString verdictWords(const QString &verdict)
{
    if (verdict == QLatin1String("will"))
        return QStringLiteral("will run btrbk at boot");
    if (verdict == QLatin1String("may"))
        return QStringLiteral("may run btrbk at boot");
    if (verdict == QLatin1String("no"))
        return QStringLiteral("does not run btrbk at boot");
    return QStringLiteral("unknown");
}

QString sessionWords(const SessionView &s, qint64 nowEpoch)
{
    QString who;
    if (s.by.startsWith(QLatin1String("job:")))
        who = QStringLiteral("running as helper job %1").arg(s.by.mid(4));
    else if (s.by.startsWith(QLatin1String("unit:")))
        who = QStringLiteral("running as scheduled unit %1").arg(s.by.mid(5));
    else
        who = QStringLiteral("held by %1").arg(s.by.startsWith(QLatin1String("other:")) ? s.by.mid(6) : s.by);
    if (s.sinceEpoch)
        who += QStringLiteral(" since %1 minutes ago").arg((nowEpoch - *s.sinceEpoch) / 60);
    if (s.attended.has_value())
        who += *s.attended ? QStringLiteral(", attended") : QStringLiteral(", unattended");
    who += s.domainState.isEmpty() ? QStringLiteral(" (VM state unknown)")
                                   : QStringLiteral(" (VM %1)").arg(s.domainState);
    return who;
}

QString scheduleWords(const ScheduleView &s)
{
    return QStringLiteral("%1: %2").arg(s.state, s.detail);
}
