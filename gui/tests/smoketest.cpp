// Headless smoke tests for the GUI (bd DAS-Backup-Manager-a59).
//
// The GUI had zero coverage: gui/CMakeLists.txt found Qt6::Test and included
// ECMAddTests, then added no tests at all, leaving a dependency that implied
// coverage which did not exist.
//
// Scope is deliberately smoke, not interaction. The GUI is a thin D-Bus view
// over btrdasd, whose logic is covered by the Rust suite; simulating clicks
// across every panel is what was deleted in March 2026 for being brittle. What
// is worth pinning here is the part with no Rust equivalent:
//
//   * pure formatting/mapping the user reads directly, and
//   * that every panel can be CONSTRUCTED when the helper is unavailable —
//     the common real-world state (helper not installed, not running, or
//     PolicyKit refusing), and the one most likely to crash on a null client.
//
// Runs under QT_QPA_PLATFORM=offscreen, so it needs no display and no session
// bus, which is what lets it run in CI.

#include <QTest>
#include <QSignalSpy>
#include <QAbstractItemModel>
#include <QJsonValue>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QFile>
#include <QCheckBox>
#include <QGroupBox>
#include <QLabel>
#include <QPushButton>
#include <QRadioButton>
#include <QTableView>
#include <QComboBox>
#include <QDateTimeEdit>

#include <functional>
#include <optional>

#include "../src/dbusclient.h"
#include "../src/recoverystatus.h"
#include "../src/recoverypanel.h"
#include "../src/filemodel.h"
#include "../src/healthdashboard.h"
#include "../src/backuphistory.h"
#include "../src/backuppanel.h"
#include "../src/backupsteps.h"
#include "../src/panelconfig.h"
#include "../src/progresspanel.h"

class GuiSmokeTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    // --- pure formatting ---------------------------------------------------

    void formatSize_data()
    {
        QTest::addColumn<qint64>("bytes");
        QTest::addColumn<QString>("expected");

        QTest::newRow("zero") << qint64(0) << QStringLiteral("0 B");
        // Boundaries are the part worth pinning: each is the last value before
        // the unit changes, where an off-by-one reads as a 1024x error.
        QTest::newRow("1023 B") << qint64(1023) << QStringLiteral("1023 B");
        QTest::newRow("1 KiB") << qint64(1024) << QStringLiteral("1.0 KiB");
        QTest::newRow("1 MiB") << qint64(1024LL * 1024) << QStringLiteral("1.0 MiB");
        QTest::newRow("1 GiB") << qint64(1024LL * 1024 * 1024) << QStringLiteral("1.0 GiB");
        // Above GiB the unit stops climbing, so a 4 TB drive reads in GiB.
        QTest::newRow("1 TiB stays GiB")
            << qint64(1024LL * 1024 * 1024 * 1024) << QStringLiteral("1024.0 GiB");
    }

    void formatSize()
    {
        QFETCH(qint64, bytes);
        QFETCH(QString, expected);
        QCOMPARE(FileModel::formatSize(bytes), expected);
    }

    // --- D-Bus error mapping ----------------------------------------------

    void mapsKnownDBusErrorsToActionableText()
    {
        // The message the user gets when the helper is missing must say what to
        // do about it, not echo D-Bus's raw wording.
        const QString unknown = DBusClient::mapDBusError(
            QStringLiteral("org.freedesktop.DBus.Error.ServiceUnknown"),
            QStringLiteral("The name is not activatable"));
        QVERIFY(unknown.contains(QStringLiteral("btrdasd-helper")));
        QVERIFY(!unknown.contains(QStringLiteral("not activatable")));

        QCOMPARE(DBusClient::mapDBusError(
                     QStringLiteral("org.freedesktop.DBus.Error.TimedOut"),
                     QStringLiteral("raw")),
                 QStringLiteral("D-Bus call timed out."));

        QVERIFY(DBusClient::mapDBusError(
                    QStringLiteral("org.freedesktop.PolicyKit1.Error.NotAuthorized"),
                    QStringLiteral("raw"))
                    .contains(QStringLiteral("PolicyKit")));
    }

    void passesUnknownDBusErrorsThroughUnchanged()
    {
        // Anything unrecognised must reach the user verbatim rather than being
        // flattened into a generic string that hides the cause.
        const QString raw = QStringLiteral("Connection reset by peer");
        QCOMPARE(DBusClient::mapDBusError(QStringLiteral("org.example.Whatever"), raw),
                 raw);
    }

    // --- construction with no helper --------------------------------------

    void clientReportsUnavailableWithoutAHelper()
    {
        DBusClient client;
        // No system-bus helper exists in the test environment, so this is the
        // unavailable path — and it must say why rather than failing silently.
        if (!client.isAvailable()) {
            QVERIFY(!client.unavailableReason().isEmpty());
        }
    }

    void panelsConstructWhenTheHelperIsUnavailable()
    {
        DBusClient client;

        // Each panel is built against a client with no reachable helper. This is
        // the ordinary state on a machine where btrdasd-helper is not installed,
        // and the one where a missing null-check surfaces as a crash on launch.
        HealthDashboard health(&client);
        QVERIFY(health.metaObject() != nullptr);

        BackupHistoryView history(&client);
        QVERIFY(history.metaObject() != nullptr);

        ProgressPanel progress(&client);
        QVERIFY(progress.metaObject() != nullptr);
    }

    // --- Cancel says "Cancelling…" until the job really ends (bd yq2) ------
    //
    // A cancel stops the job at its next safe boundary, which can be the end
    // of a long send: the button must not look as if nothing happened, nor
    // offer to cancel again, until JobFinished arrives.

    void cancelButtonSaysCancellingUntilTheJobFinishes()
    {
        DBusClient client;
        ProgressPanel panel(&client);
        auto *cancel = panel.findChild<QPushButton *>();
        QVERIFY(cancel != nullptr);

        panel.onJobStarted(QStringLiteral("job-1"), QStringLiteral("Backup"));
        QCOMPARE(cancel->text(), QStringLiteral("Cancel"));
        QVERIFY(cancel->isEnabled());

        // Not clicked: on a host with the helper running that would send a
        // real JobCancel. The helper accepting it is what cancelJob reacts to.
        panel.showCancelRequested();
        QCOMPARE(cancel->text(), QStringLiteral("Cancelling…"));
        QVERIFY(!cancel->isEnabled());

        // Another job's end changes nothing.
        panel.onJobFinished(QStringLiteral("job-2"), true, QStringLiteral("x"));
        QCOMPARE(cancel->text(), QStringLiteral("Cancelling…"));

        panel.onJobFinished(QStringLiteral("job-1"), false,
                            QStringLiteral("cancelled after mounting the sources"));
        QCOMPARE(cancel->text(), QStringLiteral("Cancel"));
        QVERIFY(!cancel->isEnabled());

        // The next job starts with a usable button.
        panel.onJobStarted(QStringLiteral("job-3"), QStringLiteral("Backup"));
        QCOMPARE(cancel->text(), QStringLiteral("Cancel"));
        QVERIFY(cancel->isEnabled());
    }

    // --- backup history: an unknown count is "unknown", never 0 -------------
    //
    // A run that could not count its snapshots is stored with NULL counts and
    // reaches the GUI as JSON null (bd DAS-Backup-Manager-6wt). QJsonValue's
    // toInteger() turns null into 0 — a count of nothing — so the view must
    // tell them apart.

    void countFromJsonTellsUnknownFromZero()
    {
        QCOMPARE(BackupHistoryView::countFromJson(QJsonValue(qint64(53))),
                 std::optional<qint64>(53));
        QCOMPARE(BackupHistoryView::countFromJson(QJsonValue(0)), std::optional<qint64>(0));
        QVERIFY(!BackupHistoryView::countFromJson(QJsonValue(QJsonValue::Null)).has_value());
        QVERIFY(!BackupHistoryView::countFromJson(QJsonValue(QJsonValue::Undefined)).has_value());
        QVERIFY(!BackupHistoryView::countFromJson(QJsonValue(-1)).has_value());
        QVERIFY(!BackupHistoryView::countFromJson(QJsonValue(2.5)).has_value());
        QVERIFY(!BackupHistoryView::countFromJson(QJsonValue(QStringLiteral("5"))).has_value());
    }

    void historyShowsAnUnknownCountAsUnknown()
    {
        DBusClient client;
        BackupHistoryView view(&client);
        // The helper's JSON, newest first: a failed run that could not count,
        // then two that did — 53 snapshots, and a measured 0.
        view.showHistory(QStringLiteral(R"([
            {"id": 290, "timestamp": 1791000000, "mode": "full", "success": false,
             "duration_secs": 516, "snaps_created": null, "snaps_sent": null,
             "bytes_sent": 0, "errors": ["btrbk: exit code 10"]},
            {"id": 289, "timestamp": 1790900000, "mode": "incremental", "success": true,
             "duration_secs": 300, "snaps_created": 53, "snaps_sent": 94,
             "bytes_sent": 4096, "errors": []},
            {"id": 288, "timestamp": 1790800000, "mode": "incremental", "success": true,
             "duration_secs": 300, "snaps_created": 0, "snaps_sent": 0,
             "bytes_sent": 0, "errors": []}
        ])"));

        const auto *table = view.findChild<QTableView *>();
        QVERIFY(table != nullptr);
        QAbstractItemModel *model = table->model();
        QCOMPARE(model->rowCount(), 3);
        int column = -1;
        for (int c = 0; c < model->columnCount(); ++c) {
            if (model->headerData(c, Qt::Horizontal).toString() == QStringLiteral("Snapshots"))
                column = c;
        }
        QVERIFY(column >= 0);
        const auto shown = [&] {
            QStringList cells;
            for (int r = 0; r < model->rowCount(); ++r)
                cells << model->index(r, column).data().toString();
            return cells;
        };

        // As the view opens: newest first.
        QCOMPARE(shown(), (QStringList{QStringLiteral("unknown"), QStringLiteral("53"),
                                       QStringLiteral("0")}));
        // Sorted on the column: by number, with "unknown" below every count.
        model->sort(column, Qt::AscendingOrder);
        QCOMPARE(shown(), (QStringList{QStringLiteral("unknown"), QStringLiteral("0"),
                                       QStringLiteral("53")}));
        model->sort(column, Qt::DescendingOrder);
        QCOMPARE(shown(), (QStringList{QStringLiteral("53"), QStringLiteral("0"),
                                       QStringLiteral("unknown")}));
    }

    void historyShowsASentCountItCouldNotTakeAsUnknownNotAsNo()
    {
        DBusClient client;
        BackupHistoryView view(&client);
        // Newest first: a failed run that could not count (NULL), a failed run
        // that counted nothing sent (a measured 0), a clean run that sent
        // bytes, and a clean run with nothing to send.
        view.showHistory(QStringLiteral(R"([
            {"id": 4, "timestamp": 1791000000, "mode": "full", "success": false,
             "duration_secs": 5, "snaps_created": null, "snaps_sent": null,
             "bytes_sent": 0, "errors": ["btrbk: exit code 10"]},
            {"id": 3, "timestamp": 1790900000, "mode": "full", "success": false,
             "duration_secs": 5, "snaps_created": 2, "snaps_sent": 0,
             "bytes_sent": 0, "errors": ["target not mounted"]},
            {"id": 2, "timestamp": 1790800000, "mode": "incremental", "success": true,
             "duration_secs": 5, "snaps_created": 3, "snaps_sent": 3,
             "bytes_sent": 4096, "errors": []},
            {"id": 1, "timestamp": 1790700000, "mode": "incremental", "success": true,
             "duration_secs": 5, "snaps_created": 0, "snaps_sent": 0,
             "bytes_sent": 0, "errors": []}
        ])"));
        const auto *table = view.findChild<QTableView *>();
        QVERIFY(table != nullptr);
        QAbstractItemModel *model = table->model();
        QCOMPARE(model->rowCount(), 4);
        int column = -1;
        for (int c = 0; c < model->columnCount(); ++c) {
            if (model->headerData(c, Qt::Horizontal).toString() == QStringLiteral("Sent"))
                column = c;
        }
        QVERIFY(column >= 0);
        QStringList cells;
        for (int r = 0; r < model->rowCount(); ++r)
            cells << model->index(r, column).data().toString();
        QCOMPARE(cells, (QStringList{QStringLiteral("unknown"), QStringLiteral("No"),
                                     QStringLiteral("Yes"), QStringLiteral("\u2014")}));
    }

    void backupStepsMapToTheHelpersKeys()
    {
        const BackupSteps all;
        const QVariantMap map = all.toDBus();
        QCOMPARE(map.keys(), (QStringList{QStringLiteral("boot_archive"), QStringLiteral("email"),
                                          QStringLiteral("index"), QStringLiteral("send"),
                                          QStringLiteral("snapshot")}));
        // One box at a time: each reaches its own key and no other.
        const QList<std::pair<bool BackupSteps::*, QString>> boxes{
            {&BackupSteps::snapshot, QStringLiteral("snapshot")},
            {&BackupSteps::send, QStringLiteral("send")},
            {&BackupSteps::bootArchive, QStringLiteral("boot_archive")},
            {&BackupSteps::index, QStringLiteral("index")},
            {&BackupSteps::email, QStringLiteral("email")},
        };
        for (const auto &[member, key] : boxes) {
            BackupSteps s;
            s.*member = false;
            const QVariantMap m = s.toDBus();
            for (auto it = m.cbegin(); it != m.cend(); ++it) {
                QCOMPARE(it.value().metaType().id(), QMetaType::Bool);
                QCOMPARE(it.value().toBool(), it.key() != key);
            }
        }
    }

    void nothingToRunWithoutSnapshotOrSend()
    {
        BackupSteps s;
        QVERIFY(s.runsBtrbk());
        s.snapshot = false;
        QVERIFY(s.runsBtrbk());
        s.send = false;
        QVERIFY(!s.runsBtrbk());
        s.snapshot = true;
        QVERIFY(s.runsBtrbk());
    }

    void fileModelIsEmptyWithoutAReachableHelper()
    {
        // An unreachable helper must leave an empty model, not throw or abort —
        // the GUI opens before anyone has run a backup, and on a machine where
        // btrdasd-helper is not installed at all.
        //
        // This used to pass a nonexistent database path. It cannot any more:
        // the helper resolves the index path itself from the canonical config
        // and no longer accepts one from the caller (bd DAS-Backup-Manager-gko).
        DBusClient client;
        FileModel model(&client);
        QCOMPARE(model.rowCount(QModelIndex()), 0);
    }

    // --- backup panel config reading (bd DAS-Backup-Manager-51j6) -----------
    //
    // The shape below is the live /etc/das-backup/config.toml layout (headers,
    // multi-line arrays, nested [[source.subvolumes]], other sections with their
    // own `enabled`), minus anything identifying.

private:
    static QString liveShapedConfig()
    {
        return QStringLiteral(
            "# Generated by btrdasd setup — do not edit.\n"
            "[general]\n"
            "version = \"1\"\n"
            "\n"
            "[boot]\n"
            "enabled = true\n"
            "subvolumes = [\n"
            "    \"@\",\n"
            "    \"@home\",\n"
            "]\n"
            "archive_retention_days = 60\n"
            "\n"
            "[scrub]\n"
            "enabled = true\n"
            "\n"
            "[[source]]\n"
            "label = \"nvme\"\n"
            "volume = \"/.btrfs-nvme\"\n"
            "target_subdirs = [\"nvme\"]\n"
            "target_labels = []\n"
            "\n"
            "[[source.subvolumes]]\n"
            "name = \"@\"\n"
            "label = \"not-a-source\"\n"
            "\n"
            "[[source]]\n"
            "label = \"ssd\"\n"
            "\n"
            "[[target]]\n"
            "label = \"primary-22tb\"\n"
            "serials = [\n"
            "    \"AAAA1111\",\n"
            "]\n"
            "role = \"primary\"\n"
            "\n"
            "[target.retention]\n"
            "daily = 7\n"
            "\n"
            "[[target]]\n"
            "label = \"system-recovery-B-2tb\"\n"
            "display_name = \"2TB Recovery B\"\n"
            "\n"
            "[email]\n"
            "enabled = false\n");
    }

