#include "configdialog.h"
#include "dbusclient.h"

#include <KLocalizedString>
#include <KMessageBox>
#include <KStandardGuiItem>

#include <QDialogButtonBox>
#include <QFontDatabase>
#include <QFontMetrics>
#include <QHBoxLayout>
#include <QLabel>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QTextOption>
#include <QVBoxLayout>
#include <QWidget>

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

ConfigDialog::ConfigDialog(DBusClient *client, QWidget *parent)
    : KPageDialog(parent)
    , m_client(client)
    , m_configPath(QStringLiteral("/etc/das-backup/config.toml"))
{
    setWindowTitle(i18n("Configuration Editor"));
    resize(800, 600);

    // Single Close button — saving is handled by the in-dialog Save button so
    // that we can show a diff confirmation before committing anything.
    setStandardButtons(QDialogButtonBox::Close);

    // -----------------------------------------------------------------------
    // Build the single page widget
    // -----------------------------------------------------------------------
    auto *page = new QWidget(this);
    auto *mainLayout = new QVBoxLayout(page);
    mainLayout->setContentsMargins(8, 8, 8, 8);
    mainLayout->setSpacing(6);

    // --- Toolbar row --------------------------------------------------------
    auto *toolbarRow = new QHBoxLayout();
    toolbarRow->setSpacing(6);

    auto *reloadButton = new QPushButton(
        QIcon::fromTheme(QStringLiteral("view-refresh")),
        i18n("Reload"), page);
    reloadButton->setToolTip(i18n("Discard edits and reload from disk"));

    m_diffButton = new QPushButton(
        QIcon::fromTheme(QStringLiteral("vcs-diff")),
        i18n("Show Diff"), page);
    m_diffButton->setToolTip(i18n("Show differences between the current editor content and the saved configuration"));

    m_saveButton = new QPushButton(
        QIcon::fromTheme(QStringLiteral("document-save")),
        i18n("Save"), page);
    m_saveButton->setToolTip(i18n("Save configuration (requires administrator privileges)"));
    // Make the Save button visually prominent
    m_saveButton->setDefault(false);
    QFont boldFont = m_saveButton->font();
    boldFont.setBold(true);
    m_saveButton->setFont(boldFont);

    toolbarRow->addWidget(reloadButton);
    toolbarRow->addStretch(1);
    toolbarRow->addWidget(m_diffButton);
    toolbarRow->addWidget(m_saveButton);
    mainLayout->addLayout(toolbarRow);

    // --- Editor -------------------------------------------------------------
    m_editor = new QPlainTextEdit(page);
    m_editor->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    m_editor->setWordWrapMode(QTextOption::NoWrap);
    // Set tab stop to 4 characters wide
    {
        const QFontMetrics fm(m_editor->font());
        m_editor->setTabStopDistance(4.0 * fm.horizontalAdvance(QLatin1Char(' ')));
    }
    mainLayout->addWidget(m_editor, 1);

    // --- Status label -------------------------------------------------------
    m_statusLabel = new QLabel(page);
    m_statusLabel->setWordWrap(true);
    mainLayout->addWidget(m_statusLabel);

    // -----------------------------------------------------------------------
    // Register the page with KPageDialog
    // -----------------------------------------------------------------------
    addPage(page, i18n("DAS Backup Configuration"));

    // -----------------------------------------------------------------------
    // Signal connections
    // -----------------------------------------------------------------------
    connect(reloadButton, &QPushButton::clicked, this, &ConfigDialog::loadConfig);
    connect(m_diffButton, &QPushButton::clicked, this, &ConfigDialog::showDiff);
    connect(m_saveButton, &QPushButton::clicked, this, &ConfigDialog::saveConfig);

    loadConfig();
}

// ---------------------------------------------------------------------------
// loadConfig
// ---------------------------------------------------------------------------

void ConfigDialog::loadConfig()
{
    m_statusLabel->clear();

    const QString content = m_client->configGet();

    if (content.isEmpty()) {
        const QString placeholder = QStringLiteral(
            "# Could not load configuration.\n"
            "# Ensure btrdasd-helper service is running.");
        m_editor->setPlainText(placeholder);
        m_originalContent.clear();
        m_saveButton->setEnabled(false);
        m_diffButton->setEnabled(false);
        m_statusLabel->setText(i18n("Failed to load configuration. Is btrdasd-helper running?"));
    } else {
        m_editor->setPlainText(content);
        m_originalContent = content;
        m_saveButton->setEnabled(true);
        m_diffButton->setEnabled(true);
        m_statusLabel->setText(i18n("Loaded: %1", m_configPath));
    }
}

// ---------------------------------------------------------------------------
// showDiff — line-by-line unified-style comparison
// ---------------------------------------------------------------------------

