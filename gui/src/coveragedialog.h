#pragma once

#include <QDialog>

class QLabel;
class QPushButton;
class QTableWidget;
class DBusClient;

// The subvolume/target pairs the coverage record holds as no longer backed
// up, and an Accept for one selected pair (bd DAS-Backup-Manager-sp78.4) —
// the GUI's `btrdasd subvol coverage list|accept`. Accept is per pair
// (volume, subvolume, target), never per pattern (operator 2026-11-01).
class CoverageDialog : public QDialog
{
    Q_OBJECT

public:
    explicit CoverageDialog(DBusClient *client, QWidget *parent = nullptr);

    // Fill the table from CoverageList's JSON (`subvol coverage list --json`):
    // one row per dropped pair. Anything else empties the table, says so in
    // the status line and returns false. Public so the smoke test can feed it.
    bool applyJson(const QString &json);

public Q_SLOTS:
    // Ask the helper for the list again and show it.
    void refresh();

private Q_SLOTS:
    void acceptSelected();
    void updateAcceptEnabled();

private:
    DBusClient *m_client;
    QTableWidget *m_table = nullptr;
    QPushButton *m_acceptButton = nullptr;
    QPushButton *m_refreshButton = nullptr;
    QPushButton *m_closeButton = nullptr;
    QLabel *m_statusLabel = nullptr;
};