private Q_SLOTS:
    void panelConfigReadsTheLiveFormat()
    {
        const PanelConfig c = parsePanelConfig(liveShapedConfig());
        QCOMPARE(c.sources, (QStringList{QStringLiteral("nvme"), QStringLiteral("ssd")}));
        QCOMPARE(c.targets,
                 (QStringList{QStringLiteral("primary-22tb"), QStringLiteral("system-recovery-B-2tb")}));
        // [email] enabled = false is not [boot]'s
        QVERIFY(c.bootEnabled);
    }

    void panelConfigBootDisabledWithTrailingComments()
    {
        QVERIFY(!parsePanelConfig(QStringLiteral("[boot] # note\nenabled = false # temporary\n")).bootEnabled);
        QVERIFY(!parsePanelConfig(QStringLiteral("[boot]\nenabled=false\n")).bootEnabled);
        QVERIFY(!parsePanelConfig(QStringLiteral("  [boot]  \n  enabled\t=\tfalse  \n")).bootEnabled);
        QVERIFY(parsePanelConfig(QStringLiteral("[boot] # note\nenabled = true # x\n")).bootEnabled);
    }

    void panelConfigEnabledFalseElsewhereDoesNotDisableBoot()
    {
        const PanelConfig c = parsePanelConfig(QStringLiteral(
            "[boot]\nenabled = true\n[email]\nenabled = false\n[scrub]\nenabled=false\n"));
        QVERIFY(c.bootEnabled);
        // ...and a [boot] that closes before its enabled line is not read either
        QVERIFY(parsePanelConfig(QStringLiteral("[boot]\n[email]\nenabled = false\n")).bootEnabled);
    }

    void panelConfigHashInsideQuotesIsNotAComment()
    {
        QCOMPARE(stripTomlComment(QStringLiteral("label = \"a#b\" # c")), QStringLiteral("label = \"a#b\""));
        QCOMPARE(stripTomlComment(QStringLiteral("label = 'a#b'# c")), QStringLiteral("label = 'a#b'"));
        QCOMPARE(stripTomlComment(QStringLiteral("x = \"q\\\"#\" # c")), QStringLiteral("x = \"q\\\"#\""));
        QCOMPARE(stripTomlComment(QStringLiteral("# only a comment")), QString());
        QCOMPARE(stripTomlComment(QStringLiteral("  plain  ")), QStringLiteral("plain"));
        const PanelConfig c = parsePanelConfig(QStringLiteral(
            "[[source]]\nlabel = \"a#b\" # trailing\n[[target]]\nlabel = \"t#1\"\n"));
        QCOMPARE(c.sources, QStringList{QStringLiteral("a#b")});
        QCOMPARE(c.targets, QStringList{QStringLiteral("t#1")});
    }

    void panelConfigLabelsAfterCommentedHeaders()
    {
        const PanelConfig c = parsePanelConfig(QStringLiteral(
            "[[source]] # first\nlabel = \"s1\" # one\n"
            "[[source]]# second\nlabel=\"s2\"\n"
            "[[target]]   # t\nlabel = \"t1\"   # primary\n"));
        QCOMPARE(c.sources, (QStringList{QStringLiteral("s1"), QStringLiteral("s2")}));
        QCOMPARE(c.targets, QStringList{QStringLiteral("t1")});
    }

    // --- backup panel logic, driven with injected config text ---------------

private:
    struct PanelParts {
        QCheckBox *snapshot = nullptr;
        QCheckBox *send = nullptr;
        QCheckBox *boot = nullptr;
        QPushButton *dryRun = nullptr;
        QPushButton *run = nullptr;
        QRadioButton *full = nullptr;
        QRadioButton *incremental = nullptr;
        bool ok() const
        {
            return snapshot && send && boot && dryRun && run && full && incremental;
        }
    };

    static PanelParts partsOf(BackupPanel &panel)
    {
        PanelParts p;
        p.snapshot = panel.findChild<QCheckBox *>(QStringLiteral("snapshotCheck"));
        p.send = panel.findChild<QCheckBox *>(QStringLiteral("sendCheck"));
        p.boot = panel.findChild<QCheckBox *>(QStringLiteral("bootArchiveCheck"));
        p.dryRun = panel.findChild<QPushButton *>(QStringLiteral("dryRunButton"));
        p.run = panel.findChild<QPushButton *>(QStringLiteral("runButton"));
        const auto radios = panel.findChildren<QRadioButton *>();
        for (QRadioButton *r : radios) {
            if (r->text() == QLatin1String("Full"))
                p.full = r;
            else if (r->text() == QLatin1String("Incremental"))
                p.incremental = r;
        }
        return p;
    }