void ConfigDialog::showDiff()
{
    const QString currentContent = m_editor->toPlainText();

    if (currentContent == m_originalContent) {
        KMessageBox::information(this, i18n("No changes made."), i18n("No Changes"));
        return;
    }

    const QStringList originalLines = m_originalContent.split(QLatin1Char('\n'));
    const QStringList currentLines  = currentContent.split(QLatin1Char('\n'));

    // Simple O(n) line-by-line diff: walk both lists in parallel.
    // For a config editor this level of detail is sufficient — the file is
    // small and the user wants a readable overview, not a git-quality patch.
    QString diffText;
    const int maxLines = qMax(originalLines.size(), currentLines.size());
    for (int i = 0; i < maxLines; ++i) {
        const bool hasOrig    = (i < originalLines.size());
        const bool hasCurrent = (i < currentLines.size());

        if (hasOrig && hasCurrent) {
            if (originalLines.at(i) == currentLines.at(i)) {
                diffText += QLatin1String("  ") + originalLines.at(i) + QLatin1Char('\n');
            } else {
                diffText += QLatin1String("- ") + originalLines.at(i) + QLatin1Char('\n');
                diffText += QLatin1String("+ ") + currentLines.at(i)  + QLatin1Char('\n');
            }
        } else if (hasOrig) {
            diffText += QLatin1String("- ") + originalLines.at(i) + QLatin1Char('\n');
        } else {
            diffText += QLatin1String("+ ") + currentLines.at(i) + QLatin1Char('\n');
        }
    }

    // Show the diff in a small resizable child dialog
    auto *diffDialog = new QDialog(this);
    diffDialog->setWindowTitle(i18n("Configuration Diff"));
    diffDialog->resize(700, 500);
    diffDialog->setAttribute(Qt::WA_DeleteOnClose);

    auto *diffLayout = new QVBoxLayout(diffDialog);

    auto *diffView = new QPlainTextEdit(diffDialog);
    diffView->setReadOnly(true);
    diffView->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    diffView->setWordWrapMode(QTextOption::NoWrap);
    diffView->setPlainText(diffText);
    diffLayout->addWidget(diffView, 1);

    auto *btnBox = new QDialogButtonBox(QDialogButtonBox::Close, diffDialog);
    connect(btnBox, &QDialogButtonBox::rejected, diffDialog, &QDialog::reject);
    diffLayout->addWidget(btnBox);

    diffDialog->exec();
}

// ---------------------------------------------------------------------------
// savedStatus
// ---------------------------------------------------------------------------

QString ConfigDialog::savedStatus(const QString &typed, const QString &written)
{
    if (written.isEmpty())
        return i18n("Saved, but the file could not be read back — press Reload.");
    if (written != typed)
        return i18n("Saved. Settings your text left out that decide what is backed up, "
                    "where and for how long were written into the file with their current "
                    "values; the editor shows the file as saved.");
    return i18n("Configuration saved successfully.");
}

// ---------------------------------------------------------------------------
// saveConfig
// ---------------------------------------------------------------------------

void ConfigDialog::saveConfig()
{
    const QString currentContent = m_editor->toPlainText();

    if (currentContent == m_originalContent) {
        KMessageBox::information(this, i18n("No changes to save."), i18n("No Changes"));
        return;
    }

    // Build a compact change-count summary for the confirmation message.
    const QStringList originalLines = m_originalContent.split(QLatin1Char('\n'));
    const QStringList currentLines  = currentContent.split(QLatin1Char('\n'));

    int added   = 0;
    int removed = 0;
    int changed = 0;
    const int maxLines = qMax(originalLines.size(), currentLines.size());
    for (int i = 0; i < maxLines; ++i) {
        const bool hasOrig    = (i < originalLines.size());
        const bool hasCurrent = (i < currentLines.size());

        if (hasOrig && hasCurrent) {
            if (originalLines.at(i) != currentLines.at(i)) {
                ++changed;
            }
        } else if (hasOrig) {
            ++removed;
        } else {
            ++added;
        }
    }

    QString summary = i18n(
        "Save changes to %1?\n\n"
        "Lines changed: %2\n"
        "Lines added:   %3\n"
        "Lines removed: %4\n\n"
        "This operation requires administrator privileges (polkit).",
        m_configPath, changed, added, removed);
    // Keys nothing reads (a typo is kept and silently ignored): said before
    // saving, which still goes ahead — a newer version's keys belong here.
    const std::optional<QStringList> warnings = m_client->configCheck(currentContent);
    if (warnings && !warnings->isEmpty()) {
        summary += QLatin1String("\n\n")
            + i18n("Warnings (the file is saved as typed, but these are not read):")
            + QLatin1String("\n- ") + warnings->join(QLatin1String("\n- "));
    }

    const auto answer = KMessageBox::questionTwoActions(
        this,
        summary,
        i18n("Save Configuration"),
        KGuiItem(i18n("Save"), QStringLiteral("document-save")),
        KStandardGuiItem::cancel());

    if (answer != KMessageBox::PrimaryAction) {
        return;
    }

    const bool ok = m_client->configSet(currentContent);
    if (ok) {
        // Show what was written, not what was typed: a save writes out the
        // keys that decide what is backed up, where and for how long when the
        // text leaves them out (bd DAS-Backup-Manager-28il).
        loadConfig();
        m_statusLabel->setText(savedStatus(currentContent, m_originalContent));
    } else {
        // DBusClient emits errorOccurred, which the main window shows in a
        // message box with the helper's reason — among them "a backup or
        // btrdasd setup is running … Try again when it finishes" (bd lxw).
        // The edits stay in the editor, so Save can simply be pressed again.
        m_statusLabel->setText(i18n("Not saved — see the error message for why. "
                                    "Your edits are kept; save again once the cause is gone."));
    }
}
