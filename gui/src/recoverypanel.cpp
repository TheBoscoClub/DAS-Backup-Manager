#include "recoverypanel.h"
#include "dbusclient.h"

#include <KColorScheme>
#include <KLocalizedString>

#include <QCheckBox>
#include <QClipboard>
#include <QComboBox>
#include <QDateTime>
#include <QDateTimeEdit>
#include <QGroupBox>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QLabel>
#include <QMessageBox>
#include <QProcess>
#include <QPushButton>
#include <QRadioButton>
#include <QScrollArea>
#include <QStandardPaths>
#include <QTimer>
#include <QVBoxLayout>

namespace {
const auto Operation = QStringLiteral("Recovery OS session");
const auto Both = QStringLiteral("both");
constexpr int RefreshMs = 30000;
// The script's own words, shown before an attended session of a `will` OS.
const auto Banner =
    QStringLiteral("this OS runs btrbk at boot; the guard is stopping it now; disable it in this session");

QString orWords(const QString &value, const QString &absent)
{
    return value.isEmpty() ? absent : value;
}

// Qt wraps a tooltip only when it is rich text; a plain one runs off the window.
QString tip(const QString &text)
{
    return QStringLiteral("<qt>%1</qt>").arg(text.toHtmlEscaped());
}

QString negative(const QString &text)
{
    const QColor c = KColorScheme(QPalette::Active).foreground(KColorScheme::NegativeText).color();
    return QStringLiteral("<span style='color:%1'>%2</span>").arg(c.name(), text.toHtmlEscaped());
}
} // namespace

RecoveryPanel::RecoveryPanel(DBusClient *client, QWidget *parent)
    : QWidget(parent)
    , m_client(client)
{
    auto *layout = new QVBoxLayout(this);
    // The cards scroll: a word-wrapped card must take its natural height,
    // not be clipped to the window's share.
    auto *scroll = new QScrollArea(this);
    scroll->setWidgetResizable(true);
    scroll->setFrameShape(QFrame::NoFrame);
    auto *cardsHost = new QWidget(scroll);
    m_cards = new QVBoxLayout(cardsHost);
    m_cards->addStretch();
    scroll->setWidget(cardsHost);
    layout->addWidget(scroll, 1);
    buildControls();
    m_status = new QLabel(this);
    m_status->setObjectName(QStringLiteral("statusLine"));
    m_status->setWordWrap(true);
    layout->addWidget(m_status);

    m_timer = new QTimer(this);
    m_timer->setInterval(RefreshMs);
    connect(m_timer, &QTimer::timeout, this, &RecoveryPanel::refresh);

    connect(m_client, &DBusClient::recoveryOsStatusResult, this, &RecoveryPanel::onStatusResult);
    connect(m_client, &DBusClient::recoveryOsStatusError, this, &RecoveryPanel::onStatusError);
    connect(m_client, &DBusClient::jobStarted, this, &RecoveryPanel::onJobStarted);
    connect(m_client, &DBusClient::jobFinished, this, &RecoveryPanel::onJobFinished);
    connect(m_client, &DBusClient::recoveryOsSessionEndResult, this, &RecoveryPanel::onSessionEndResult);
    connect(m_client, &DBusClient::recoveryOsScheduleResult, this, &RecoveryPanel::onScheduleResult);
    connect(m_client, &DBusClient::recoveryOsConsoleResult, this, &RecoveryPanel::onConsoleResult);
    m_confirm = [this](const QString &title, const QString &text) {
        return QMessageBox::warning(this, title, text, QMessageBox::Yes | QMessageBox::No, QMessageBox::No)
            == QMessageBox::Yes;
    };
    connect(m_client, &DBusClient::errorOccurred, this, [this](const QString &op, const QString &) {
        // A refused RecoveryOsSession call: the request is over, nothing runs.
        if (op == Operation)
            m_jobRequested = false;
        if (op == QLatin1String("RecoveryOsScheduleSet") || op == QLatin1String("RecoveryOsConsole"))
            refresh();
        rederive();
    });

    if (!m_client->isAvailable()) {
        m_parseError = m_client->unavailableReason();
        m_status->setText(m_parseError);
    }
    rederive();
}