private Q_SLOTS:
    void panelButtonsNeedSnapshotOrSend()
    {
        DBusClient client; // no helper: its own config comes back empty
        BackupPanel panel(&client);
        panel.applyConfig(liveShapedConfig());
        const PanelParts p = partsOf(panel);
        QVERIFY(p.ok());

        QVERIFY(p.dryRun->isEnabled());
        QVERIFY(p.run->isEnabled());
        p.snapshot->setChecked(false);
        QVERIFY(p.run->isEnabled()); // Send alone is enough
        p.send->setChecked(false);
        QVERIFY(!p.dryRun->isEnabled());
        QVERIFY(!p.run->isEnabled());
        p.send->setChecked(true);
        QVERIFY(p.dryRun->isEnabled());
        QVERIFY(p.run->isEnabled());
    }

    void panelButtonsNeedASourceAndATarget()
    {
        DBusClient client;
        BackupPanel panel(&client);
        const PanelParts p = partsOf(panel);
        QVERIFY(p.ok());
        // No helper and no injected text: nothing to tick, so nothing to run
        QVERIFY(!p.run->isEnabled());
        panel.applyConfig(liveShapedConfig());
        QVERIFY(p.run->isEnabled());
        const auto boxes = panel.findChildren<QCheckBox *>();
        for (QCheckBox *cb : boxes) {
            const QString label = cb->property("originalLabel").toString();
            if (label == QLatin1String("primary-22tb") || label == QLatin1String("system-recovery-B-2tb"))
                cb->setChecked(false);
        }
        QVERIFY(!p.run->isEnabled()); // targets all unticked
    }

    void panelBootTooltipFollowsTheMode()
    {
        DBusClient client;
        BackupPanel panel(&client);
        panel.applyConfig(liveShapedConfig());
        const PanelParts p = partsOf(panel);
        QVERIFY(p.ok());
        QVERIFY(p.incremental->isChecked());
        const QString incremental = p.boot->toolTip();
        p.full->setChecked(true);
        const QString full = p.boot->toolTip();
        QVERIFY(!incremental.isEmpty());
        QVERIFY(!full.isEmpty());
        QVERIFY(incremental != full);
        p.incremental->setChecked(true);
        QCOMPARE(p.boot->toolTip(), incremental);
    }

    void panelBootArchiveFollowsTheConfig()
    {
        DBusClient client;
        BackupPanel panel(&client);
        const PanelParts p = partsOf(panel);
        QVERIFY(p.ok());
        const QString on = QStringLiteral("[boot]\nenabled = true\n");
        const QString off = QStringLiteral("[boot] # note\nenabled = false # x\n");

        panel.applyConfig(on);
        QVERIFY(p.boot->isEnabled());
        QVERIFY(p.boot->isChecked());

        panel.applyConfig(off);
        QVERIFY(!p.boot->isEnabled());
        QVERIFY(!p.boot->isChecked());
        QVERIFY(p.boot->toolTip().contains(QLatin1String("enabled = false")));
        // A mode toggle while disabled keeps it disabled with the reason
        p.full->setChecked(true);
        QVERIFY(!p.boot->isEnabled());
        QVERIFY(!p.boot->isChecked());
        QVERIFY(p.boot->toolTip().contains(QLatin1String("enabled = false")));

        // Re-enabled on reload: usable again, and ticked (the default)
        panel.applyConfig(on);
        QVERIFY(p.boot->isEnabled());
        QVERIFY(p.boot->isChecked());
        QVERIFY(!p.boot->toolTip().contains(QLatin1String("enabled = false")));
    }

    void panelBootUntickByTheUserSurvivesModeToggles()
    {
        DBusClient client;
        BackupPanel panel(&client);
        panel.applyConfig(QStringLiteral("[boot]\nenabled = true\n"));
        const PanelParts p = partsOf(panel);
        QVERIFY(p.boot);
        QVERIFY(p.full);
        QVERIFY(p.incremental);
        p.boot->setChecked(false); // the user's own untick
        p.full->setChecked(true);
        p.incremental->setChecked(true);
        QVERIFY(!p.boot->isChecked());
    }

    // Rows the panel shows now (no event-loop turn: a deferred delete must not count)
    static int rowsShown(BackupPanel &panel, int &boxes, int &labels)
    {
        boxes = 0;
        labels = 0;
        const auto groups = panel.findChildren<QGroupBox *>();
        for (QGroupBox *g : groups) {
            if (g->title() != QLatin1String("Sources") && g->title() != QLatin1String("Targets"))
                continue;
            boxes += g->findChildren<QCheckBox *>().size();
            labels += g->findChildren<QLabel *>().size();
        }
        return boxes + labels;
    }

    void panelReloadDoesNotDuplicateRows()
    {
        DBusClient client;
        BackupPanel panel(&client);
        int boxes = 0, labels = 0;
        // No helper: loadConfig() takes the placeholder-label path
        rowsShown(panel, boxes, labels);
        QCOMPARE(labels, 2);
        QMetaObject::invokeMethod(&panel, "loadConfig", Qt::DirectConnection);
        rowsShown(panel, boxes, labels);
        QCOMPARE(labels, 2);

        panel.applyConfig(liveShapedConfig());
        rowsShown(panel, boxes, labels);
        const int firstBoxes = boxes;
        QVERIFY(firstBoxes > 0);
        QCOMPARE(labels, 0);
        panel.applyConfig(liveShapedConfig());
        rowsShown(panel, boxes, labels);
        QCOMPARE(boxes, firstBoxes);
        QCOMPARE(labels, 0);
        QMetaObject::invokeMethod(&panel, "loadConfig", Qt::DirectConnection);
        rowsShown(panel, boxes, labels);
        QCOMPARE(boxes, 0);
        QCOMPARE(labels, 2);
    }

    void panelButtonsFollowTheBackupJob()
    {
        DBusClient client;
        BackupPanel panel(&client);
        panel.applyConfig(liveShapedConfig());
        const PanelParts p = partsOf(panel);
        QVERIFY(p.ok());
        QVERIFY(p.run->isEnabled());

        p.run->click(); // job requested
        QVERIFY(!p.run->isEnabled());
        QVERIFY(!p.dryRun->isEnabled());
        Q_EMIT client.jobStarted(QStringLiteral("job-1"), QStringLiteral("BackupRun"));
        QVERIFY(!p.run->isEnabled());

        // Selection toggles meanwhile
        const auto boxes = panel.findChildren<QCheckBox *>();
        for (QCheckBox *cb : boxes) {
            if (!cb->property("originalLabel").isValid())
                continue;
            cb->setChecked(false);
            cb->setChecked(true);
        }
        p.send->setChecked(false);
        p.send->setChecked(true);
        QVERIFY(!p.run->isEnabled());
        QVERIFY(!p.dryRun->isEnabled());

        // A config reload meanwhile
        panel.applyConfig(liveShapedConfig());
        QVERIFY(!p.run->isEnabled());

        // Another operation failing, or another job finishing, is not this job ending
        Q_EMIT client.errorOccurred(QStringLiteral("ConfigGet"), QStringLiteral("x"));
        QVERIFY(!p.run->isEnabled());
        Q_EMIT client.jobFinished(QStringLiteral("job-other"), true, QString());
        QVERIFY(!p.run->isEnabled());

        // This job finishing is
        Q_EMIT client.jobFinished(QStringLiteral("job-1"), true, QString());
        QVERIFY(p.run->isEnabled());
        QVERIFY(p.dryRun->isEnabled());
    }

    void panelButtonsComeBackWhenTheJobFinishesBeforeItsReply()
    {
        DBusClient client;
        BackupPanel panel(&client);
        panel.applyConfig(liveShapedConfig());
        const PanelParts p = partsOf(panel);
        QVERIFY(p.ok());
        p.run->click();
        QVERIFY(!p.run->isEnabled());
        // A refusal ends at once: the signal beats the method reply
        Q_EMIT client.jobFinished(QStringLiteral("job-9"), false, QStringLiteral("refused"));
        Q_EMIT client.jobStarted(QStringLiteral("job-9"), QStringLiteral("BackupRun"));
        QVERIFY(p.run->isEnabled());
        QVERIFY(p.dryRun->isEnabled());
        // Counter-case: a finish for a different id does not end the next job
        p.run->click();
        Q_EMIT client.jobFinished(QStringLiteral("job-other"), true, QString());
        Q_EMIT client.jobStarted(QStringLiteral("job-10"), QStringLiteral("BackupRun"));
        QVERIFY(!p.run->isEnabled());
    }

    void panelButtonsComeBackWhenTheCallItselfFails()
    {
        DBusClient client;
        BackupPanel panel(&client);
        panel.applyConfig(liveShapedConfig());
        const PanelParts p = partsOf(panel);
        QVERIFY(p.ok());
        p.run->click();
        QVERIFY(!p.run->isEnabled());
        Q_EMIT client.errorOccurred(QStringLiteral("BackupRun"), QStringLiteral("denied"));
        QVERIFY(p.run->isEnabled());
    }

    // TigerVNC's viewer takes a UNIX socket path as its server; remote-viewer's
    // unix-path is SPICE-only and gvncviewer takes host:display (found on the
    // host, 2026-10-09: the first form failed with 'host != NULL').
    void consoleCommandIsTigerVncOnTheUnixSocket()
    {
        const auto [program, args] = DBusClient::consoleCommand(QStringLiteral("/run/das-recovery-os-vm/x/vnc.sock"));
        QCOMPARE(program, QStringLiteral("vncviewer"));
        QCOMPARE(args, QStringList{QStringLiteral("/run/das-recovery-os-vm/x/vnc.sock")});
    }

    void sessionEndUsesATenMinuteTimeout()
    {
        DBusClient client;
        QCOMPARE(client.sessionEndTimeoutMs(), 600000);
    }

    void recoveryCallsOnAnUnavailableHelperAnswerEmptyNotSilent()
    {
        DBusClient client;
        if (client.isAvailable())
            QSKIP("a helper is reachable; this pins the unavailable path");
        QSignalSpy status(&client, &DBusClient::recoveryOsStatusResult);
        client.recoveryOsStatusAsync();
        QCOMPARE(status.count(), 1);
        QVERIFY(status.at(0).at(0).toString().isEmpty());
        QSignalSpy end(&client, &DBusClient::recoveryOsSessionEndResult);
        client.recoveryOsSessionEnd(QStringLiteral("system-recovery-A-2tb"));
        QCOMPARE(end.count(), 1);
        QVERIFY(!end.at(0).at(1).toBool());
    }

    // --- Recovery drives: the status document (8249 stage 3) ----------------
    //
    // gui/tests/fixtures/recovery-status.json is written by a panel.rs test
    // from a fixed scripted scenario and must equal status_json's output
    // byte for byte; this side parses the same file. Drive A: a record
    // (verdict may), a running helper job, a missed schedule, 2 clean runs.
    // Drive B: no record, every part unreadable.

    static QByteArray fixture()
    {
        QFile f(QStringLiteral(RECOVERY_FIXTURE_PATH));
        if (!f.open(QIODevice::ReadOnly))
            return {};
        return f.readAll();
    }

    void parseReadsEveryPartOfTheFixture()
    {
        QString err;
        const auto doc = RecoveryDocument::parse(fixture(), &err);
        QVERIFY2(doc.has_value(), qPrintable(err));
        QCOMPARE(doc->schema, 1);
        QCOMPARE(doc->maxAgeDays, 60);
        QCOMPARE(doc->today, QStringLiteral("2026-10-09"));
        QCOMPARE(doc->pair.modeDefault, QStringLiteral("sequential"));
        QVERIFY(!doc->pair.schedule.has_value());
        QVERIFY(!doc->pair.scheduleError.has_value());
        QCOMPARE(doc->drives.size(), 2);

        const DriveView &a = doc->drives[0];
        QCOMPARE(a.label, QStringLiteral("system-recovery-A-2tb"));
        QCOMPARE(a.displayName, QStringLiteral("Drive system-recovery-A-2tb"));
        QCOMPARE(a.serials, QStringList{QStringLiteral("SER-system-recovery-A-2tb")});
        QCOMPARE(a.checkedEpoch, std::optional<qint64>(1791448102));
        QVERIFY(!a.recordError.has_value());
        QVERIFY(a.record.has_value());
        QCOMPARE(a.record->kernel, QStringLiteral("6.17.1-1-cachyos"));
        QCOMPARE(a.record->hostKernel, QStringLiteral("6.17.2-1-cachyos"));
        QCOMPARE(a.record->lastFullUpgrade, QStringLiteral("2026-09-20"));
        QVERIFY(!a.record->packagesRead); // the scenario's package database was not read
        QVERIFY(a.record->btrbk.isEmpty()); // null, not "not installed"
        QCOMPARE(a.record->guestAgent.state, QStringLiteral("read"));
        QCOMPARE(a.record->guestAgent.installed, std::optional<bool>(true));
        QCOMPARE(a.record->guestAgent.enabled, std::optional<bool>(true));
        QVERIFY(a.assessment.has_value());
        QCOMPARE(a.assessment->ageDays, std::optional<qint64>(19));
        QVERIFY(!a.assessment->stale);
        QCOMPARE(a.assessment->warnings.size(), 1);
        QVERIFY(!a.due);
        QCOMPARE(a.verdict, QStringLiteral("may"));
        QVERIFY(a.unattendedPossible);
        QVERIFY(a.unattendedWhy.isEmpty());
        QCOMPARE(a.cleanRuns, std::optional<qint64>(2));
        QVERIFY(!a.cleanRunsError.has_value());
        QCOMPARE(a.history.size(), 2);
        QCOMPARE(a.history[1][QStringLiteral("outcome")].toString(), QStringLiteral("clean"));
        QVERIFY(!a.historyError.has_value());
        QVERIFY(a.schedule.has_value());
        QCOMPARE(a.schedule->state, QStringLiteral("missed"));
        QCOMPARE(a.schedule->unit, QStringLiteral("das-recovery-os-update-system-recovery-A-2tb.timer"));
        QVERIFY(!a.schedule->atEpoch.has_value());
        QVERIFY(a.schedule->mode.isEmpty());
        QVERIFY(!a.schedule->detail.isEmpty());
        QVERIFY(!a.scheduleError.has_value());
        QVERIFY(a.session.has_value());
        QCOMPARE(a.session->by, QStringLiteral("job:job-7"));
        QCOMPARE(a.session->domainState, QStringLiteral("running"));
        QVERIFY(!a.session->sinceEpoch.has_value());
        QVERIFY(!a.session->attended.has_value()); // null in the document
        QVERIFY(!a.sessionError.has_value());

        const DriveView &b = doc->drives[1];
        QCOMPARE(b.label, QStringLiteral("system-recovery-B-2tb"));
        QVERIFY(b.recordError.has_value());
        QVERIFY(b.recordError->contains(QStringLiteral("no record")));
        QVERIFY(!b.record.has_value());
        QVERIFY(!b.assessment.has_value());
        QVERIFY(b.verdict.isEmpty());
        QVERIFY(!b.unattendedPossible);
        QVERIFY(b.unattendedWhy.contains(QStringLiteral("no record")));
        QVERIFY(!b.cleanRuns.has_value());
        QVERIFY(b.cleanRunsError.has_value());
        QVERIFY(b.history.isEmpty());
        QVERIFY(b.historyError.has_value());
        QVERIFY(!b.schedule.has_value());
        QVERIFY(b.scheduleError.has_value());
        QVERIFY(b.scheduleError->contains(QStringLiteral("cannot be read")));
        QVERIFY(!b.session.has_value());
        QVERIFY(b.sessionError.has_value());
    }

    void parseRefusesAnotherSchemaAndAMissingKey()
    {
        QString err;
        QVERIFY(!RecoveryDocument::parse("not json", &err).has_value());
        QVERIFY2(err.contains(QStringLiteral("not JSON")), qPrintable(err));

        const QJsonDocument d = QJsonDocument::fromJson(fixture());
        QJsonObject o = d.object();
        o[QStringLiteral("schema")] = 2;
        QVERIFY(!RecoveryDocument::parse(QJsonDocument(o).toJson(), &err).has_value());
        QVERIFY2(err.contains(QStringLiteral("schema 2")), qPrintable(err));

        o = d.object();
        QJsonArray drives = o[QStringLiteral("drives")].toArray();
        QJsonObject drive = drives[1].toObject();
        drive.remove(QStringLiteral("unattended"));
        drives[1] = drive;
        o[QStringLiteral("drives")] = drives;
        QVERIFY(!RecoveryDocument::parse(QJsonDocument(o).toJson(), &err).has_value());
        QVERIFY2(err.contains(QStringLiteral("drives[1]")) && err.contains(QStringLiteral("unattended")), qPrintable(err));

        o = d.object();
        o.remove(QStringLiteral("pair"));
        QVERIFY(!RecoveryDocument::parse(QJsonDocument(o).toJson(), &err).has_value());
        QVERIFY2(err.contains(QStringLiteral("pair")), qPrintable(err));
    }

    // --- Recovery drives: the enablement rules (spec §4), both ways -------

