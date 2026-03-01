#include <QTest>
#include <QDateTime>
#include <QTemporaryFile>
#include <QSqlDatabase>
#include <QSqlQuery>
#include "../src/database.h"

class DatabaseTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void initTestCase();
    void opensDatabase();
    void returnsEmptyStatsForFreshDb();
    void listsSnapshotsFromPopulatedDb();
    void searchesFts5();
    void listFilesInSnapshot();
    void testBackupHistory();
    void testTargetUsageHistory();
    void cleanupTestCase();

private:
    QString m_dbPath;
};

void DatabaseTest::initTestCase()
{
    QTemporaryFile tmp;
    tmp.setAutoRemove(false);
    QVERIFY(tmp.open());
    m_dbPath = tmp.fileName();
    tmp.close();

    // Create schema matching the Rust indexer's SCHEMA_SQL
    QSqlDatabase db = QSqlDatabase::addDatabase(QStringLiteral("QSQLITE"), QStringLiteral("setup"));
    db.setDatabaseName(m_dbPath);
    QVERIFY(db.open());

    QSqlQuery q(db);
    QVERIFY(q.exec(QStringLiteral("PRAGMA journal_mode = WAL")));
    QVERIFY(q.exec(QStringLiteral("PRAGMA foreign_keys = ON")));

    // snapshots
    QVERIFY(q.exec(QStringLiteral(
        "CREATE TABLE snapshots ("
        "  id INTEGER PRIMARY KEY,"
        "  name TEXT NOT NULL,"
        "  ts TEXT NOT NULL,"
        "  source TEXT NOT NULL,"
        "  path TEXT NOT NULL UNIQUE,"
        "  indexed_at INTEGER NOT NULL)")));

    // files
    QVERIFY(q.exec(QStringLiteral(
        "CREATE TABLE files ("
        "  id INTEGER PRIMARY KEY,"
        "  path TEXT NOT NULL,"
        "  name TEXT NOT NULL,"
        "  size INTEGER NOT NULL DEFAULT 0,"
        "  mtime INTEGER NOT NULL DEFAULT 0,"
        "  type INTEGER NOT NULL DEFAULT 0)")));
    QVERIFY(q.exec(QStringLiteral("CREATE UNIQUE INDEX idx_files_path ON files(path)")));

    // spans
    QVERIFY(q.exec(QStringLiteral(
        "CREATE TABLE spans ("
        "  file_id INTEGER NOT NULL REFERENCES files(id),"
        "  first_snap INTEGER NOT NULL REFERENCES snapshots(id),"
        "  last_snap INTEGER NOT NULL REFERENCES snapshots(id),"
        "  PRIMARY KEY (file_id, first_snap))")));

    // FTS5
    QVERIFY(q.exec(QStringLiteral(
        "CREATE VIRTUAL TABLE files_fts USING fts5(name, path, content=files, content_rowid=id)")));
    QVERIFY(q.exec(QStringLiteral(
        "CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN "
        "INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path); END")));
    QVERIFY(q.exec(QStringLiteral(
        "CREATE TRIGGER files_au AFTER UPDATE ON files BEGIN "
        "INSERT INTO files_fts(files_fts, rowid, name, path) VALUES('delete', old.id, old.name, old.path); "
        "INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path); END")));

    // Seed test data
    QVERIFY(q.exec(QStringLiteral(
        "INSERT INTO snapshots (name, ts, source, path, indexed_at) VALUES "
        "('root', '20260220T0304', 'nvme', '/mnt/backup/nvme/root.20260220T0304', 1740000000),"
        "('root', '20260221T0304', 'nvme', '/mnt/backup/nvme/root.20260221T0304', 1740100000),"
        "('home', '20260221T0304', 'nvme', '/mnt/backup/nvme/home.20260221T0304', 1740100000)")));

    QVERIFY(q.exec(QStringLiteral(
        "INSERT INTO files (path, name, size, mtime, type) VALUES "
        "('etc/fstab', 'fstab', 1024, 1740000000, 0),"
        "('home/bosco/.zshrc', '.zshrc', 512, 1740000000, 0),"
        "('docs/report.pdf', 'report.pdf', 15234, 1740050000, 0)")));

    QVERIFY(q.exec(QStringLiteral(
        "INSERT INTO spans (file_id, first_snap, last_snap) VALUES "
        "(1, 1, 2),"
        "(2, 1, 2),"
        "(3, 2, 2)"
    )));

    db.close();
    QSqlDatabase::removeDatabase(QStringLiteral("setup"));
}

