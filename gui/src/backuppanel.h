#pragma once
#include <QWidget>

class QRadioButton;
class QCheckBox;
class QPushButton;
class QGroupBox;
class DBusClient;

class BackupPanel : public QWidget
{
    Q_OBJECT
public:
    explicit BackupPanel(DBusClient *client, QWidget *parent = nullptr);

private Q_SLOTS:
    void runBackup(bool dryRun);
    void loadConfig();

private:
    DBusClient *m_client;
    QString m_configPath;

    QRadioButton *m_incrementalRadio = nullptr;
    QRadioButton *m_fullRadio = nullptr;

    QGroupBox *m_operationsGroup = nullptr;
    QCheckBox *m_snapshotCheck = nullptr;
    QCheckBox *m_sendCheck = nullptr;
    QCheckBox *m_bootArchiveCheck = nullptr;
    QCheckBox *m_indexCheck = nullptr;
    QCheckBox *m_emailCheck = nullptr;

    QGroupBox *m_sourcesGroup = nullptr;
    QGroupBox *m_targetsGroup = nullptr;
    QList<QCheckBox *> m_sourceChecks;
    QList<QCheckBox *> m_targetChecks;

    QPushButton *m_dryRunButton = nullptr;
    QPushButton *m_runButton = nullptr;
};
