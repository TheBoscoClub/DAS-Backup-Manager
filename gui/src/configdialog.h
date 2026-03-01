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