void DatabaseTest::opensDatabase()
{
    Database database;
    QVERIFY(database.open(m_dbPath));
    QVERIFY(database.isOpen());
}

void DatabaseTest::returnsEmptyStatsForFreshDb()
{
    QTemporaryFile freshTmp;
    freshTmp.setAutoRemove(true);
    QVERIFY(freshTmp.open());

    {
        QSqlDatabase db = QSqlDatabase::addDatabase(QStringLiteral("QSQLITE"), QStringLiteral("fresh"));
        db.setDatabaseName(freshTmp.fileName());
        QVERIFY(db.open());
        QSqlQuery q(db);
        q.exec(QStringLiteral(
            "CREATE TABLE snapshots (id INTEGER PRIMARY KEY, name TEXT, ts TEXT, source TEXT, path TEXT UNIQUE, indexed_at INTEGER)"));
        q.exec(QStringLiteral(
            "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, name TEXT, size INTEGER DEFAULT 0, mtime INTEGER DEFAULT 0, type INTEGER DEFAULT 0)"));
        q.exec(QStringLiteral(
            "CREATE TABLE spans (file_id INTEGER, first_snap INTEGER, last_snap INTEGER, PRIMARY KEY(file_id, first_snap))"));
        db.close();
        QSqlDatabase::removeDatabase(QStringLiteral("fresh"));
    }

    Database database;
    QVERIFY(database.open(freshTmp.fileName()));
    auto stats = database.stats();
    QCOMPARE(stats.snapshotCount, 0);
    QCOMPARE(stats.fileCount, 0);
    QCOMPARE(stats.spanCount, 0);
}

void DatabaseTest::listsSnapshotsFromPopulatedDb()
{
    Database database;
    QVERIFY(database.open(m_dbPath));

    auto snapshots = database.listSnapshots();
    QCOMPARE(snapshots.size(), 3);
    // Ordered by ts DESC, source, name
    // For ts 20260221T0304 with same source "nvme": home < root alphabetically
    QCOMPARE(snapshots[0].name, QStringLiteral("home"));
    QCOMPARE(snapshots[0].ts, QStringLiteral("20260221T0304"));
    QCOMPARE(snapshots[1].name, QStringLiteral("root"));
    QCOMPARE(snapshots[1].ts, QStringLiteral("20260221T0304"));
    QCOMPARE(snapshots[2].name, QStringLiteral("root"));
    QCOMPARE(snapshots[2].ts, QStringLiteral("20260220T0304"));
}

void DatabaseTest::searchesFts5()
{
    Database database;
    QVERIFY(database.open(m_dbPath));

    auto results = database.search(QStringLiteral("report"), 50);
    QCOMPARE(results.size(), 1);
    QCOMPARE(results[0].name, QStringLiteral("report.pdf"));
    QCOMPARE(results[0].size, 15234);
}

void DatabaseTest::listFilesInSnapshot()
{
    Database database;
    QVERIFY(database.open(m_dbPath));

    auto files = database.filesInSnapshot(2);
    QCOMPARE(files.size(), 3);
}

