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
