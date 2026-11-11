#include "coveragedialog.h"
#include "dbusclient.h"

#include <KLocalizedString>
#include <KMessageBox>

#include <QAbstractItemView>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QItemSelectionModel>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QPushButton>
#include <QTableWidget>
#include <QVBoxLayout>

namespace {
enum Column { Volume = 0, Subvolume, Target, Since, Why, ColumnCount };

// The raw subvolume name, kept beside the cell's display text: the top level
// is shown as the CLI writes it but accepted by its name, `.`.
constexpr int RawNameRole = Qt::UserRole + 1;

// `.` is the volume's top level (adopt::display_name).
QString displayName(const QString &subvolume)
{
    return subvolume == QLatin1String(".") ? QStringLiteral(". (top level)") : subvolume;
}
} // namespace

CoverageDialog::CoverageDialog(DBusClient *client, QWidget *parent)
    : QDialog(parent)
    , m_client(client)
{
    setWindowTitle(i18n("Subvolume Coverage"));
    resize(900, 400);

    auto *layout = new QVBoxLayout(this);

    auto *intro = new QLabel(
        i18n("These subvolume/target pairs were backed up by an earlier run and are no longer "
             "scheduled, though the subvolume is still on disk. Each backup run warns about them "
             "until the pair is scheduled again, its subvolume is deleted, or you accept it here. "
             "Accept only a pair you meant to stop backing up."),
        this);
    intro->setWordWrap(true);
    layout->addWidget(intro);

    m_table = new QTableWidget(0, ColumnCount, this);
    m_table->setObjectName(QStringLiteral("coverageTable"));
    m_table->setHorizontalHeaderLabels({i18n("Volume"), i18n("Subvolume"), i18n("Target"),
                                        i18n("No longer backed up since"), i18n("Why")});
    m_table->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_table->setSelectionMode(QAbstractItemView::SingleSelection);
    m_table->setEditTriggers(QAbstractItemView::NoEditTriggers);
    m_table->verticalHeader()->setVisible(false);
    m_table->horizontalHeader()->setStretchLastSection(true);
    layout->addWidget(m_table, 1);

    m_statusLabel = new QLabel(this);
    m_statusLabel->setWordWrap(true);
    layout->addWidget(m_statusLabel);

    auto *buttons = new QHBoxLayout();
    m_refreshButton = new QPushButton(QIcon::fromTheme(QStringLiteral("view-refresh")),
                                      i18n("Refresh"), this);
    m_refreshButton->setToolTip(i18n("Read the coverage record again"));
    m_acceptButton = new QPushButton(QIcon::fromTheme(QStringLiteral("dialog-ok-apply")),
                                     i18n("Accept Selected Pair"), this);
    m_acceptButton->setObjectName(QStringLiteral("coverageAcceptButton"));
    m_acceptButton->setToolTip(
        i18n("Stop reporting the selected pair (volume, subvolume, target) as no longer backed "
             "up; it is kept in the accept log (requires administrator privileges)"));
    m_closeButton = new QPushButton(QIcon::fromTheme(QStringLiteral("dialog-close")),
                                    i18n("Close"), this);
    m_closeButton->setToolTip(i18n("Close this window"));
    buttons->addWidget(m_refreshButton);
    buttons->addStretch(1);
    buttons->addWidget(m_acceptButton);
    buttons->addWidget(m_closeButton);
    layout->addLayout(buttons);

    connect(m_refreshButton, &QPushButton::clicked, this, &CoverageDialog::refresh);
    connect(m_acceptButton, &QPushButton::clicked, this, &CoverageDialog::acceptSelected);
    connect(m_closeButton, &QPushButton::clicked, this, &QDialog::close);
    connect(m_table->selectionModel(), &QItemSelectionModel::selectionChanged,
            this, &CoverageDialog::updateAcceptEnabled);

    updateAcceptEnabled();
    refresh();
}

