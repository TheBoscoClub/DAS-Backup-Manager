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
#include <QTableView>

#include <optional>

#include "../src/dbusclient.h"
#include "../src/filemodel.h"
#include "../src/healthdashboard.h"
#include "../src/backuphistory.h"
#include "../src/backupsteps.h"
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
};

QTEST_MAIN(GuiSmokeTest)
#include "smoketest.moc"
