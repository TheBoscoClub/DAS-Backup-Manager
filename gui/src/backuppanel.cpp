#include "backuppanel.h"
#include "dbusclient.h"

#include <KLocalizedString>

#include <QButtonGroup>
#include <QCheckBox>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QLabel>
#include <QPushButton>
#include <QRadioButton>
#include <QScrollArea>
#include <QStringList>
#include <QVBoxLayout>

BackupPanel::BackupPanel(DBusClient *client, QWidget *parent)
    : QWidget(parent)
    , m_client(client)
    , m_configPath(QStringLiteral("/etc/das-backup/config.toml"))
{
    auto *outerLayout = new QVBoxLayout(this);
    outerLayout->setContentsMargins(12, 12, 12, 12);
    outerLayout->setSpacing(10);

    // --- Title ---
    auto *titleLabel = new QLabel(i18n("Backup Operations"), this);
    QFont titleFont = titleLabel->font();
    titleFont.setPointSize(titleFont.pointSize() + 2);
    titleFont.setBold(true);
    titleLabel->setFont(titleFont);
    outerLayout->addWidget(titleLabel);

    // --- Mode group ---
    auto *modeGroup = new QGroupBox(i18n("Mode"), this);
    auto *modeLayout = new QHBoxLayout(modeGroup);
    modeLayout->setSpacing(16);

    m_incrementalRadio = new QRadioButton(i18n("Incremental"), modeGroup);
    m_incrementalRadio->setToolTip(i18n("Send only changed blocks since the last snapshot (faster, less data)"));
    m_incrementalRadio->setChecked(true);

    m_fullRadio = new QRadioButton(i18n("Full"), modeGroup);
    m_fullRadio->setToolTip(i18n("Send complete snapshots without incremental parents (slower, standalone)"));

    // Button group keeps the two radios mutually exclusive within the panel
    auto *modeButtonGroup = new QButtonGroup(this);
    modeButtonGroup->addButton(m_incrementalRadio);
    modeButtonGroup->addButton(m_fullRadio);

    modeLayout->addWidget(m_incrementalRadio);
    modeLayout->addWidget(m_fullRadio);
    modeLayout->addStretch(1);
    outerLayout->addWidget(modeGroup);

    // --- Operations group ---
    m_operationsGroup = new QGroupBox(i18n("Operations"), this);
    auto *opsLayout = new QVBoxLayout(m_operationsGroup);
    opsLayout->setSpacing(4);

    m_snapshotCheck = new QCheckBox(i18n("Snapshot"), m_operationsGroup);
    m_snapshotCheck->setToolTip(i18n("Create local BTRFS snapshots of source subvolumes"));
    m_snapshotCheck->setChecked(true);

    m_sendCheck = new QCheckBox(i18n("Send"), m_operationsGroup);
    m_sendCheck->setToolTip(i18n("Transfer snapshots to backup targets via btrfs send/receive"));
    m_sendCheck->setChecked(true);

    m_bootArchiveCheck = new QCheckBox(i18n("Boot Archive"), m_operationsGroup);
    m_bootArchiveCheck->setToolTip(i18n("Archive the @boot subvolume as a read-only snapshot before recreation"));
    m_bootArchiveCheck->setChecked(true);

    m_indexCheck = new QCheckBox(i18n("Index"), m_operationsGroup);
    m_indexCheck->setToolTip(i18n("Run btrdasd content indexer on new snapshots for file search"));
    m_indexCheck->setChecked(true);

    m_emailCheck = new QCheckBox(i18n("Email Report"), m_operationsGroup);
    m_emailCheck->setToolTip(i18n("Send a summary email report after the backup completes"));
    m_emailCheck->setChecked(true);

    opsLayout->addWidget(m_snapshotCheck);
    opsLayout->addWidget(m_sendCheck);
    opsLayout->addWidget(m_bootArchiveCheck);
    opsLayout->addWidget(m_indexCheck);
    opsLayout->addWidget(m_emailCheck);
    outerLayout->addWidget(m_operationsGroup);

    // --- Sources group (populated by loadConfig) ---
    m_sourcesGroup = new QGroupBox(i18n("Sources"), this);
    m_sourcesGroup->setLayout(new QVBoxLayout(m_sourcesGroup));
    outerLayout->addWidget(m_sourcesGroup);

    // --- Targets group (populated by loadConfig) ---
    m_targetsGroup = new QGroupBox(i18n("Targets"), this);
    m_targetsGroup->setLayout(new QVBoxLayout(m_targetsGroup));
    outerLayout->addWidget(m_targetsGroup);

    outerLayout->addStretch(1);

    // --- Button row ---
    auto *buttonRow = new QHBoxLayout();
    buttonRow->setSpacing(8);

    m_dryRunButton = new QPushButton(
        QIcon::fromTheme(QStringLiteral("system-run")),
        i18n("Dry Run"), this);
    m_dryRunButton->setToolTip(i18n("Simulate a backup run without writing any data"));

    m_runButton = new QPushButton(
        QIcon::fromTheme(QStringLiteral("media-playback-start")),
        i18n("Run Backup"), this);
    m_runButton->setToolTip(i18n("Start the backup with the selected options"));

    buttonRow->addStretch(1);
    buttonRow->addWidget(m_dryRunButton);
    buttonRow->addWidget(m_runButton);
    outerLayout->addLayout(buttonRow);

    // --- Connections ---
    connect(m_dryRunButton, &QPushButton::clicked, this, [this]() {
        runBackup(true);
    });
    connect(m_runButton, &QPushButton::clicked, this, [this]() {
        runBackup(false);
    });

    // Populate sources and targets from config
    loadConfig();
}