void RecoveryPanel::buildControls()
{
    auto *outer = static_cast<QVBoxLayout *>(layout());
    auto *row = new QHBoxLayout;

    m_selector = new QComboBox(this);
    m_selector->setObjectName(QStringLiteral("selector"));
    m_selector->setToolTip(i18n("Which recovery drive the buttons act on, or both drives in one run"));
    row->addWidget(m_selector);

    m_attended = new QRadioButton(i18n("Attended"), this);
    m_attended->setObjectName(QStringLiteral("attended"));
    m_attended->setToolTip(i18n("The console opens; you log in and run the checklist. Required for a drive's "
                                "first session, where you install and enable qemu-guest-agent"));
    m_attended->setChecked(true);
    m_unattended = new QRadioButton(i18n("Unattended"), this);
    m_unattended->setObjectName(QStringLiteral("unattended"));
    m_unattended->setToolTip(i18n("The recovery OS upgrades itself through its guest agent and powers off; "
                                  "needs the agent installed and started at boot, and no 'will' record"));
    row->addWidget(m_attended);
    row->addWidget(m_unattended);

    m_sequential = new QRadioButton(i18n("One after the other"), this);
    m_sequential->setObjectName(QStringLiteral("sequential"));
    m_sequential->setToolTip(i18n("Both drives: the second starts only after the first ended clean — "
                                  "a bad update reaches one drive, not both"));
    m_sequential->setAutoExclusive(false);
    m_parallel = new QRadioButton(i18n("In parallel"), this);
    m_parallel->setObjectName(QStringLiteral("parallel"));
    m_parallel->setToolTip(i18n("Both drives at once: halves the time backups wait; the everyday choice "
                                "once each drive has 3 clean unattended runs"));
    m_parallel->setAutoExclusive(false);
    // The attended/unattended pair and the mode pair are two exclusive groups.
    connect(m_sequential, &QRadioButton::toggled, this, [this](bool on) {
        if (on)
            m_parallel->setChecked(false);
    });
    connect(m_parallel, &QRadioButton::toggled, this, [this](bool on) {
        if (on)
            m_sequential->setChecked(false);
    });
    row->addWidget(m_sequential);
    row->addWidget(m_parallel);

    m_now = new QCheckBox(i18n("Now"), this);
    m_now->setObjectName(QStringLiteral("now"));
    m_now->setToolTip(i18n("Start the session now; untick to pick a date and time for an unattended one"));
    m_now->setChecked(true);
    m_when = new QDateTimeEdit(QDateTime::currentDateTime().addSecs(3600), this);
    m_when->setObjectName(QStringLiteral("when"));
    m_when->setCalendarPopup(true);
    m_when->setToolTip(i18n("When the unattended session runs; at least 2 minutes ahead"));
    m_when->setEnabled(false);
    row->addWidget(m_now);
    row->addWidget(m_when);
    row->addStretch();
    outer->addLayout(row);

    auto *buttons = new QHBoxLayout;
    auto button = [this, buttons](const char *name, const QString &text, void (RecoveryPanel::*slot)()) {
        auto *b = new QPushButton(text, this);
        b->setObjectName(QLatin1String(name));
        b->setEnabled(false);
        connect(b, &QPushButton::clicked, this, slot);
        buttons->addWidget(b);
        return b;
    };
    m_upgrade = button("upgrade", i18n("Upgrade"), &RecoveryPanel::onUpgrade);
    m_schedule = button("schedule", i18n("Schedule"), &RecoveryPanel::onSchedule);
    m_clear = button("clearSchedule", i18n("Clear schedule"), &RecoveryPanel::onClearSchedule);
    m_console = button("console", i18n("Open console"), &RecoveryPanel::onConsole);
    m_end = button("endSession", i18n("End session"), &RecoveryPanel::onEndSession);
    buttons->addStretch();
    outer->addLayout(buttons);

    for (QWidget *w : std::initializer_list<QWidget *>{m_selector, m_attended, m_unattended, m_sequential, m_parallel, m_now, m_when})
        w->setToolTip(tip(w->toolTip()));

    connect(m_selector, &QComboBox::currentIndexChanged, this, &RecoveryPanel::rederive);
    connect(m_attended, &QRadioButton::toggled, this, &RecoveryPanel::rederive);
    connect(m_now, &QCheckBox::toggled, this, [this](bool now) {
        m_when->setEnabled(!now);
        rederive();
    });
    connect(m_when, &QDateTimeEdit::dateTimeChanged, this, &RecoveryPanel::rederive);
}

