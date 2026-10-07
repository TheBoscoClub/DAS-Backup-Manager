#include "panelconfig.h"

QString stripTomlComment(const QString &line)
{
    QChar quote; // null when outside a string
    for (qsizetype i = 0; i < line.size(); ++i) {
        const QChar c = line.at(i);
        if (!quote.isNull()) {
            if (quote == QLatin1Char('"') && c == QLatin1Char('\\'))
                ++i; // skip the escaped character
            else if (c == quote)
                quote = QChar();
        } else if (c == QLatin1Char('"') || c == QLatin1Char('\'')) {
            quote = c;
        } else if (c == QLatin1Char('#')) {
            return line.left(i).trimmed();
        }
    }
    return line.trimmed();
}

PanelConfig parsePanelConfig(const QString &toml)
{
    PanelConfig cfg;

    enum class Section { None, Source, Target, Boot };
    Section currentSection = Section::None;

    const QStringList lines = toml.split(QLatin1Char('\n'));
    for (const QString &rawLine : lines) {
        const QString line = stripTomlComment(rawLine);

        // Detect section headers
        if (line == QLatin1String("[[source]]")) {
            currentSection = Section::Source;
            continue;
        }
        if (line == QLatin1String("[[target]]")) {
            currentSection = Section::Target;
            continue;
        }
        if (line == QLatin1String("[boot]")) {
            currentSection = Section::Boot;
            continue;
        }
        // Any other section header resets context
        if (line.startsWith(QLatin1Char('['))) {
            currentSection = Section::None;
            continue;
        }

        // Empty (or comment-only) lines
        if (line.isEmpty())
            continue;

        // Parse key = value (handles quoted and unquoted values)
        const qsizetype eqPos = line.indexOf(QLatin1Char('='));
        if (eqPos < 0)
            continue;

        const QString key = line.left(eqPos).trimmed();
        QString value = line.mid(eqPos + 1).trimmed();
        // Strip surrounding quotes
        if (value.length() >= 2
            && value.startsWith(QLatin1Char('"'))
            && value.endsWith(QLatin1Char('"'))) {
            value = value.mid(1, value.length() - 2);
        }

        switch (currentSection) {
        case Section::Source:
            if (key == QLatin1String("label"))
                cfg.sources.append(value);
            break;
        case Section::Target:
            if (key == QLatin1String("label"))
                cfg.targets.append(value);
            break;
        case Section::Boot:
            if (key == QLatin1String("enabled") && value == QLatin1String("false"))
                cfg.bootEnabled = false;
            break;
        case Section::None:
            break;
        }
    }
    return cfg;
}
