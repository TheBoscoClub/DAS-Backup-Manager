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

    // Rebuilds the source/target boxes and the Boot Archive state from config
    // text. loadConfig() feeds it the helper's config; tests feed it directly.
    void applyConfig(const QString &toml);

private Q_SLOTS:
    void runBackup(bool dryRun);
    void loadConfig();
    // Run and Dry Run are available only while a source and a target are
    // ticked and no job is running. An empty selection is not "everything":
    // the helper refuses it (bd DAS-Backup-Manager-7tx).
    void updateRunEnabled();
    // Boot Archive says what it does in the selected mode, and is unticked and
    // disabled, with the reason, when [boot] enabled = false in config.toml.
    void updateBootArchive();

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
    bool m_jobRunning = false;
    bool m_bootEnabledInConfig = true;
    bool m_bootForcedOff = false; // unticked by the config, re-ticked on re-enable
};
