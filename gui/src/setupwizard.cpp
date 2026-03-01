#include "setupwizard.h"
#include "dbusclient.h"

#include <KLocalizedString>
#include <KMessageBox>

#include <QButtonGroup>
#include <QFile>
#include <QFileInfo>
#include <QHBoxLayout>
#include <QLabel>
#include <QListWidget>
#include <QRadioButton>
#include <QTextStream>
#include <QTimeEdit>
#include <QVBoxLayout>
#include <QWizardPage>

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

SetupWizard::SetupWizard(DBusClient *client, QWidget *parent)
    : QWizard(parent)
    , m_client(client)
{
    setWindowTitle(i18n("DAS Backup Manager Setup"));
    setWizardStyle(QWizard::ModernStyle);
    resize(640, 480);

    buildWelcomePage();
    buildSourcePage();
    buildTargetPage();
    buildSchedulePage();
    buildSummaryPage();
}

// ---------------------------------------------------------------------------
// needsSetup — static
// ---------------------------------------------------------------------------

bool SetupWizard::needsSetup()
{
    const QFileInfo info(QStringLiteral("/etc/btrbk/btrbk.conf"));
    if (!info.exists()) {
        return true;
    }
    return info.size() == 0;
}

// ---------------------------------------------------------------------------
// Page 1: Welcome
// ---------------------------------------------------------------------------

void SetupWizard::buildWelcomePage()
{
    auto *page = new QWizardPage(this);
    page->setTitle(i18n("DAS Backup Manager Setup"));
    page->setSubTitle(i18n("This wizard will help you configure your backup system."));

    auto *layout = new QVBoxLayout(page);

    auto *icon = new QLabel(page);
    icon->setPixmap(QIcon::fromTheme(QStringLiteral("preferences-system-backup"),
                                     QIcon::fromTheme(QStringLiteral("drive-harddisk")))
                        .pixmap(64, 64));
    icon->setAlignment(Qt::AlignCenter);
    icon->setToolTip(i18n("Backup system configuration wizard"));
    layout->addWidget(icon);

    auto *description = new QLabel(
        i18n("Welcome to the DAS Backup Manager first-run setup.\n\n"
             "This wizard will walk you through:\n"
             "  1. Selecting source subvolumes to back up\n"
             "  2. Choosing backup target drives\n"
             "  3. Configuring a backup schedule\n\n"
             "You can change these settings at any time from the "
             "configuration editor."),
        page);
    description->setWordWrap(true);
    description->setToolTip(i18n("Overview of the setup process"));
    layout->addWidget(description);

    layout->addStretch(1);

    setPage(Page_Welcome, page);
}

// ---------------------------------------------------------------------------
// Page 2: Source Selection
// ---------------------------------------------------------------------------

void SetupWizard::buildSourcePage()
{
    auto *page = new QWizardPage(this);
    page->setTitle(i18n("Source Subvolumes"));
    page->setSubTitle(i18n("Select the BTRFS subvolumes you want to back up."));

    auto *layout = new QVBoxLayout(page);

    auto *label = new QLabel(
        i18n("The following BTRFS mount points were detected on this system. "
             "Select the ones you want to include in your backup configuration."),
        page);
    label->setWordWrap(true);
    label->setToolTip(i18n("Detected BTRFS file systems from /proc/mounts"));
    layout->addWidget(label);

    m_sourceList = new QListWidget(page);
    m_sourceList->setToolTip(i18n("Check the subvolumes to include as backup sources"));

    const QStringList mounts = detectBtrfsMounts();
    for (const QString &mount : mounts) {
        auto *item = new QListWidgetItem(mount, m_sourceList);
        item->setFlags(item->flags() | Qt::ItemIsUserCheckable);
        // Default-check / and /home
        if (mount == QLatin1String("/") || mount == QLatin1String("/home")) {
            item->setCheckState(Qt::Checked);
        } else {
            item->setCheckState(Qt::Unchecked);
        }
    }

    if (mounts.isEmpty()) {
        auto *item = new QListWidgetItem(
            i18n("(No BTRFS mount points detected)"), m_sourceList);
        item->setFlags(item->flags() & ~Qt::ItemIsUserCheckable);
        item->setForeground(Qt::gray);
    }

    layout->addWidget(m_sourceList, 1);

    setPage(Page_Source, page);
}

// ---------------------------------------------------------------------------
// Page 3: Target Selection
// ---------------------------------------------------------------------------

