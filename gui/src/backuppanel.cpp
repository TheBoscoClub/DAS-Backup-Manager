#include "backuppanel.h"
#include "backupsteps.h"
#include "dbusclient.h"
#include "panelconfig.h"

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

#include <algorithm>

namespace {
// Property key used to store the original (accelerator-free) label on each
// dynamically created source/target checkbox.  KAcceleratorManager inserts
// '&' characters into widget text for keyboard shortcuts; reading text()
// back would pass those markers to the Rust backend as part of the label.
const char OriginalLabelProp[] = "originalLabel";
} // namespace

BackupPanel::BackupPanel(DBusClient *client, QWidget *parent)
    : QWidget(parent)
    , m_client(client)
    , m_configPath(QStringLiteral("/etc/das-backup/config.toml"))
{
    // Wrap the entire panel in a scroll area so that group-box content
    // remains accessible when the progress dock squeezes the central widget.
    auto *topLayout = new QVBoxLayout(this);
    topLayout->setContentsMargins(0, 0, 0, 0);

    auto *scrollArea = new QScrollArea(this);
    scrollArea->setWidgetResizable(true);
    scrollArea->setFrameShape(QFrame::NoFrame);
    topLayout->addWidget(scrollArea);

    auto *innerWidget = new QWidget(scrollArea);
    scrollArea->setWidget(innerWidget);

    auto *outerLayout = new QVBoxLayout(innerWidget);
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
    m_bootArchiveCheck->setChecked(true);

    m_indexCheck = new QCheckBox(i18n("Index"), m_operationsGroup);
    m_indexCheck->setToolTip(i18n("Run btrdasd content indexer on new snapshots for file search"));
    m_indexCheck->setChecked(true);

    m_emailCheck = new QCheckBox(i18n("Email Report"), m_operationsGroup);
    m_emailCheck->setToolTip(i18n("Email the report after the run (when [email] is enabled in config). The report is always saved."));
    m_emailCheck->setChecked(true);

    m_snapshotCheck->setObjectName(QStringLiteral("snapshotCheck"));
    m_sendCheck->setObjectName(QStringLiteral("sendCheck"));
    m_bootArchiveCheck->setObjectName(QStringLiteral("bootArchiveCheck"));
    m_indexCheck->setObjectName(QStringLiteral("indexCheck"));
    m_emailCheck->setObjectName(QStringLiteral("emailCheck"));

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

    m_dryRunButton->setObjectName(QStringLiteral("dryRunButton"));
    m_runButton->setObjectName(QStringLiteral("runButton"));

    // --- Connections ---
    connect(m_fullRadio, &QRadioButton::toggled, this, &BackupPanel::updateBootArchive);
    connect(m_incrementalRadio, &QRadioButton::toggled, this, &BackupPanel::updateBootArchive);
    connect(m_snapshotCheck, &QCheckBox::toggled, this, &BackupPanel::updateRunEnabled);
    connect(m_sendCheck, &QCheckBox::toggled, this, &BackupPanel::updateRunEnabled);
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
    applyConfig(m_client->configGet());
}