private:
    struct Scenario {
        RecoveryDocument doc;
        GuiFacts facts;
        DriveView &a() { return doc.drives[0]; }
        DriveView &b() { return doc.drives[1]; }
    };

    // Drive A of the fixture with its running job and its schedule removed:
    // a drive on which everything is allowed. nowEpoch is the fixture's clock.
    static Scenario idleA()
    {
        QString err;
        Scenario s{*RecoveryDocument::parse(fixture(), &err), {}};
        s.a().session.reset();
        s.a().schedule.reset();
        s.facts.nowEpoch = 1791500000;
        s.facts.chosenNow = true;
        s.facts.viewerInstalled = true;
        return s;
    }

private Q_SLOTS:
    void upgradeAttendedNeedsARecordAndNoSession()
    {
        Scenario s = idleA();
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY2(a.upgrade.enabled, qPrintable(a.upgrade.why));
        QVERIFY(!a.bannerNeeded);
        QVERIFY(a.upgrade.why.contains(QStringLiteral("attended")));

        Scenario r = idleA();
        r.a().record.reset();
        r.a().recordError = QStringLiteral("the record of system-recovery-A-2tb holds no reading");
        a = deriveActions(r.a(), r.doc.pair, r.facts);
        QVERIFY(!a.upgrade.enabled);
        QCOMPARE(a.upgrade.why, *r.a().recordError);

        Scenario h = idleA();
        h.a().session = SessionView{QStringLiteral("other:recovery-os VM session system-recovery-A-2tb pid 1"), 1791400000, QStringLiteral("running"), std::nullopt};
        a = deriveActions(h.a(), h.doc.pair, h.facts);
        QVERIFY(!a.upgrade.enabled);
        QVERIFY(a.upgrade.why.contains(QStringLiteral("a session holds")));

        Scenario pr = idleA();
        pr.doc.pair.session = SessionView{QStringLiteral("unit:das-recovery-os-update-both.service"), 1791400000, QString(), false};
        a = deriveActions(pr.a(), pr.doc.pair, pr.facts);
        QVERIFY(!a.upgrade.enabled);
        QVERIFY(a.upgrade.why.contains(QStringLiteral("both drives")));

        Scenario j = idleA();
        j.facts.ownJobId = QStringLiteral("job-9");
        a = deriveActions(j.a(), j.doc.pair, j.facts);
        QVERIFY(!a.upgrade.enabled);
        QVERIFY(a.upgrade.why.contains(QStringLiteral("this window")));
    }

    void upgradeUnattendedFollowsTheDocumentsPossible()
    {
        Scenario s = idleA();
        s.facts.unattended = true;
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY2(a.upgrade.enabled, qPrintable(a.upgrade.why));
        QVERIFY(a.upgrade.why.contains(QStringLiteral("unattended")));

        s.a().unattendedPossible = false;
        s.a().unattendedWhy = QStringLiteral("the boot record says btrbk will run");
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.upgrade.enabled);
        QCOMPARE(a.upgrade.why, s.a().unattendedWhy);
    }

    void theBannerIsNeededForAttendedOnAWillRecordOnly()
    {
        Scenario s = idleA();
        s.a().verdict = QStringLiteral("will");
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(a.upgrade.enabled);
        QVERIFY(a.bannerNeeded);
        s.facts.unattended = true;
        s.a().unattendedPossible = false;
        s.a().unattendedWhy = QStringLiteral("the boot record says btrbk will run");
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.upgrade.enabled);
        QVERIFY(!a.bannerNeeded);
        s.facts.unattended = false;
        s.a().verdict = QStringLiteral("may");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).bannerNeeded);
    }

    void scheduleNeedsATimeTwoMinutesAhead()
    {
        Scenario s = idleA();
        s.facts.unattended = true;
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.schedule.enabled); // Now is ticked
        QVERIFY(a.schedule.why.contains(QStringLiteral("untick Now")));

        s.facts.chosenNow = false;
        s.facts.chosenEpoch = s.facts.nowEpoch + ScheduleMinLeadSeconds - 1;
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.schedule.enabled);
        QVERIFY(a.schedule.why.contains(QStringLiteral("2 minutes")));

        s.facts.chosenEpoch = s.facts.nowEpoch - 3600;
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).schedule.enabled);

        s.facts.chosenEpoch = s.facts.nowEpoch + ScheduleMinLeadSeconds;
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY2(a.schedule.enabled, qPrintable(a.schedule.why));

        // A running session does not block scheduling; the attended radio does not either
        s.a().session = SessionView{QStringLiteral("other:x"), std::nullopt, QString(), std::nullopt};
        s.facts.unattended = false;
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).schedule.enabled);

        s.a().unattendedPossible = false;
        s.a().unattendedWhy = QStringLiteral("no guest agent");
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.schedule.enabled);
        QCOMPARE(a.schedule.why, s.a().unattendedWhy);
    }

    void clearScheduleNeedsAPendingOrMissedOne()
    {
        Scenario s = idleA();
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
        s.a().schedule = ScheduleView{QStringLiteral("u.timer"), 1791600000, QString(), QStringLiteral("pending"), QString()};
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
        s.a().schedule->state = QStringLiteral("missed");
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
        s.a().schedule->state = QStringLiteral("fired");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
        s.a().schedule->state = QStringLiteral("running");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
    }

    void consoleNeedsAnAttendedSessionThisWindowKnows()
    {
        Scenario s = idleA();
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);

        s.a().session = SessionView{QStringLiteral("other:x"), std::nullopt, QStringLiteral("running"), true};
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);
        s.a().session->domainState = QStringLiteral("shut off");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);

        // A job session: attended is null; only this window's attended job qualifies
        s.a().session = SessionView{QStringLiteral("job:job-7"), std::nullopt, QStringLiteral("running"), std::nullopt};
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.console.enabled);
        QVERIFY(a.console.why.contains(QStringLiteral("cannot tell")));
        s.facts.ownJobId = QStringLiteral("job-7");
        s.facts.ownJobAttended = false;
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);
        s.facts.ownJobAttended = true;
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);

        // The viewer missing keeps the button enabled (the click shows the path)
        s.facts.viewerInstalled = false;
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(a.console.enabled);
        QVERIFY(a.console.why.contains(QStringLiteral("tigervnc")));
    }

    void endSessionIsForAHolderThatIsNeitherAJobNorAUnit()
    {
        Scenario s = idleA();
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).endSession.enabled);
        s.a().session = SessionView{QStringLiteral("other:recovery-os VM session system-recovery-A-2tb pid 1"), std::nullopt, QString(), std::nullopt};
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).endSession.enabled);
        s.a().session->by = QStringLiteral("job:job-7");
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.endSession.enabled);
        QVERIFY(a.endSession.why.contains(QStringLiteral("Cancel")));
        s.a().session->by = QStringLiteral("unit:das-recovery-os-update-system-recovery-A-2tb.service");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).endSession.enabled);
    }

    void anErrorInAPartDisablesWhatDependsOnIt()
    {
        Scenario s = idleA();
        s.a().sessionError = QStringLiteral("das-recovery-os-update-both.service cannot be read: boom");
        s.a().session = SessionView{QStringLiteral("other:x"), std::nullopt, QStringLiteral("running"), true};
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.upgrade.enabled);
        QCOMPARE(a.upgrade.why, *s.a().sessionError);
        QVERIFY(!a.console.enabled);
        QCOMPARE(a.console.why, *s.a().sessionError);
        QVERIFY(!a.endSession.enabled);
        QCOMPARE(a.endSession.why, *s.a().sessionError);

        Scenario t = idleA();
        t.a().scheduleError = QStringLiteral("x.timer cannot be read: boom");
        t.a().schedule = ScheduleView{QStringLiteral("x.timer"), std::nullopt, QString(), QStringLiteral("pending"), QString()};
        t.facts.unattended = true;
        t.facts.chosenNow = false;
        t.facts.chosenEpoch = t.facts.nowEpoch + 600;
        a = deriveActions(t.a(), t.doc.pair, t.facts);
        QVERIFY(!a.schedule.enabled);
        QCOMPARE(a.schedule.why, *t.a().scheduleError);
        QVERIFY(!a.clearSchedule.enabled);

        // The pair's session error blocks the drive's upgrade too
        Scenario u = idleA();
        u.doc.pair.sessionError = QStringLiteral("pair boom");
        QVERIFY(!deriveActions(u.a(), u.doc.pair, u.facts).upgrade.enabled);
    }

    void pairActionsNeedBothDrivesAndNoPairError()
    {
        Scenario s = idleA();
        s.facts.unattended = true;
        // Drive B of the fixture is all errors: the pair is refused with B's reason
        PairActions p = derivePairActions(s.doc, s.facts);
        QVERIFY(!p.upgrade.enabled);
        QVERIFY(p.upgrade.why.contains(QStringLiteral("system-recovery-B-2tb")));
        QCOMPARE(p.modeDefault, QStringLiteral("sequential"));

        // Make B a copy of A: allowed
        s.b() = s.a();
        s.b().label = QStringLiteral("system-recovery-B-2tb");
        p = derivePairActions(s.doc, s.facts);
        QVERIFY2(p.upgrade.enabled, qPrintable(p.upgrade.why));
        s.facts.chosenNow = false;
        s.facts.chosenEpoch = s.facts.nowEpoch + 600;
        QVERIFY(derivePairActions(s.doc, s.facts).schedule.enabled);

        s.doc.pair.scheduleError = QStringLiteral("both boom");
        p = derivePairActions(s.doc, s.facts);
        QVERIFY(!p.schedule.enabled);
        QCOMPARE(p.schedule.why, *s.doc.pair.scheduleError);

        // One drive only: no pair
        s.doc.drives.removeLast();
        QVERIFY(!derivePairActions(s.doc, s.facts).upgrade.enabled);
    }

    void wordsForTheCards()
    {
        QCOMPARE(ageWords(std::nullopt, 100), QStringLiteral("unknown"));
        QCOMPARE(ageWords(1791500000 - 3 * 86400, 1791500000), QStringLiteral("3 days ago"));
        QCOMPARE(ageWords(1791500000 - 3600, 1791500000), QStringLiteral("today"));
        QCOMPARE(ageWords(1791500000 - 86400, 1791500000), QStringLiteral("1 day ago"));
        QCOMPARE(verdictWords(QStringLiteral("will")), QStringLiteral("will run btrbk at boot"));
        QCOMPARE(verdictWords(QString()), QStringLiteral("unknown"));
        QCOMPARE(sessionWords(SessionView{QStringLiteral("job:job-7"), std::nullopt, QStringLiteral("running"), std::nullopt}, 0),
                 QStringLiteral("running as helper job job-7 (VM running)"));
        QCOMPARE(sessionWords(SessionView{QStringLiteral("unit:x.service"), 1791500000 - 600, QString(), false}, 1791500000),
                 QStringLiteral("running as scheduled unit x.service since 10 minutes ago, unattended (VM state unknown)"));
        QCOMPARE(scheduleWords(ScheduleView{QStringLiteral("u"), std::nullopt, QString(), QStringLiteral("missed"), QStringLiteral("The time passed.")}),
                 QStringLiteral("missed: The time passed."));
    }

    // --- Recovery drives: the panel binds the rules (spec §3.2, §5) ---------