void SetupWizard::buildTargetPage()
{
    auto *page = new QWizardPage(this);
    page->setTitle(i18n("Backup Targets"));
    page->setSubTitle(i18n("Select the drives where backups should be stored."));

    auto *layout = new QVBoxLayout(page);

    auto *label = new QLabel(
        i18n("The following non-root mount points were detected. "
             "Select the ones to use as backup destinations. "
             "These should be external or secondary drives."),
        page);
    label->setWordWrap(true);
    label->setToolTip(i18n("Detected mount points excluding the root filesystem"));
    layout->addWidget(label);

    m_targetList = new QListWidget(page);
    m_targetList->setToolTip(i18n("Check the drives to use as backup targets"));

    const QStringList targets = detectTargetMounts();
    for (const QString &target : targets) {
        auto *item = new QListWidgetItem(target, m_targetList);
        item->setFlags(item->flags() | Qt::ItemIsUserCheckable);
        item->setCheckState(Qt::Unchecked);
    }

    if (targets.isEmpty()) {
        auto *item = new QListWidgetItem(
            i18n("(No suitable target mount points detected)"), m_targetList);
        item->setFlags(item->flags() & ~Qt::ItemIsUserCheckable);
        item->setForeground(Qt::gray);
    }

    layout->addWidget(m_targetList, 1);

    setPage(Page_Target, page);
}

// ---------------------------------------------------------------------------
// Page 4: Schedule
// ---------------------------------------------------------------------------

void SetupWizard::buildSchedulePage()
{
    auto *page = new QWizardPage(this);
    page->setTitle(i18n("Backup Schedule"));
    page->setSubTitle(i18n("Configure how often backups should run."));

    auto *layout = new QVBoxLayout(page);

    auto *freqLabel = new QLabel(i18n("Backup frequency:"), page);
    freqLabel->setToolTip(i18n("How often the backup timer fires"));
    layout->addWidget(freqLabel);

    m_dailyRadio = new QRadioButton(i18n("Daily"), page);
    m_dailyRadio->setToolTip(i18n("Run a backup once every day at the scheduled time"));
    m_dailyRadio->setChecked(true);

    m_weeklyRadio = new QRadioButton(i18n("Weekly"), page);
    m_weeklyRadio->setToolTip(i18n("Run a backup once every week at the scheduled time"));

    m_manualRadio = new QRadioButton(i18n("Manual only"), page);
    m_manualRadio->setToolTip(i18n("No automatic schedule — backups must be started manually"));

    auto *freqGroup = new QButtonGroup(this);
    freqGroup->addButton(m_dailyRadio);
    freqGroup->addButton(m_weeklyRadio);
    freqGroup->addButton(m_manualRadio);

    layout->addWidget(m_dailyRadio);
    layout->addWidget(m_weeklyRadio);
    layout->addWidget(m_manualRadio);

    layout->addSpacing(12);

    auto *timeRow = new QHBoxLayout();
    auto *timeLabel = new QLabel(i18n("Scheduled time:"), page);
    timeLabel->setToolTip(i18n("The time of day when scheduled backups will start"));
    timeRow->addWidget(timeLabel);

    m_timeEdit = new QTimeEdit(page);
    m_timeEdit->setDisplayFormat(QStringLiteral("HH:mm"));
    m_timeEdit->setTime(QTime(4, 0));
    m_timeEdit->setToolTip(i18n("Set the hour and minute for scheduled backups (24-hour format)"));
    timeRow->addWidget(m_timeEdit);
    timeRow->addStretch(1);
    layout->addLayout(timeRow);

    // Disable the time picker when "Manual only" is selected
    connect(m_manualRadio, &QRadioButton::toggled, this, [this](bool checked) {
        m_timeEdit->setEnabled(!checked);
    });

    layout->addStretch(1);

    setPage(Page_Schedule, page);
}

// ---------------------------------------------------------------------------
// Page 5: Summary & Finish
// ---------------------------------------------------------------------------

