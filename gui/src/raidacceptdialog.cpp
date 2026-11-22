#include "raidacceptdialog.h"
#include "dbusclient.h"

#include <KLocalizedString>

#include <QFontDatabase>
#include <QHBoxLayout>
#include <QLabel>
#include <QLineEdit>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QVBoxLayout>

RaidAcceptDialog::RaidAcceptDialog(DBusClient *client, const QString &label, QWidget *parent)
    : QDialog(parent)
    , m_client(client)
    , m_label(label)
{
    setWindowTitle(i18n("Accept the Surviving RAID-1 Drive — %1", label));
    resize(800, 520);

    auto *layout = new QVBoxLayout(this);

    auto *intro = new QLabel(
        i18n("Use this only when the RAID-1 guard refuses target '%1' for split brain because "
             "the other drive is dead. Accepting makes the drive present the target's history "
             "and gives up everything only the absent drive holds; the absent drive is then "
             "refused until it is wiped or removed from the filesystem.",
             label),
        this);
    intro->setWordWrap(true);
    layout->addWidget(intro);

    m_explanation = new QPlainTextEdit(this);
    m_explanation->setObjectName(QStringLiteral("raidAcceptExplanation"));
    m_explanation->setReadOnly(true);
    m_explanation->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    m_explanation->setToolTip(
        i18n("What the drive present holds and what accepting gives up, as 'btrdasd raid "
             "accept' shows it"));
    layout->addWidget(m_explanation, 1);

    auto *typedRow = new QHBoxLayout();
    auto *typedLabel = new QLabel(i18n("Type the target label to accept:"), this);
    m_typed = new QLineEdit(this);
    m_typed->setObjectName(QStringLiteral("raidAcceptLabelEdit"));
    m_typed->setPlaceholderText(label);
    m_typed->setToolTip(i18n("Type the target label '%1' exactly; nothing else accepts", label));
    typedRow->addWidget(typedLabel);
    typedRow->addWidget(m_typed, 1);
    layout->addLayout(typedRow);

    m_statusLabel = new QLabel(this);
    m_statusLabel->setObjectName(QStringLiteral("raidAcceptStatus"));
    m_statusLabel->setWordWrap(true);
    layout->addWidget(m_statusLabel);

    auto *buttons = new QHBoxLayout();
    m_refreshButton = new QPushButton(QIcon::fromTheme(QStringLiteral("view-refresh")),
                                      i18n("Refresh"), this);
    m_refreshButton->setToolTip(i18n("Look at the target again"));
    m_acceptButton = new QPushButton(QIcon::fromTheme(QStringLiteral("dialog-warning")),
                                     i18n("Accept Surviving Drive"), this);
    m_acceptButton->setObjectName(QStringLiteral("raidAcceptButton"));
    m_acceptButton->setToolTip(
        i18n("Make the drive present this target's history and refuse the absent drive from "
             "now on (requires administrator authentication every time)"));
    m_closeButton = new QPushButton(QIcon::fromTheme(QStringLiteral("dialog-close")),
                                    i18n("Close"), this);
    m_closeButton->setToolTip(i18n("Close this window without accepting"));
    buttons->addWidget(m_refreshButton);
    buttons->addStretch(1);
    buttons->addWidget(m_acceptButton);
    buttons->addWidget(m_closeButton);
    layout->addLayout(buttons);

    connect(m_refreshButton, &QPushButton::clicked, this, &RaidAcceptDialog::refresh);
    connect(m_acceptButton, &QPushButton::clicked, this, &RaidAcceptDialog::acceptTyped);
    connect(m_closeButton, &QPushButton::clicked, this, &QDialog::close);
    connect(m_typed, &QLineEdit::textChanged, this, &RaidAcceptDialog::updateAcceptEnabled);

    updateAcceptEnabled();
    refresh();
}

bool RaidAcceptDialog::applyPreview(bool ok, const QString &text)
{
    m_previewOk = ok;
    if (ok) {
        m_explanation->setPlainText(text);
        m_statusLabel->setText(QString());
    } else {
        m_explanation->clear();
        m_statusLabel->setText(i18n("Nothing to accept: %1", text));
    }
    updateAcceptEnabled();
    return ok;
}

bool RaidAcceptDialog::acceptOffered() const
{
    return m_previewOk && m_typed->text() == m_label;
}

void RaidAcceptDialog::refresh()
{
    const DBusClient::RaidReply reply = m_client->raidAcceptPreview(m_label);
    applyPreview(reply.ok, reply.text);
}

void RaidAcceptDialog::updateAcceptEnabled()
{
    m_acceptButton->setEnabled(acceptOffered());
}

void RaidAcceptDialog::acceptTyped()
{
    if (!acceptOffered())
        return;
    // The helper compares the typed text with the label itself; this button
    // only spares a round trip that would be refused.
    const DBusClient::RaidReply reply = m_client->raidAccept(m_label, m_typed->text());
    if (reply.ok) {
        m_previewOk = false;
        m_typed->clear();
        m_explanation->setPlainText(reply.text);
        m_statusLabel->setText(i18n("Accepted."));
    } else {
        m_statusLabel->setText(i18n("Not accepted: %1", reply.text));
    }
    updateAcceptEnabled();
}