bool CoverageDialog::applyJson(const QString &json)
{
    m_table->clearSelection();
    m_table->setRowCount(0);
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    if (!doc.isObject() || !doc.object().value(QStringLiteral("dropped")).isArray()) {
        m_statusLabel->setText(i18n("The helper's reply is not a coverage list: nothing shown."));
        updateAcceptEnabled();
        return false;
    }
    const QJsonObject root = doc.object();
    const QJsonArray dropped = root.value(QStringLiteral("dropped")).toArray();
    for (const QJsonValue &v : dropped) {
        const QJsonObject r = v.toObject();
        const int row = m_table->rowCount();
        m_table->insertRow(row);
        const QString subvolume = r.value(QStringLiteral("subvolume")).toString();
        auto *name = new QTableWidgetItem(displayName(subvolume));
        name->setData(RawNameRole, subvolume);
        m_table->setItem(row, Volume, new QTableWidgetItem(r.value(QStringLiteral("volume")).toString()));
        m_table->setItem(row, Subvolume, name);
        m_table->setItem(row, Target, new QTableWidgetItem(r.value(QStringLiteral("target")).toString()));
        m_table->setItem(row, Since, new QTableWidgetItem(r.value(QStringLiteral("dropped_since")).toString()));
        const QString why = r.value(QStringLiteral("drop_rule")).toString();
        auto *whyItem = new QTableWidgetItem(why);
        whyItem->setToolTip(why);
        m_table->setItem(row, Why, whyItem);
    }
    m_table->resizeColumnsToContents();
    const qsizetype accepted = root.value(QStringLiteral("accepted")).toArray().size();
    QString status = dropped.isEmpty()
        ? i18n("No pair is reported as no longer backed up.")
        : i18np("1 pair is no longer backed up.", "%1 pairs are no longer backed up.",
                dropped.size());
    if (root.value(QStringLiteral("state")).isNull())
        status += QLatin1Char(' ') + i18n("No coverage record yet: the next backup run records the first.");
    if (accepted > 0)
        status += QLatin1Char(' ')
            + i18np("1 pair accepted earlier.", "%1 pairs accepted earlier.", accepted);
    m_statusLabel->setText(status);
    updateAcceptEnabled();
    return true;
}

void CoverageDialog::refresh()
{
    if (!m_client->isAvailable()) {
        m_table->setRowCount(0);
        m_statusLabel->setText(i18n("The backup helper is not available: %1",
                                    m_client->unavailableReason()));
        updateAcceptEnabled();
        return;
    }
    const std::optional<QString> json = m_client->coverageList();
    if (!json) {
        m_table->setRowCount(0);
        m_statusLabel->setText(i18n("The coverage record could not be read."));
        updateAcceptEnabled();
        return;
    }
    applyJson(*json);
}

void CoverageDialog::acceptSelected()
{
    const QModelIndexList rows = m_table->selectionModel()->selectedRows();
    if (rows.size() != 1)
        return;
    const int row = rows.first().row();
    const QString volume = m_table->item(row, Volume)->text();
    const QString subvolume = m_table->item(row, Subvolume)->data(RawNameRole).toString();
    const QString target = m_table->item(row, Target)->text();

    const auto answer = KMessageBox::warningContinueCancel(
        this,
        i18n("Stop reporting %1 %2 → %3 as no longer backed up?\n\n"
             "No backup run will warn about this pair again unless it is backed up and then "
             "dropped once more.",
             volume, displayName(subvolume), target),
        i18n("Accept Pair"));
    if (answer != KMessageBox::Continue)
        return;

    // A refusal (polkit, or a pair no longer reported) arrives as
    // errorOccurred; either way the list is read again, so it shows what
    // the record now holds.
    const std::optional<QString> done = m_client->coverageAccept(volume, subvolume, target);
    refresh();
    m_statusLabel->setText((done ? done->trimmed() : i18n("Not accepted: see the error shown."))
                           + QLatin1Char('\n') + m_statusLabel->text());
}

void CoverageDialog::updateAcceptEnabled()
{
    // Rows come only from the helper's reply, so a selection means it
    // answered; acceptSelected reports a call that fails.
    m_acceptButton->setEnabled(m_table->selectionModel()->selectedRows().size() == 1);
}
