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
    , m_configPath(QStringLiteral("/etc/btrbk/btrbk.conf"))
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

    // Simple line-by-line config parser:
    //   volume /path        -> source volume (context for subvolumes below it)
    //   subvolume @name     -> subvolume under the current volume
    //   target /path        -> backup target
    //   # manual-only       -> comment immediately after a subvolume marks it manual
    //
    // A subvolume entry is a source; its display label is "volume/subvolume".
    // If no subvolumes are found under a volume, the volume itself is a source.

    struct SourceEntry {
        QString label;
        bool manualOnly{false};
    };

    QList<SourceEntry> sources;
    QStringList targets;

    QString currentVolume;
    bool lastWasSubvolume = false;
    QString lastSubvolumeLabel;
    int lastSubvolumeIndex = -1;

    const QStringList lines = toml.split(QLatin1Char('\n'));
    for (const QString &rawLine : lines) {
        const QString line = rawLine.trimmed();

        if (line.startsWith(QLatin1String("volume "))) {
            currentVolume = line.mid(7).trimmed();  // text after "volume "
            lastWasSubvolume = false;
            continue;
        }

        if (line.startsWith(QLatin1String("subvolume "))) {
            const QString subName = line.mid(10).trimmed();  // text after "subvolume "
            lastSubvolumeLabel = currentVolume.isEmpty()
                ? subName
                : currentVolume + QLatin1Char('/') + subName;
            lastSubvolumeIndex = static_cast<int>(sources.size());
            sources.append({lastSubvolumeLabel, false});
            lastWasSubvolume = true;
            continue;
        }

        if (line.startsWith(QLatin1String("target "))) {
            targets.append(line.mid(7).trimmed());  // text after "target "
            lastWasSubvolume = false;
            continue;
        }

        if (line.startsWith(QLatin1Char('#')) && lastWasSubvolume) {
            // Check for the manual-only flag marker anywhere in the comment
            if (line.contains(QLatin1String("manual-only")) && lastSubvolumeIndex >= 0) {
                sources[lastSubvolumeIndex].manualOnly = true;
            }
            // A comment does not clear the lastWasSubvolume state so that
            // multiple comment lines after a subvolume all see it.
            continue;
        }

        // Any non-comment, non-blank line clears the subvolume context
        if (!line.isEmpty()) {
            lastWasSubvolume = false;
        }
    }

    // If no subvolumes were parsed but we have volumes, fall back to volumes as sources
    if (sources.isEmpty() && !currentVolume.isEmpty()) {
        sources.append({currentVolume, false});
    }

    auto *srcLayout = qobject_cast<QVBoxLayout *>(m_sourcesGroup->layout());
    if (sources.isEmpty()) {
        auto *noSrc = new QLabel(i18n("No source volumes found in configuration"), m_sourcesGroup);
        noSrc->setEnabled(false);
        srcLayout->addWidget(noSrc);
    } else {
        for (const SourceEntry &entry : std::as_const(sources)) {
            auto *cb = new QCheckBox(entry.label, m_sourcesGroup);
            if (entry.manualOnly) {
                cb->setChecked(false);
                cb->setEnabled(false);
                cb->setToolTip(i18n("Manual-only subvolume"));
            } else {
                cb->setChecked(true);
                cb->setToolTip(i18n("Include this source in the backup"));
            }
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
        for (const QString &path : std::as_const(targets)) {
            auto *cb = new QCheckBox(path, m_targetsGroup);
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

    m_client->backupRun(m_configPath, mode, sources, targets, dryRun);
}