void DatabaseTest::testBackupHistory()
{
    // Add backup_runs table to existing test database
    {
        QSqlDatabase db = QSqlDatabase::addDatabase(
            QStringLiteral("QSQLITE"), QStringLiteral("history_setup"));
        db.setDatabaseName(m_dbPath);
        QVERIFY(db.open());

        QSqlQuery q(db);
        QVERIFY(q.exec(QStringLiteral(
            "CREATE TABLE IF NOT EXISTS backup_runs ("
            "id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, "
            "success INTEGER NOT NULL, mode TEXT NOT NULL, "
            "snaps_created INTEGER DEFAULT 0, snaps_sent INTEGER DEFAULT 0, "
            "bytes_sent INTEGER DEFAULT 0, duration_secs INTEGER DEFAULT 0, "
            "errors TEXT DEFAULT '')")));

        QVERIFY(q.exec(QStringLiteral(
            "INSERT INTO backup_runs "
            "(timestamp, success, mode, snaps_created, snaps_sent, "
            "bytes_sent, duration_secs, errors) VALUES "
            "(1709000000, 1, 'incremental', 5, 5, 1073741824, 3600, '')")));
        QVERIFY(q.exec(QStringLiteral(
            "INSERT INTO backup_runs "
            "(timestamp, success, mode, snaps_created, snaps_sent, "
            "bytes_sent, duration_secs, errors) VALUES "
            "(1709100000, 0, 'full', 2, 0, 0, 60, "
            "'target not mounted\nbtrbk failed')")));

        db.close();
        QSqlDatabase::removeDatabase(QStringLiteral("history_setup"));
    }

    Database database;
    QVERIFY(database.open(m_dbPath));

    auto history = database.getBackupHistory(10);
    QCOMPARE(history.size(), 2);

    // Most recent first
    QCOMPARE(history[0].timestamp, 1709100000LL);
    QVERIFY(!history[0].success);
    QCOMPARE(history[0].mode, QStringLiteral("full"));
    QCOMPARE(history[0].errors.size(), 2);
    QCOMPARE(history[0].errors[0], QStringLiteral("target not mounted"));

    QCOMPARE(history[1].timestamp, 1709000000LL);
    QVERIFY(history[1].success);
    QCOMPARE(history[1].bytesSent, 1073741824LL);
}

void DatabaseTest::testTargetUsageHistory()
{
    const qint64 now = QDateTime::currentSecsSinceEpoch();

    {
        QSqlDatabase db = QSqlDatabase::addDatabase(
            QStringLiteral("QSQLITE"), QStringLiteral("usage_setup"));
        db.setDatabaseName(m_dbPath);
        QVERIFY(db.open());

        QSqlQuery q(db);
        QVERIFY(q.exec(QStringLiteral(
            "CREATE TABLE IF NOT EXISTS target_usage ("
            "id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, "
            "label TEXT NOT NULL, total_bytes INTEGER DEFAULT 0, "
            "used_bytes INTEGER DEFAULT 0, snapshot_count INTEGER DEFAULT 0)")));

        q.prepare(QStringLiteral(
            "INSERT INTO target_usage "
            "(timestamp, label, total_bytes, used_bytes, snapshot_count) "
            "VALUES (:ts, :label, :total, :used, :count)"));

        q.bindValue(QStringLiteral(":ts"), now - 86400);
        q.bindValue(QStringLiteral(":label"), QStringLiteral("backup-22tb"));
        q.bindValue(QStringLiteral(":total"), 22000000000000LL);
        q.bindValue(QStringLiteral(":used"), 15000000000000LL);
        q.bindValue(QStringLiteral(":count"), 365);
        QVERIFY(q.exec());

        q.bindValue(QStringLiteral(":ts"), now - 3600);
        q.bindValue(QStringLiteral(":label"), QStringLiteral("backup-22tb"));
        q.bindValue(QStringLiteral(":total"), 22000000000000LL);
        q.bindValue(QStringLiteral(":used"), 15100000000000LL);
        q.bindValue(QStringLiteral(":count"), 366);
        QVERIFY(q.exec());

        db.close();
        QSqlDatabase::removeDatabase(QStringLiteral("usage_setup"));
    }

    Database database;
    QVERIFY(database.open(m_dbPath));

    auto usage = database.getTargetUsageHistory(QStringLiteral("backup-22tb"), 7);
    QCOMPARE(usage.size(), 2);
    QCOMPARE(usage[0].label, QStringLiteral("backup-22tb"));
    QVERIFY(usage[1].usedBytes > usage[0].usedBytes);
}

void DatabaseTest::cleanupTestCase()
{
    QFile::remove(m_dbPath);
}

QTEST_GUILESS_MAIN(DatabaseTest)
#include "databasetest.moc"
