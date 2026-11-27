#include "statustext.h"
#include "filemodel.h"

#include <KLocalizedString>

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>

StatusText StatusText::assemble(const QString &statsJson, const QString &scheduleJson,
                                const QString &healthJson, const QStringList &failed)
{
    QStringList parts;

    // Next backup schedule
    if (!scheduleJson.isEmpty()) {
        const QJsonObject obj = QJsonDocument::fromJson(scheduleJson.toUtf8()).object();
        const QString next = obj.value(QLatin1String("next_incremental")).toString();
        if (!next.isEmpty())
            parts.append(i18n("Next: %1", next));
    }

    // Targets online
    if (!healthJson.isEmpty()) {
        const QJsonArray targets = QJsonDocument::fromJson(healthJson.toUtf8())
                                       .object().value(QLatin1String("targets")).toArray();
        int mounted = 0;
        for (const QJsonValue &v : targets) {
            if (v.toObject().value(QLatin1String("mounted")).toBool())
                ++mounted;
        }
        parts.append(i18n("%1 targets online", mounted));
    } else if (!failed.filter(QStringLiteral("HealthQuery")).isEmpty()) {
        parts.append(i18n("health unavailable"));
    }

    // DB stats: no reply is "unknown", never a database of 0 B
    if (!statsJson.isEmpty()) {
        const QJsonObject s = QJsonDocument::fromJson(statsJson.toUtf8()).object();
        parts.append(i18n("DB: %1", FileModel::formatSize(s.value(QLatin1String("db_size_bytes")).toInteger())));
        parts.append(i18n("%1 snapshots", s.value(QLatin1String("snapshots")).toInteger()));
    } else {
        parts.append(i18n("DB: unknown"));
    }

    return {
        parts.join(QStringLiteral(" | ")),
        failed.isEmpty() ? QString() : i18n("Refresh failed:\n%1", failed.join(QLatin1Char('\n'))),
    };
}

QString StatusText::lastBackupLine(qint64 ageSecs)
{
    if (ageSecs < 0)
        return i18n("Unknown");
    const qint64 hours = ageSecs / 3600;
    return hours > 0 ? i18n("%1 hours ago", hours)
                     : i18n("%1 minutes ago", (ageSecs % 3600) / 60);
}

QString OutcomeText::tip(const QString &plain)
{
    return QStringLiteral("<qt>%1</qt>").arg(plain.toHtmlEscaped().replace(QLatin1Char('\n'), QStringLiteral("<br>")));
}

QStringList OutcomeText::segments(const QString &summary)
{
    QStringList out;
    for (const QString &line : summary.split(QLatin1Char('\n'))) {
        for (const QString &part : line.split(QStringLiteral("; "))) {
            const QString t = part.trimmed();
            if (!t.isEmpty())
                out << t;
        }
    }
    return out;
}

namespace {
bool namesTheStop(const QString &segment)
{
    static const char *const words[] = {"REFUSED", "FAILED", "FAIL", "ERROR", "stopped at", "refused"};
    for (const char *w : words)
        if (segment.contains(QLatin1String(w)))
            return true;
    return false;
}

QString elided(const QString &text)
{
    constexpr int Longest = 240;
    return text.size() <= Longest ? text : text.left(Longest - 1) + QChar(0x2026);
}
} // namespace

OutcomeText OutcomeText::make(const QString &word, const QString &summary, const QString &lead,
                              const QString &note)
{
    QStringList rest = segments(summary);
    QString deciding = lead;
    if (deciding.isEmpty() && !rest.isEmpty()) {
        qsizetype at = 0;
        for (qsizetype i = 0; i < rest.size(); ++i) {
            if (namesTheStop(rest.at(i))) {
                at = i;
                break;
            }
        }
        deciding = rest.takeAt(at);
    } else if (!lead.isEmpty()) {
        // The lead is part of one segment: list the others.
        for (qsizetype i = 0; i < rest.size(); ++i) {
            if (rest.at(i).contains(lead)) {
                rest.removeAt(i);
                break;
            }
        }
    }

    OutcomeText out;
    out.plain = summary;
    QString html = QStringLiteral("<p><b style=\"color:#cc0000\">%1</b>&nbsp; %2</p>")
                       .arg(word.toHtmlEscaped(), elided(deciding).toHtmlEscaped());
    if (!note.isEmpty())
        html += QStringLiteral("<p>%1</p>").arg(note.toHtmlEscaped());
    if (!rest.isEmpty()) {
        html += QStringLiteral("<ul style=\"margin-top:0\">");
        const int shown = int(qMin<qsizetype>(rest.size(), ShownSegments));
        for (int i = 0; i < shown; ++i)
            html += QStringLiteral("<li>%1</li>").arg(elided(rest.at(i)).toHtmlEscaped());
        html += QStringLiteral("</ul>");
        if (rest.size() > shown)
            html += QStringLiteral("<p><i>%1</i></p>")
                        .arg(i18n("… and %1 more. The whole text is in the tooltip.", rest.size() - shown)
                                 .toHtmlEscaped());
    }
    out.html = html;
    return out;
}
