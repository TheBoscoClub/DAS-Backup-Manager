#pragma once
#include <KPageDialog>

class QPlainTextEdit;
class QPushButton;
class QLabel;
class DBusClient;

class ConfigDialog : public KPageDialog
{
    Q_OBJECT
public:
    explicit ConfigDialog(DBusClient *client, QWidget *parent = nullptr);

    // The status line after a successful save: whether the file written
    // (read back) differs from the text typed — the save wrote out
    // backup-scoping keys the text left out.
    [[nodiscard]] static QString savedStatus(const QString &typed, const QString &written);

private Q_SLOTS:
    void loadConfig();
    void saveConfig();
    void showDiff();

private:
    DBusClient *m_client;
    QString m_configPath;
    QString m_originalContent;
    QPlainTextEdit *m_editor = nullptr;
    QPushButton *m_saveButton = nullptr;
    QPushButton *m_diffButton = nullptr;
    QLabel *m_statusLabel = nullptr;
};
