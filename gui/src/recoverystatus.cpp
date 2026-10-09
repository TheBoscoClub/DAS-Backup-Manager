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
