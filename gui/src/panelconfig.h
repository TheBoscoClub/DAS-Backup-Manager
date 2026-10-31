#pragma once
#include <QString>
#include <QStringList>

#include <optional>

// What the Backup panel shows: the labels it offers as checkboxes and whether
// [boot] is enabled. The helper reads them from the parsed config and sends
// them as JSON (PanelConfigGet, `Config::panel_json`); this side only decodes
// that, so the panel can never misread a hand-written config.toml (a spaced
// header, an inline table, a header inside a string — bd
// DAS-Backup-Manager-28il). Pure text in, struct out: testable without a
// widget or a D-Bus helper (bd 51j6).
struct PanelConfig {
    QStringList sources;
    QStringList targets;
    bool bootEnabled = true;
};

// `{"sources": [...], "targets": [...], "boot_enabled": bool}`. Empty, not
// JSON, or missing any of the three keys with its type: nullopt, never a
// guessed default.
[[nodiscard]] std::optional<PanelConfig> parsePanelJson(const QString &json);
