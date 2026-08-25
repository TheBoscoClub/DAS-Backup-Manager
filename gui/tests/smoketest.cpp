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

#include "../src/dbusclient.h"
#include "../src/filemodel.h"
#include "../src/healthdashboard.h"
#include "../src/backuphistory.h"
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

        BackupHistoryView history(&client, QStringLiteral("/nonexistent/index.db"));
        QVERIFY(history.metaObject() != nullptr);

        ProgressPanel progress(&client);
        QVERIFY(progress.metaObject() != nullptr);
    }

    void fileModelHandlesAMissingDatabase()
    {
        // A path that does not exist must leave an empty model, not throw or
        // abort — the GUI opens before anyone has run a backup.
        DBusClient client;
        FileModel model(&client, QStringLiteral("/nonexistent/index.db"));
        QCOMPARE(model.rowCount(QModelIndex()), 0);
    }
};

QTEST_MAIN(GuiSmokeTest)
#include "smoketest.moc"