void RecoveryPanel::applyDocument(const QByteArray &json)
{
    QString err;
    auto doc = RecoveryDocument::parse(json, &err);
    if (!doc) {
        m_doc.reset();
        m_parseError = err;
    } else {
        // A parse error or a failed refresh the status line reported is over
        // once a document reads.
        if ((!m_parseError.isEmpty() && m_status->text() == m_parseError)
            || (!m_staleReason.isEmpty() && m_status->text() == m_staleReason))
            m_status->clear();
        m_parseError.clear();
        m_staleReason.clear();
        const int keep = m_selector->currentIndex();
        m_doc = std::move(doc);
        m_selector->blockSignals(true);
        m_selector->clear();
        for (const DriveView &d : m_doc->drives)
            m_selector->addItem(orWords(d.displayName, d.label), d.label);
        if (m_doc->drives.size() == 2)
            m_selector->addItem(i18n("Both drives"), Both);
        m_selector->setCurrentIndex(keep >= 0 && keep < m_selector->count() ? keep : 0);
        m_selector->blockSignals(false);
        if (!m_sequential->isChecked() && !m_parallel->isChecked())
            (m_doc->pair.modeDefault == QLatin1String("parallel") ? m_parallel : m_sequential)->setChecked(true);
    }
    rebuildCards();
    rederive();
}

void RecoveryPanel::rebuildCards()
{
    for (QGroupBox *box : std::as_const(m_cardWidgets))
        delete box;
    m_cardWidgets.clear();
    if (!m_doc)
        return;
    const qint64 now = QDateTime::currentSecsSinceEpoch();
    auto addCard = [this](const QString &title, const QStringList &lines, const QStringList &errors) {
        auto *box = new QGroupBox(title, this);
        auto *v = new QVBoxLayout(box);
        auto *body = new QLabel(lines.join(QStringLiteral("<br>")), box);
        body->setTextFormat(Qt::RichText);
        body->setWordWrap(true);
        v->addWidget(body);
        if (!errors.isEmpty()) {
            auto *err = new QLabel(negative(errors.join(QStringLiteral(" / "))), box);
            err->setObjectName(QStringLiteral("errors"));
            err->setTextFormat(Qt::RichText);
            err->setWordWrap(true);
            v->addWidget(err);
        }
        m_cards->insertWidget(m_cards->count() - 1, box); // before the stretch
        m_cardWidgets << box;
    };
    const auto esc = [](const QString &s) { return s.toHtmlEscaped(); };
    for (const DriveView &d : m_doc->drives) {
        QStringList lines;
        QStringList errors;
        if (d.record) {
            const RecordView &r = *d.record;
            lines << i18n("OS: %1, installed %2", esc(orWords(r.os, i18n("unknown"))), esc(orWords(r.installed, i18n("unknown"))));
            lines << i18n("Last full upgrade: %1", esc(orWords(r.lastFullUpgrade, i18n("unknown"))));
            lines << i18n("Kernel: %1 (host %2)", esc(orWords(r.kernel, i18n("unknown"))), esc(orWords(r.hostKernel, i18n("unknown"))));
            if (r.packagesRead) {
                lines << i18n("btrfs-progs: %1 (host %2); btrbk: %3",
                              esc(orWords(r.btrfsProgs, i18n("not installed"))),
                              esc(orWords(r.hostBtrfsProgs, i18n("unknown"))),
                              esc(orWords(r.btrbk, i18n("not installed"))));
            } else {
                lines << i18n("btrfs-progs, btrbk: package database not read");
            }
            lines << i18n("Guest agent: %1", esc(orWords(r.guestAgent.why, orWords(r.guestAgent.state, i18n("unknown")))));
        } else if (d.recordError) {
            errors << i18n("record: %1", *d.recordError);
        }
        if (d.assessment) {
            const AssessmentView &a = *d.assessment;
            const QString age = a.ageDays ? i18n("%1 days", *a.ageDays) : i18n("unknown age");
            lines << (a.stale ? QStringLiteral("<b>%1</b>").arg(i18n("STALE (%1): %2", age, esc(a.reasons.join(QStringLiteral("; ")))))
                              : i18n("Current (%1)", age));
            if (!a.warnings.isEmpty())
                lines << i18n("Warnings: %1", esc(a.warnings.join(QStringLiteral("; "))));
        }
        lines << i18n("Boot: %1", esc(verdictWords(d.verdict)));
        lines << i18n("Record age: %1", esc(ageWords(d.checkedEpoch, now)));
        if (d.cleanRuns)
            lines << i18n("Clean unattended runs: %1", *d.cleanRuns);
        else if (d.cleanRunsError)
            errors << i18n("clean runs: %1", *d.cleanRunsError);
        if (d.historyError)
            errors << i18n("history: %1", *d.historyError);
        if (d.schedule)
            lines << i18n("Schedule: %1", esc(scheduleWords(*d.schedule)));
        else if (d.scheduleError)
            errors << i18n("schedule: %1", *d.scheduleError);
        else
            lines << i18n("Schedule: none");
        if (d.session)
            lines << i18n("Session: %1", esc(sessionWords(*d.session, now)));
        else if (d.sessionError)
            errors << i18n("session: %1", *d.sessionError);
        else
            lines << i18n("Session: none");
        if (d.due)
            lines << QStringLiteral("<b>%1</b>").arg(i18n("UPDATE DUE"));
        // The configured display name often carries the serial already.
        QStringList missing;
        for (const QString &serial : d.serials)
            if (!d.displayName.contains(serial))
                missing << serial;
        const QString name = orWords(d.displayName, d.label);
        addCard(missing.isEmpty() ? name : QStringLiteral("%1 (%2)").arg(name, missing.join(QStringLiteral(", "))), lines, errors);
    }
    if (m_doc->drives.size() == 2) {
        const PairView &p = m_doc->pair;
        QStringList lines;
        QStringList errors;
        lines << i18n("Default for both drives: %1", esc(orWords(p.modeDefault, i18n("unknown"))));
        if (p.schedule)
            lines << i18n("Schedule: %1", esc(scheduleWords(*p.schedule)));
        else if (p.scheduleError)
            errors << i18n("schedule: %1", *p.scheduleError);
        else
            lines << i18n("Schedule: none");
        if (p.session)
            lines << i18n("Session: %1", esc(sessionWords(*p.session, now)));
        else if (p.sessionError)
            errors << i18n("session: %1", *p.sessionError);
        else
            lines << i18n("Session: none");
        addCard(i18n("Both drives"), lines, errors);
    }
}