void SetupWizard::buildSummaryPage()
{
    auto *page = new QWizardPage(this);
    page->setTitle(i18n("Summary"));
    page->setSubTitle(i18n("Review your selections and apply the configuration."));
    page->setFinalPage(true);

    auto *layout = new QVBoxLayout(page);

    m_summaryLabel = new QLabel(page);
    m_summaryLabel->setWordWrap(true);
    m_summaryLabel->setTextFormat(Qt::RichText);
    m_summaryLabel->setToolTip(i18n("Summary of the configuration that will be applied"));
    layout->addWidget(m_summaryLabel, 1);

    // Override initializePage to populate summary when the user navigates here
    connect(this, &QWizard::currentIdChanged, this, [this](int id) {
        if (id != Page_Summary) {
            return;
        }

        // Gather selected sources
        QStringList sources;
        for (int i = 0; i < m_sourceList->count(); ++i) {
            auto *item = m_sourceList->item(i);
            if (item->checkState() == Qt::Checked) {
                sources.append(item->text());
            }
        }

        // Gather selected targets
        QStringList targets;
        for (int i = 0; i < m_targetList->count(); ++i) {
            auto *item = m_targetList->item(i);
            if (item->checkState() == Qt::Checked) {
                targets.append(item->text());
            }
        }

        // Schedule description
        QString schedule;
        if (m_manualRadio->isChecked()) {
            schedule = i18n("Manual only (no automatic schedule)");
        } else if (m_weeklyRadio->isChecked()) {
            schedule = i18n("Weekly at %1", m_timeEdit->time().toString(QStringLiteral("HH:mm")));
        } else {
            schedule = i18n("Daily at %1", m_timeEdit->time().toString(QStringLiteral("HH:mm")));
        }

        // Build rich-text summary
        QString html;
        html += QStringLiteral("<h3>") + i18n("Configuration Summary") + QStringLiteral("</h3>");

        html += QStringLiteral("<p><b>") + i18n("Source subvolumes:") + QStringLiteral("</b></p><ul>");
        if (sources.isEmpty()) {
            html += QStringLiteral("<li><i>") + i18n("None selected") + QStringLiteral("</i></li>");
        } else {
            for (const QString &s : std::as_const(sources)) {
                html += QStringLiteral("<li>") + s.toHtmlEscaped() + QStringLiteral("</li>");
            }
        }
        html += QStringLiteral("</ul>");

        html += QStringLiteral("<p><b>") + i18n("Backup targets:") + QStringLiteral("</b></p><ul>");
        if (targets.isEmpty()) {
            html += QStringLiteral("<li><i>") + i18n("None selected") + QStringLiteral("</i></li>");
        } else {
            for (const QString &t : std::as_const(targets)) {
                html += QStringLiteral("<li>") + t.toHtmlEscaped() + QStringLiteral("</li>");
            }
        }
        html += QStringLiteral("</ul>");

        html += QStringLiteral("<p><b>") + i18n("Schedule:") + QStringLiteral("</b> ")
              + schedule.toHtmlEscaped() + QStringLiteral("</p>");

        html += QStringLiteral("<p>") +
                i18n("Click <b>Finish</b> to generate and save the btrbk configuration.") +
                QStringLiteral("</p>");

        m_summaryLabel->setText(html);
    });

    setPage(Page_Summary, page);

    // Apply configuration when the wizard finishes
    connect(this, &QWizard::accepted, this, &SetupWizard::applyConfiguration);
}

// ---------------------------------------------------------------------------
// applyConfiguration — generate config and save via DBusClient
// ---------------------------------------------------------------------------

void SetupWizard::applyConfiguration()
{
    const QString config = generateConfig();

    if (config.isEmpty()) {
        KMessageBox::error(this,
            i18n("No sources or targets selected. Configuration was not saved."),
            i18n("Setup Incomplete"));
        return;
    }

    const QString configPath = QStringLiteral("/etc/btrbk/btrbk.conf");
    const bool ok = m_client->configSet(configPath, config);

    if (ok) {
        Q_EMIT setupComplete();
    } else {
        KMessageBox::error(this,
            i18n("Failed to save configuration. Ensure the btrdasd-helper "
                 "service is running and try again from the configuration editor."),
            i18n("Save Failed"));
    }
}

// ---------------------------------------------------------------------------
// generateConfig — build btrbk.conf from wizard selections
// ---------------------------------------------------------------------------