private:
    struct RecoveryParts {
        QPushButton *upgrade, *schedule, *clear, *console, *end;
        QComboBox *selector;
        QRadioButton *attended, *unattended;
        QCheckBox *now;
        QDateTimeEdit *when;
        bool ok() const
        {
            return upgrade && schedule && clear && console && end && selector && attended && unattended && now && when;
        }
    };

    static RecoveryParts recoveryPartsOf(QWidget &w)
    {
        return {w.findChild<QPushButton *>(QStringLiteral("upgrade")),
                w.findChild<QPushButton *>(QStringLiteral("schedule")),
                w.findChild<QPushButton *>(QStringLiteral("clearSchedule")),
                w.findChild<QPushButton *>(QStringLiteral("console")),
                w.findChild<QPushButton *>(QStringLiteral("endSession")),
                w.findChild<QComboBox *>(QStringLiteral("selector")),
                w.findChild<QRadioButton *>(QStringLiteral("attended")),
                w.findChild<QRadioButton *>(QStringLiteral("unattended")),
                w.findChild<QCheckBox *>(QStringLiteral("now")),
                w.findChild<QDateTimeEdit *>(QStringLiteral("when"))};
    }

    // The fixture with drive A idle (no session) and drive B a copy of A under
    // its own label, each then edited by the caller.
    static QByteArray twoIdleDrives(const std::function<void(QJsonObject &a, QJsonObject &b)> &edit)
    {
        const QJsonDocument d = QJsonDocument::fromJson(fixture());
        QJsonObject o = d.object();
        QJsonArray drives = o[QStringLiteral("drives")].toArray();
        QJsonObject a = drives[0].toObject();
        a[QStringLiteral("session")] = QJsonValue::Null;
        QJsonObject b = a;
        b[QStringLiteral("label")] = QStringLiteral("system-recovery-B-2tb");
        b[QStringLiteral("display_name")] = QStringLiteral("Drive system-recovery-B-2tb");
        edit(a, b);
        drives[0] = a;
        drives[1] = b;
        o[QStringLiteral("drives")] = drives;
        return QJsonDocument(o).toJson();
    }

    // Two idle, readable drives (the fixture's drive B has an unreadable
    // session, which rightly blocks a session on A too), then Upgrade
    // clicked: the panel now awaits jobStarted for a session of its own.
    static void requestSession(RecoveryPanel &panel, const RecoveryParts &p)
    {
        panel.applyDocument(twoIdleDrives([](QJsonObject &, QJsonObject &) {}));
        p.selector->setCurrentIndex(0);
        p.attended->setChecked(true);
        QVERIFY(p.upgrade->isEnabled());
        p.upgrade->click(); // verdict "may": no banner dialog
        QVERIFY(!p.upgrade->isEnabled());
    }