GuiFacts RecoveryPanel::facts() const
{
    GuiFacts g;
    g.ownJobId = m_ownJobId;
    g.ownJobAttended = m_ownJobAttended;
    g.viewerInstalled = !QStandardPaths::findExecutable(QStringLiteral("vncviewer")).isEmpty();
    g.nowEpoch = QDateTime::currentSecsSinceEpoch();
    g.chosenNow = m_now->isChecked();
    g.chosenEpoch = m_when->dateTime().toSecsSinceEpoch();
    g.unattended = m_unattended->isChecked();
    return g;
}

bool RecoveryPanel::selectionIsBoth() const
{
    return m_selector->currentData().toString() == Both;
}

QStringList RecoveryPanel::selectedLabels() const
{
    if (!m_doc)
        return {};
    if (selectionIsBoth()) {
        QStringList both;
        for (const DriveView &d : m_doc->drives)
            both << d.label;
        return both;
    }
    const QString label = m_selector->currentData().toString();
    return label.isEmpty() ? QStringList{} : QStringList{label};
}

const DriveView *RecoveryPanel::selectedDrive() const
{
    if (!m_doc || selectionIsBoth())
        return nullptr;
    const QString label = m_selector->currentData().toString();
    for (const DriveView &d : m_doc->drives)
        if (d.label == label)
            return &d;
    return nullptr;
}

QString RecoveryPanel::chosenMode() const
{
    if (!selectionIsBoth())
        return {};
    return m_parallel->isChecked() ? QStringLiteral("parallel") : QStringLiteral("sequential");
}

