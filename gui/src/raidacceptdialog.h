#pragma once

#include <QDialog>
#include <QString>

class QLabel;
class QLineEdit;
class QPlainTextEdit;
class QPushButton;
class DBusClient;

// The GUI's `btrdasd raid accept <label>` (bd DAS-Backup-Manager-su1b.11):
// when the RAID-1 guard refuses a target for split brain because the drive
// its last degraded period ran on is dead, show what the present drive holds
// and what is given up (the helper's RaidAcceptPreview), and accept only once
// the target's label is typed exactly (RaidAccept, which compares it again).
class RaidAcceptDialog : public QDialog
{
    Q_OBJECT

public:
    RaidAcceptDialog(DBusClient *client, const QString &label, QWidget *parent = nullptr);

    // Show the preview: `ok` with the explanation, or why there is nothing to
    // accept. Public so the smoke test can feed it. Returns `ok`.
    bool applyPreview(bool ok, const QString &text);

    // Whether Accept is offered now: a preview that succeeded and the label
    // typed exactly.
    [[nodiscard]] bool acceptOffered() const;

public Q_SLOTS:
    // Ask the helper for the preview again and show it.
    void refresh();

private Q_SLOTS:
    void acceptTyped();
    void updateAcceptEnabled();

private:
    DBusClient *m_client;
    QString m_label;
    bool m_previewOk = false;
    QPlainTextEdit *m_explanation = nullptr;
    QLineEdit *m_typed = nullptr;
    QPushButton *m_acceptButton = nullptr;
    QPushButton *m_refreshButton = nullptr;
    QPushButton *m_closeButton = nullptr;
    QLabel *m_statusLabel = nullptr;
};
