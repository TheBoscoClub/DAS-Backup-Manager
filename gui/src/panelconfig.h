#pragma once
#include <QString>
#include <QStringList>

// What the Backup panel reads out of config.toml: the labels it offers as
// checkboxes and whether [boot] is enabled. Pure text in, struct out, so the
// parsing is testable without a widget or a D-Bus helper (bd 51j6).
// `bootEnabled` defaults to true for display only: the backend refuses the
// boot step unless [boot] enabled is an explicit true.
struct PanelConfig {
    QStringList sources;
    QStringList targets;
    bool bootEnabled = true;
};

// Removes a trailing '#' comment, ignoring any '#' inside a "basic" or 'literal'
// string, and trims surrounding whitespace.
[[nodiscard]] QString stripTomlComment(const QString &line);

// Line-oriented reader (not a full TOML parser): recognises [[source]],
// [[target]] and [boot] headers and `label = ...` / `enabled = ...` keys, each
// with or without a trailing comment. Any other header ends the section.
[[nodiscard]] PanelConfig parsePanelConfig(const QString &toml);