void RecoveryPanel::rederive()
{
    const bool both = selectionIsBoth();
    m_sequential->setVisible(both);
    m_parallel->setVisible(both);
    auto bind = [](QPushButton *b, const Action &a) {
        b->setEnabled(a.enabled);
        b->setToolTip(tip(a.why));
    };
    if (!m_doc || !m_parseError.isEmpty()) {
        const Action off{false, m_parseError.isEmpty() ? i18n("no status document yet") : m_parseError};
        for (QPushButton *b : {m_upgrade, m_schedule, m_clear, m_console, m_end})
            bind(b, off);
        if (!m_parseError.isEmpty())
            m_status->setText(m_parseError);
        return;
    }
    if (!m_staleReason.isEmpty()) {
        // The cards show the last document; nothing may act on it.
        const Action stale{false, i18n("the last status refresh failed: %1", m_staleReason)};
        for (QPushButton *b : {m_upgrade, m_schedule, m_clear, m_console, m_end})
            bind(b, stale);
        m_status->setText(m_staleReason);
        return;
    }
    const GuiFacts g = facts();
    if (both) {
        const PairActions p = derivePairActions(*m_doc, g);
        bind(m_upgrade, p.upgrade);
        bind(m_schedule, p.schedule);
        bind(m_clear, p.clearSchedule);
        bind(m_console, {false, i18n("pick one drive to open its console")});
        bind(m_end, {false, i18n("pick one drive to end its session")});
    } else if (const DriveView *d = selectedDrive()) {
        const DriveActions a = deriveActions(*d, *m_doc, g);
        bind(m_upgrade, a.upgrade);
        bind(m_schedule, a.schedule);
        bind(m_clear, a.clearSchedule);
        bind(m_console, a.console);
        bind(m_end, a.endSession);
    }
    if (m_jobRequested || !m_ownJobId.isEmpty()) {
        m_upgrade->setEnabled(false);
        m_upgrade->setToolTip(i18n("this window's session is running"));
    }
    if (m_endInFlight) {
        m_end->setEnabled(false);
        m_end->setText(i18n("Ending…"));
    } else {
        m_end->setText(i18n("End session"));
    }
}

void RecoveryPanel::onUpgrade()
{
    const QStringList labels = selectedLabels();
    if (labels.isEmpty())
        return;
    const bool unattended = m_unattended->isChecked();
    if (!unattended) {
        // The banner, for every selected drive whose record says `will` —
        // the rules decide (bannerNeeded), the panel only asks.
        const GuiFacts g = facts();
        QStringList will;
        for (const DriveView &d : m_doc->drives)
            if (labels.contains(d.label) && deriveActions(d, *m_doc, g).bannerNeeded)
                will << d.displayName;
        if (!will.isEmpty()
            && !m_confirm(i18n("This recovery OS runs btrbk at boot"),
                          i18n("%1: %2\n\nThe session boots it under the guard. At the console, disable what "
                               "the record's reasons name before you power off. Continue?",
                               will.join(QStringLiteral(", ")), Banner)))
            return;
    }
    m_jobRequested = true;
    m_ownJobAttended = !unattended;
    rederive();
    // accept_boot_record_risk is never passed from the GUI (the client hard-codes false).
    m_client->recoveryOsSession(labels, unattended, chosenMode());
}

void RecoveryPanel::onSchedule()
{
    const QStringList labels = selectedLabels();
    if (labels.isEmpty())
        return;
    const qint64 at = m_when->dateTime().toSecsSinceEpoch();
    std::optional<ScheduleView> existing;
    if (selectionIsBoth())
        existing = m_doc->pair.schedule;
    else if (const DriveView *d = selectedDrive())
        existing = d->schedule;
    if (existing && (existing->state == QLatin1String("pending") || existing->state == QLatin1String("missed"))) {
        const QString when = existing->atEpoch
            ? QDateTime::fromSecsSinceEpoch(*existing->atEpoch).toString(Qt::TextDate)
            : i18n("a time that cannot be read");
        if (QMessageBox::question(this, i18n("Replace the schedule?"),
                                  i18n("%1 is already scheduled for %2. Replace it?", existing->unit, when))
            != QMessageBox::Yes)
            return;
    }
    m_client->recoveryOsScheduleSet(labels, at, chosenMode());
}