private Q_SLOTS:
    void recoveryPanelConstructsWithoutAHelperAndDisablesEverything()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        for (QPushButton *b : {p.upgrade, p.schedule, p.clear, p.console, p.end}) {
            QVERIFY(!b->isEnabled());
            QVERIFY(!b->toolTip().isEmpty());
        }
    }

    void recoveryPanelBindsTheDocument()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        panel.applyDocument(fixture());
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        QCOMPARE(p.selector->count(), 3); // A, B, both
        QCOMPARE(p.selector->itemText(2), QStringLiteral("Both drives"));
        QVERIFY(p.attended->isChecked());
        QVERIFY(p.now->isChecked());
        QVERIFY(!p.when->isEnabled());

        // Drive A has a running job and a missed schedule in the fixture
        p.selector->setCurrentIndex(0);
        QVERIFY(!p.upgrade->isEnabled());
        QVERIFY(p.upgrade->toolTip().contains(QStringLiteral("a session holds")));
        QVERIFY(p.clear->isEnabled());
        QVERIFY(!p.end->isEnabled());
        QVERIFY(p.end->toolTip().contains(QStringLiteral("Cancel")));

        // Drive B is all errors
        p.selector->setCurrentIndex(1);
        QVERIFY(!p.upgrade->isEnabled());
        QVERIFY(p.upgrade->toolTip().contains(QStringLiteral("no record")));
        QVERIFY(!p.clear->isEnabled());

        // The cards show every error in its place
        bool sawError = false;
        const auto labels = panel.findChildren<QLabel *>();
        for (QLabel *l : labels)
            if (l->text().contains(QStringLiteral("cannot be read")))
                sawError = true;
        QVERIFY(sawError);
    }

    void panelShowsTheParseErrorAndDisablesEverything()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        panel.applyDocument(fixture());
        panel.applyDocument("{\"schema\": 2}");
        QVERIFY2(panel.statusLine().contains(QStringLiteral("schema 2")), qPrintable(panel.statusLine()));
        const RecoveryParts p = recoveryPartsOf(panel);
        for (QPushButton *b : {p.upgrade, p.schedule, p.clear, p.console, p.end})
            QVERIFY(!b->isEnabled());
        // A good document afterwards recovers it
        panel.applyDocument(fixture());
        QVERIFY(!panel.statusLine().contains(QStringLiteral("schema 2")));
        p.selector->setCurrentIndex(0);
        QVERIFY(p.clear->isEnabled());
    }

    void aWarnedSessionNeedsALookAFailedOneFailed()
    {
        DBusClient client; // unavailable: a click reaches no helper, but the request is remembered
        RecoveryPanel panel(&client);
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());

        requestSession(panel, p);
        QVERIFY(!p.upgrade->isEnabled());
        panel.onJobStarted(QStringLiteral("job-1"), QStringLiteral("Recovery OS session"));
        panel.onJobFinished(QStringLiteral("job-1"), false, QStringLiteral("warnings: the guard was not confirmed on a no record"));
        QVERIFY2(panel.statusLine().startsWith(QStringLiteral("Needs a look")), qPrintable(panel.statusLine()));

        requestSession(panel, p);
        QVERIFY(!p.upgrade->isEnabled());
        panel.onJobStarted(QStringLiteral("job-2"), QStringLiteral("Recovery OS session"));
        panel.onJobFinished(QStringLiteral("job-2"), false, QStringLiteral("exit 7 at upgrade"));
        QVERIFY2(panel.statusLine().startsWith(QStringLiteral("Failed")), qPrintable(panel.statusLine()));

        requestSession(panel, p);
        QVERIFY(!p.upgrade->isEnabled());
        panel.onJobStarted(QStringLiteral("job-3"), QStringLiteral("Recovery OS session"));
        panel.onJobFinished(QStringLiteral("job-3"), true, QStringLiteral("clean"));
        QVERIFY2(panel.statusLine().startsWith(QStringLiteral("Done")), qPrintable(panel.statusLine()));

        // Another operation's job, and a session job nothing here requested,
        // are not this panel's
        panel.onJobStarted(QStringLiteral("job-4"), QStringLiteral("BackupRun"));
        QVERIFY(panel.facts().ownJobId.isEmpty());
        panel.onJobStarted(QStringLiteral("job-6"), QStringLiteral("Recovery OS session"));
        QVERIFY(panel.facts().ownJobId.isEmpty());
    }

    void thePanelKnowsItsOwnJobAndItsAttendedness()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        requestSession(panel, p);
        QVERIFY(!p.upgrade->isEnabled()); // requested, attended
        panel.onJobStarted(QStringLiteral("job-5"), QStringLiteral("Recovery OS session"));
        QCOMPARE(panel.facts().ownJobId, QStringLiteral("job-5"));
        QVERIFY(panel.facts().ownJobAttended);
        panel.onJobFinished(QStringLiteral("job-5"), true, QStringLiteral("clean"));
        QVERIFY(panel.facts().ownJobId.isEmpty());

        // A JobFinished that beats the method reply (a helper refusal) is
        // honoured when jobStarted names the job
        requestSession(panel, p);
        panel.onJobFinished(QStringLiteral("job-8"), false, QStringLiteral("refused: a session job is running"));
        panel.onJobStarted(QStringLiteral("job-8"), QStringLiteral("Recovery OS session"));
        QVERIFY(panel.facts().ownJobId.isEmpty());
        QVERIFY(p.upgrade->isEnabled());
    }

    // --- Final review fixes (8249 stage 3) ----------------------------------