void BackupPanel::loadConfig()
{
    // Clear any previously created dynamic checkboxes
    for (QCheckBox *cb : std::as_const(m_sourceChecks)) {
        cb->deleteLater();
    }
    m_sourceChecks.clear();

    for (QCheckBox *cb : std::as_const(m_targetChecks)) {
        cb->deleteLater();
    }
    m_targetChecks.clear();

    const QString toml = m_client->configGet(m_configPath);

    if (toml.isEmpty()) {
        auto *errLabel = new QLabel(i18n("Could not load configuration"), m_sourcesGroup);
        errLabel->setEnabled(false);
        qobject_cast<QVBoxLayout *>(m_sourcesGroup->layout())->addWidget(errLabel);

        auto *errLabel2 = new QLabel(i18n("Could not load configuration"), m_targetsGroup);
        errLabel2->setEnabled(false);
        qobject_cast<QVBoxLayout *>(m_targetsGroup->layout())->addWidget(errLabel2);
        return;
    }

    // Parse TOML config to extract source labels and target labels.
    //
    // The config uses inline arrays for subvolumes:
    //   [[source]]
    //   label = "nvme"
    //   volume = "/.btrfs-nvme"
    //   subvolumes = ["@", "@home", "@root", "@log"]
    //
    //   [[target]]
    //   label = "primary-22tb"
    //   display_name = "22TB Exos (Bay 2)"
    //
    // The GUI shows source labels and target labels as checkboxes.
    // The Rust backend resolves subvolumes from labels internally.

    QStringList sources;
    QStringList targets;

    enum class Section { None, Source, Target };
    Section currentSection = Section::None;

    const QStringList lines = toml.split(QLatin1Char('\n'));
    for (const QString &rawLine : lines) {
        const QString line = rawLine.trimmed();

        // Detect section headers
        if (line == QLatin1String("[[source]]")) {
            currentSection = Section::Source;
            continue;
        }
        if (line == QLatin1String("[[target]]")) {
            currentSection = Section::Target;
            continue;
        }
        // Any other section header resets context
        if (line.startsWith(QLatin1Char('['))) {
            currentSection = Section::None;
            continue;
        }

        // Skip comments and empty lines
        if (line.isEmpty() || line.startsWith(QLatin1Char('#')))
            continue;

        // Parse key = value (handles quoted and unquoted values)
        const int eqPos = line.indexOf(QLatin1Char('='));
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
                sources.append(value);
            break;
        case Section::Target:
            if (key == QLatin1String("label"))
                targets.append(value);
            break;
        case Section::None:
            break;
        }
    }

    auto *srcLayout = qobject_cast<QVBoxLayout *>(m_sourcesGroup->layout());
    if (sources.isEmpty()) {
        auto *noSrc = new QLabel(i18n("No source volumes found in configuration"), m_sourcesGroup);
        noSrc->setEnabled(false);
        srcLayout->addWidget(noSrc);
    } else {
        for (const QString &label : std::as_const(sources)) {
            auto *cb = new QCheckBox(label, m_sourcesGroup);
            cb->setChecked(true);
            cb->setToolTip(i18n("Include this source in the backup"));
            srcLayout->addWidget(cb);
            m_sourceChecks.append(cb);
        }
    }

    auto *tgtLayout = qobject_cast<QVBoxLayout *>(m_targetsGroup->layout());
    if (targets.isEmpty()) {
        auto *noTgt = new QLabel(i18n("No target paths found in configuration"), m_targetsGroup);
        noTgt->setEnabled(false);
        tgtLayout->addWidget(noTgt);
    } else {
        for (const QString &label : std::as_const(targets)) {
            auto *cb = new QCheckBox(label, m_targetsGroup);
            cb->setChecked(true);
            cb->setToolTip(i18n("Include this target in the backup"));
            tgtLayout->addWidget(cb);
            m_targetChecks.append(cb);
        }
    }
}

void BackupPanel::runBackup(bool dryRun)
{
    const QString mode = m_incrementalRadio->isChecked()
        ? QStringLiteral("incremental")
        : QStringLiteral("full");

    QStringList sources;
    for (const QCheckBox *cb : std::as_const(m_sourceChecks)) {
        if (cb->isChecked()) {
            sources.append(cb->text());
        }
    }

    QStringList targets;
    for (const QCheckBox *cb : std::as_const(m_targetChecks)) {
        if (cb->isChecked()) {
            targets.append(cb->text());
        }
    }

    m_dryRunButton->setEnabled(false);
    m_runButton->setEnabled(false);

    // Re-enable the buttons once the job completes (success or failure)
    connect(m_client, &DBusClient::jobFinished, this,
            [this](const QString & /*jobId*/, bool /*success*/, const QString & /*summary*/) {
                m_dryRunButton->setEnabled(true);
                m_runButton->setEnabled(true);
            },
            Qt::SingleShotConnection);

    // Re-enable buttons if the D-Bus call itself fails (e.g. polkit denied)
    connect(m_client, &DBusClient::errorOccurred, this,
            [this](const QString & /*operation*/, const QString & /*error*/) {
                m_dryRunButton->setEnabled(true);
                m_runButton->setEnabled(true);
            },
            Qt::SingleShotConnection);

    m_client->backupRun(m_configPath, mode, sources, targets, dryRun);
}