void RecoveryPanel::onClearSchedule()
{
    const QStringList labels = selectedLabels();
    if (!labels.isEmpty())
        m_client->recoveryOsScheduleSet(labels, 0, QString());
}

void RecoveryPanel::onConsole()
{
    const QStringList labels = selectedLabels();
    if (labels.size() == 1)
        m_client->recoveryOsConsole(labels.first());
}

void RecoveryPanel::onConsoleResult(const QString &label, const QString &path)
{
    const auto [program, args] = DBusClient::consoleCommand(path);
    if (!QStandardPaths::findExecutable(program).isEmpty() && QProcess::startDetached(program, args)) {
        m_status->setText(i18n("Console of %1 opened in %2", label, program));
        return;
    }
    QMessageBox box(QMessageBox::Information, i18n("Console socket"),
                    i18n("%1 is not installed (package tigervnc). Connect any VNC viewer that speaks "
                         "UNIX sockets to:\n\n%2\n\nThe socket accepts one connection and is removed when "
                         "the drive is given back.",
                         program, path),
                    QMessageBox::Ok, this);
    QPushButton *copy = box.addButton(i18n("Copy path"), QMessageBox::ActionRole);
    copy->setToolTip(tip(i18n("Copy the socket's path to the clipboard")));
    box.exec();
    if (box.clickedButton() == copy)
        QGuiApplication::clipboard()->setText(path);
}

void RecoveryPanel::onEndSession()
{
    const QStringList labels = selectedLabels();
    if (labels.size() != 1)
        return;
    m_endInFlight = true;
    rederive();
    m_client->recoveryOsSessionEnd(labels.first());
}

void RecoveryPanel::onSessionEndResult(const QString &label, bool ok, const QString &lines)
{
    m_endInFlight = false;
    m_status->setText(ok ? i18n("Session of %1 ended:\n%2", label, lines)
                         : i18n("Ending the session of %1 failed:\n%2", label, lines));
    rederive();
    refresh();
}

void RecoveryPanel::onScheduleResult(const QString &unit)
{
    m_status->setText(i18n("Schedule written: %1", unit));
    refresh();
}

void RecoveryPanel::setConfirmer(Confirmer confirmer)
{
    m_confirm = std::move(confirmer);
}

void RecoveryPanel::onJobStarted(const QString &jobId, const QString &operation)
{
    // Any job may be a session another window started: the document says.
    refresh();
    if (operation != Operation)
        return;
    if (m_earlyFinished.remove(jobId)) {
        // Finished before its reply arrived (a helper refusal): over already.
        m_jobRequested = false;
        rederive();
        return;
    }
    if (!m_jobRequested)
        return; // another window's session: the document will show it
    m_jobRequested = false;
    m_ownJobId = jobId;
    rederive();
}

void RecoveryPanel::onJobFinished(const QString &jobId, bool success, const QString &summary)
{
    if (m_ownJobId.isEmpty() && m_jobRequested)
        m_earlyFinished.insert(jobId);
    if (jobId != m_ownJobId) {
        refresh();
        return;
    }
    m_ownJobId.clear();
    if (success)
        m_status->setText(i18n("Done: %1", summary));
    else if (summary.startsWith(QLatin1String("warnings:")))
        m_status->setText(i18n("Needs a look: %1", summary));
    else
        m_status->setText(i18n("Failed: %1", summary));
    rederive();
    refresh();
}

void RecoveryPanel::refresh()
{
    if (m_refreshInFlight)
        return;
    m_refreshInFlight = true;
    m_client->recoveryOsStatusAsync();
}

void RecoveryPanel::onStatusResult(const QString &json)
{
    m_refreshInFlight = false;
    if (json.isEmpty())
        return; // unavailable, or the error came by onStatusError
    applyDocument(json.toUtf8());
}

void RecoveryPanel::onStatusError(const QString &reason)
{
    m_refreshInFlight = false;
    m_staleReason = reason;
    rederive();
}

void RecoveryPanel::setShown(bool shown)
{
    if (shown) {
        refresh();
        m_timer->start();
    } else {
        m_timer->stop();
    }
}

QString RecoveryPanel::statusLine() const
{
    return m_status->text();
}