QString SetupWizard::generateConfig() const
{
    QStringList sources;
    for (int i = 0; i < m_sourceList->count(); ++i) {
        auto *item = m_sourceList->item(i);
        if (item->checkState() == Qt::Checked) {
            sources.append(item->text());
        }
    }

    QStringList targets;
    for (int i = 0; i < m_targetList->count(); ++i) {
        auto *item = m_targetList->item(i);
        if (item->checkState() == Qt::Checked) {
            targets.append(item->text());
        }
    }

    if (sources.isEmpty() || targets.isEmpty()) {
        return {};
    }

    QString config;
    QTextStream out(&config);

    out << "# btrbk configuration\n";
    out << "# Generated by DAS Backup Manager Setup Wizard\n";
    out << "#\n";
    out << "# See btrbk.conf(5) for detailed documentation.\n\n";

    // Global settings
    out << "# Snapshot and backup retention policy\n";
    out << "snapshot_preserve_min   latest\n";
    out << "snapshot_preserve       14d\n\n";

    out << "target_preserve_min     latest\n";
    out << "target_preserve         30d 4w 6m\n\n";

    out << "# Lockfile\n";
    out << "lockfile                /var/lock/btrbk.lock\n\n";

    out << "# Use raw send/receive for efficiency\n";
    out << "stream_compress         no\n\n";

    // Targets
    for (const QString &target : std::as_const(targets)) {
        out << "target  " << target << "\n";
    }
    out << "\n";

    // Sources — each volume with its subvolume
    for (const QString &source : std::as_const(sources)) {
        // For mount points like / or /home, the volume is the mount point
        // and the subvolume is determined by the BTRFS subvolume name.
        // In a typical CachyOS setup:
        //   / is subvolume @
        //   /home is subvolume @home
        out << "volume  " << source << "\n";

        // Derive subvolume name from mount point
        if (source == QLatin1String("/")) {
            out << "  subvolume  @\n";
        } else {
            // /home -> @home, /var -> @var, etc.
            const QString subvolName = QLatin1Char('@') + source.mid(1).replace(QLatin1Char('/'), QLatin1Char('-'));
            out << "  subvolume  " << subvolName << "\n";
        }
        out << "\n";
    }

    return config;
}

// ---------------------------------------------------------------------------
// detectBtrfsMounts — parse /proc/mounts for btrfs entries
// ---------------------------------------------------------------------------

QStringList SetupWizard::detectBtrfsMounts()
{
    QStringList mounts;

    QFile procMounts(QStringLiteral("/proc/mounts"));
    if (!procMounts.open(QIODevice::ReadOnly | QIODevice::Text)) {
        return mounts;
    }

    QTextStream in(&procMounts);
    while (!in.atEnd()) {
        const QString line = in.readLine();
        const QStringList fields = line.split(QLatin1Char(' '), Qt::SkipEmptyParts);
        if (fields.size() < 3) {
            continue;
        }

        const QString fsType = fields.at(2);
        const QString mountPoint = fields.at(1);

        if (fsType != QLatin1String("btrfs")) {
            continue;
        }

        // Skip pseudo-filesystem paths
        if (mountPoint.startsWith(QLatin1String("/proc"))
            || mountPoint.startsWith(QLatin1String("/sys"))
            || mountPoint.startsWith(QLatin1String("/dev"))) {
            continue;
        }

        // Avoid duplicates (btrfs can show multiple subvolumes under the same mount)
        if (!mounts.contains(mountPoint)) {
            mounts.append(mountPoint);
        }
    }

    mounts.sort();
    return mounts;
}

// ---------------------------------------------------------------------------
// detectTargetMounts — parse /proc/mounts for non-root block devices
// ---------------------------------------------------------------------------

QStringList SetupWizard::detectTargetMounts()
{
    QStringList targets;

    QFile procMounts(QStringLiteral("/proc/mounts"));
    if (!procMounts.open(QIODevice::ReadOnly | QIODevice::Text)) {
        return targets;
    }

    QTextStream in(&procMounts);
    while (!in.atEnd()) {
        const QString line = in.readLine();
        const QStringList fields = line.split(QLatin1Char(' '), Qt::SkipEmptyParts);
        if (fields.size() < 3) {
            continue;
        }

        const QString device = fields.at(0);
        const QString mountPoint = fields.at(1);
        const QString fsType = fields.at(2);

        // Only real block devices
        if (!device.startsWith(QLatin1String("/dev/"))) {
            continue;
        }

        // Skip the root filesystem
        if (mountPoint == QLatin1String("/")) {
            continue;
        }

        // Skip pseudo-filesystem paths
        if (mountPoint.startsWith(QLatin1String("/proc"))
            || mountPoint.startsWith(QLatin1String("/sys"))
            || mountPoint.startsWith(QLatin1String("/boot"))
            || mountPoint.startsWith(QLatin1String("/snap"))) {
            continue;
        }

        // Prefer BTRFS targets (btrbk requires btrfs on both ends for
        // send/receive), but show others as informational
        if (!targets.contains(mountPoint)) {
            const QString label = (fsType == QLatin1String("btrfs"))
                ? mountPoint
                : mountPoint + QStringLiteral(" (") + fsType + QStringLiteral(", not btrfs)");
            targets.append(label);
        }
    }

    targets.sort();
    return targets;
}