private:
private Q_SLOTS:
    void upgradeIsBlockedWhileAnyDrivesSessionRuns()
    {
        Scenario s = idleA();
        s.b() = s.a();
        s.b().label = QStringLiteral("system-recovery-B-2tb");
        s.b().displayName = QStringLiteral("Drive system-recovery-B-2tb");
        QVERIFY(deriveActions(s.b(), s.doc, s.facts).upgrade.enabled);

        s.a().session = SessionView{QStringLiteral("job:job-7"), std::nullopt, QStringLiteral("running"), std::nullopt};
        DriveActions b = deriveActions(s.b(), s.doc, s.facts);
        QVERIFY(!b.upgrade.enabled);
        QVERIFY2(b.upgrade.why.contains(QStringLiteral("system-recovery-A-2tb")), qPrintable(b.upgrade.why));

        s.a().session.reset();
        s.a().sessionError = QStringLiteral("x.service cannot be read: boom");
        b = deriveActions(s.b(), s.doc, s.facts);
        QVERIFY(!b.upgrade.enabled);
        QCOMPARE(b.upgrade.why, *s.a().sessionError);

        // Scheduling B is not blocked by A's session
        s.facts.unattended = true;
        s.facts.chosenNow = false;
        s.facts.chosenEpoch = s.facts.nowEpoch + 600;
        QVERIFY(deriveActions(s.b(), s.doc, s.facts).schedule.enabled);
    }

    void thePanelRefreshesOnAnyJobSignal()
    {
        DBusClient client; // unavailable: recoveryOsStatusAsync answers at once with an empty document
        RecoveryPanel panel(&client);
        QSignalSpy spy(&client, &DBusClient::recoveryOsStatusResult);
        Q_EMIT client.jobStarted(QStringLiteral("x"), QStringLiteral("BackupRun"));
        QCOMPARE(spy.count(), 1);
        Q_EMIT client.jobFinished(QStringLiteral("x"), true, QString());
        QCOMPARE(spy.count(), 2);
    }

    void theBannerShowsForAnAttendedPairWhenEitherDriveIsWill()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        panel.applyDocument(twoIdleDrives([](QJsonObject &, QJsonObject &b) {
            b[QStringLiteral("verdict")] = QStringLiteral("will");
        }));
        QStringList prompts;
        panel.setConfirmer([&prompts](const QString &, const QString &text) {
            prompts << text;
            return false; // declined
        });
        p.attended->setChecked(true);
        p.selector->setCurrentIndex(2); // both
        QVERIFY2(p.upgrade->isEnabled(), qPrintable(p.upgrade->toolTip()));
        p.upgrade->click();
        QCOMPARE(prompts.size(), 1);
        QVERIFY(prompts[0].contains(QStringLiteral("this OS runs btrbk at boot")));
        QVERIFY(prompts[0].contains(QStringLiteral("system-recovery-B-2tb")));
        QVERIFY(p.upgrade->isEnabled()); // declined: nothing was requested

        p.selector->setCurrentIndex(1); // B alone, will
        p.upgrade->click();
        QCOMPARE(prompts.size(), 2);
        QVERIFY(p.upgrade->isEnabled());

        p.selector->setCurrentIndex(0); // A alone, may: no banner, requested
        p.upgrade->click();
        QCOMPARE(prompts.size(), 2);
        QVERIFY(!p.upgrade->isEnabled());
    }

    void aFailedRefreshDisablesEverythingWithoutLosingTheCards()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        panel.applyDocument(fixture());
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        p.selector->setCurrentIndex(0);
        QVERIFY(p.clear->isEnabled());

        Q_EMIT client.recoveryOsStatusError(QStringLiteral("polkit denied the status read"));
        for (QPushButton *b : {p.upgrade, p.schedule, p.clear, p.console, p.end}) {
            QVERIFY(!b->isEnabled());
            QVERIFY2(b->toolTip().contains(QStringLiteral("polkit denied")), qPrintable(b->toolTip()));
        }
        QCOMPARE(p.selector->count(), 3); // the cards and the selection are kept
        QVERIFY(panel.statusLine().contains(QStringLiteral("polkit denied")));

        // The next good document recovers it
        panel.applyDocument(fixture());
        QVERIFY(p.clear->isEnabled());
        QVERIFY(!panel.statusLine().contains(QStringLiteral("polkit denied")));
    }

    // --- Live-panel findings (Task 8, 2026-10-09) ----------------------------

    void tooltipsWrapAndCardTitlesDoNotRepeatTheSerial()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        panel.applyDocument(twoIdleDrives([](QJsonObject &a, QJsonObject &b) {
            // A display name that already carries the serial, as the author's config does
            a[QStringLiteral("display_name")] = QStringLiteral("2TB Recovery A (Bay 1, SER-system-recovery-A-2tb)");
            b[QStringLiteral("display_name")] = QStringLiteral("Drive B");
        }));
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        // Qt wraps only rich-text tooltips: every tooltip is one
        for (QPushButton *b : {p.upgrade, p.schedule, p.clear, p.console, p.end})
            QVERIFY2(b->toolTip().startsWith(QStringLiteral("<qt>")), qPrintable(b->toolTip()));
        QVERIFY(p.attended->toolTip().startsWith(QStringLiteral("<qt>")));
        QVERIFY(p.selector->toolTip().startsWith(QStringLiteral("<qt>")));
        // The serial is appended only when the display name lacks it
        const auto boxes = panel.findChildren<QGroupBox *>();
        QStringList titles;
        for (QGroupBox *g : boxes)
            titles << g->title();
        QVERIFY2(titles.contains(QStringLiteral("2TB Recovery A (Bay 1, SER-system-recovery-A-2tb)")), qPrintable(titles.join(QStringLiteral(" | "))));
        QVERIFY2(titles.contains(QStringLiteral("Drive B (SER-system-recovery-A-2tb)")), qPrintable(titles.join(QStringLiteral(" | "))));
    }
};

QTEST_MAIN(GuiSmokeTest)
#include "smoketest.moc"