void BackupPanel::applyConfig(const QString &toml)
{
    m_bootEnabledInConfig = true;

    // Clear any previously created dynamic checkboxes
    for (QCheckBox *cb : std::as_const(m_sourceChecks)) {
        cb->deleteLater();
    }
    m_sourceChecks.clear();

    for (QCheckBox *cb : std::as_const(m_targetChecks)) {
        cb->deleteLater();
    }
    m_targetChecks.clear();

    if (toml.isEmpty()) {
        auto *errLabel = new QLabel(i18n("Could not load configuration"), m_sourcesGroup);
        errLabel->setEnabled(false);
        qobject_cast<QVBoxLayout *>(m_sourcesGroup->layout())->addWidget(errLabel);

        auto *errLabel2 = new QLabel(i18n("Could not load configuration"), m_targetsGroup);
        errLabel2->setEnabled(false);
        qobject_cast<QVBoxLayout *>(m_targetsGroup->layout())->addWidget(errLabel2);
        updateBootArchive();
        updateRunEnabled();
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

    const PanelConfig cfg = parsePanelConfig(toml);
    const QStringList &sources = cfg.sources;
    const QStringList &targets = cfg.targets;
    m_bootEnabledInConfig = cfg.bootEnabled;

    auto *srcLayout = qobject_cast<QVBoxLayout *>(m_sourcesGroup->layout());
    if (sources.isEmpty()) {
        auto *noSrc = new QLabel(i18n("No source volumes found in configuration"), m_sourcesGroup);
        noSrc->setEnabled(false);
        srcLayout->addWidget(noSrc);
    } else {
        for (const QString &label : std::as_const(sources)) {
            auto *cb = new QCheckBox(label, m_sourcesGroup);
            cb->setProperty(OriginalLabelProp, label);
            cb->setChecked(true);
            cb->setToolTip(i18n("Include this source in the backup"));
            srcLayout->addWidget(cb);
            m_sourceChecks.append(cb);
            connect(cb, &QCheckBox::toggled, this, &BackupPanel::updateRunEnabled);
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
            cb->setProperty(OriginalLabelProp, label);
            cb->setChecked(true);
            cb->setToolTip(i18n("Include this target in the backup"));
            tgtLayout->addWidget(cb);
            m_targetChecks.append(cb);
            connect(cb, &QCheckBox::toggled, this, &BackupPanel::updateRunEnabled);
        }
    }
    updateBootArchive();
    updateRunEnabled();
}

void BackupPanel::updateBootArchive()
{
    if (!m_bootEnabledInConfig) {
        m_bootForcedOff = true;
        m_bootArchiveCheck->setChecked(false);
        m_bootArchiveCheck->setEnabled(false);
        m_bootArchiveCheck->setToolTip(i18n("Disabled in config.toml ([boot] enabled = false)"));
        return;
    }
    m_bootArchiveCheck->setEnabled(true);
    // Only the disabled -> enabled transition re-ticks (the default is ticked);
    // a later mode toggle must not undo the user's own untick.
    if (m_bootForcedOff) {
        m_bootForcedOff = false;
        m_bootArchiveCheck->setChecked(true);
    }
    m_bootArchiveCheck->setToolTip(m_fullRadio->isChecked()
        ? i18n("Archive each [boot] subvolume (@, @home) on primary targets read-only, then replace it from the newest snapshot. Mirror targets are never touched.")
        : i18n("Create a [boot] subvolume (@, @home) that is missing on a primary target from the newest snapshot. An existing one is never replaced. Mirror targets are never touched."));
}

void BackupPanel::updateRunEnabled()
{
    const auto anyChecked = [](const QList<QCheckBox *> &boxes) {
        return std::any_of(boxes.cbegin(), boxes.cend(),
                           [](const QCheckBox *cb) { return cb->isChecked(); });
    };
    const bool anySelected = anyChecked(m_sourceChecks) && anyChecked(m_targetChecks);
    const bool opChosen = m_snapshotCheck->isChecked() || m_sendCheck->isChecked();
    const bool selected = anySelected && opChosen;
    const bool enabled = selected && !m_jobRunning;
    m_dryRunButton->setEnabled(enabled);
    m_runButton->setEnabled(enabled);
    QString why;
    if (!anySelected)
        why = i18n("Tick at least one source and one target");
    else if (!opChosen)
        why = i18n("Tick Snapshot or Send");
    m_dryRunButton->setStatusTip(why);
    m_runButton->setStatusTip(why);
}

void BackupPanel::runBackup(bool dryRun)
{
    const QString mode = m_incrementalRadio->isChecked()
        ? QStringLiteral("incremental")
        : QStringLiteral("full");

    QStringList sources;
    for (const QCheckBox *cb : std::as_const(m_sourceChecks)) {
        if (cb->isChecked()) {
            sources.append(cb->property(OriginalLabelProp).toString());
        }
    }

    QStringList targets;
    for (const QCheckBox *cb : std::as_const(m_targetChecks)) {
        if (cb->isChecked()) {
            targets.append(cb->property(OriginalLabelProp).toString());
        }
    }

    BackupSteps steps;
    steps.snapshot = m_snapshotCheck->isChecked();
    steps.send = m_sendCheck->isChecked();
    steps.bootArchive = m_bootArchiveCheck->isChecked();
    steps.index = m_indexCheck->isChecked();
    steps.email = m_emailCheck->isChecked();

    // Nothing ticked is nothing to back up — never "everything". The buttons
    // are disabled in that state; this keeps a stray call from sending it.
    if (sources.isEmpty() || targets.isEmpty() || !steps.runsBtrbk())
        return;

    m_jobRunning = true;
    updateRunEnabled();

    // Re-enable the buttons once the job completes (success or failure)
    connect(m_client, &DBusClient::jobFinished, this,
            [this](const QString & /*jobId*/, bool /*success*/, const QString & /*summary*/) {
                m_jobRunning = false;
                updateRunEnabled();
            },
            Qt::SingleShotConnection);

    // Re-enable buttons if the D-Bus call itself fails (e.g. polkit denied)
    connect(m_client, &DBusClient::errorOccurred, this,
            [this](const QString & /*operation*/, const QString & /*error*/) {
                m_jobRunning = false;
                updateRunEnabled();
            },
            Qt::SingleShotConnection);

    m_client->backupRun(mode, sources, targets, dryRun, steps);
}
