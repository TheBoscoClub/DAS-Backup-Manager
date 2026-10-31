#include "panelconfig.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>

namespace {

std::optional<QStringList> labels(const QJsonValue &v)
{
    if (!v.isArray())
        return std::nullopt;
    QStringList out;
    for (const QJsonValue &s : v.toArray()) {
        if (!s.isString())
            return std::nullopt;
        out.append(s.toString());
    }
    return out;
}

} // namespace

std::optional<PanelConfig> parsePanelJson(const QString &json)
{
    QJsonParseError err{};
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8(), &err);
    if (err.error != QJsonParseError::NoError || !doc.isObject())
        return std::nullopt;
    const QJsonObject o = doc.object();
    const auto sources = labels(o.value(QStringLiteral("sources")));
    const auto targets = labels(o.value(QStringLiteral("targets")));
    const QJsonValue boot = o.value(QStringLiteral("boot_enabled"));
    if (!sources || !targets || !boot.isBool())
        return std::nullopt;
    return PanelConfig{*sources, *targets, boot.toBool()};
}
