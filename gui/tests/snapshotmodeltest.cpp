#include <QTest>
#include <QTemporaryFile>
#include <QSqlDatabase>
#include <QSqlQuery>
#include "../src/snapshotmodel.h"
#include "../src/database.h"

class SnapshotModelTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void initTestCase();
    void hasDateGroupsAsTopLevel();
    void hasSnapshotsAsChildren();
    void returnsCorrectData();
    void parentOfChildIsDateGroup();
    void cleanupTestCase();

private:
    QString m_dbPath;
    Database m_database;
};

void SnapshotModelTest::initTestCase()
{
    QTemporaryFile tmp;
    tmp.setAutoRemove(false);
    QVERIFY(tmp.open());
    m_dbPath = tmp.fileName();
    tmp.close();

    QSqlDatabase db = QSqlDatabase::addDatabase(QStringLiteral("QSQLITE"), QStringLiteral("snmodel_setup"));
    db.setDatabaseName(m_dbPath);
    QVERIFY(db.open());
    QSqlQuery q(db);
    q.exec(QStringLiteral(
        "CREATE TABLE snapshots (id INTEGER PRIMARY KEY, name TEXT, ts TEXT, source TEXT, path TEXT UNIQUE, indexed_at INTEGER)"));
    q.exec(QStringLiteral(
        "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, name TEXT, size INTEGER DEFAULT 0, mtime INTEGER DEFAULT 0, type INTEGER DEFAULT 0)"));
    q.exec(QStringLiteral(
        "CREATE TABLE spans (file_id INTEGER, first_snap INTEGER, last_snap INTEGER, PRIMARY KEY(file_id, first_snap))"));
    q.exec(QStringLiteral(
        "INSERT INTO snapshots VALUES "
        "(1, 'root', '20260220T0304', 'nvme', '/mnt/backup/nvme/root.20260220T0304', 1740000000),"
        "(2, 'root', '20260221T0304', 'nvme', '/mnt/backup/nvme/root.20260221T0304', 1740100000),"
        "(3, 'home', '20260221T0304', 'nvme', '/mnt/backup/nvme/home.20260221T0304', 1740100000),"
        "(4, 'data', '20260221T0304', 'sata', '/mnt/backup/sata/data.20260221T0304', 1740100000)"));
    db.close();
    QSqlDatabase::removeDatabase(QStringLiteral("snmodel_setup"));

    QVERIFY(m_database.open(m_dbPath));
}

void SnapshotModelTest::hasDateGroupsAsTopLevel()
{
    SnapshotModel model(&m_database);
    model.reload();
    // 2 dates: 20260221 and 20260220 (ordered desc)
    QCOMPARE(model.rowCount(QModelIndex()), 2);
}

void SnapshotModelTest::hasSnapshotsAsChildren()
{
    SnapshotModel model(&m_database);
    model.reload();
    // First date group (20260221) has 3 snapshots
    auto firstDate = model.index(0, 0);
    QCOMPARE(model.rowCount(firstDate), 3);
    // Second date group (20260220) has 1 snapshot
    auto secondDate = model.index(1, 0);
    QCOMPARE(model.rowCount(secondDate), 1);
}

void SnapshotModelTest::returnsCorrectData()
{
    SnapshotModel model(&m_database);
    model.reload();
    // Date group display
    auto firstDate = model.index(0, 0);
    QVERIFY(model.data(firstDate, Qt::DisplayRole).toString().contains(QStringLiteral("2026-02-21")));
    // Snapshot display: source/name
    auto firstChild = model.index(0, 0, firstDate);
    QString label = model.data(firstChild, Qt::DisplayRole).toString();
    QVERIFY(label.contains(QLatin1Char('/')));
}

void SnapshotModelTest::parentOfChildIsDateGroup()
{
    SnapshotModel model(&m_database);
    model.reload();
    auto firstDate = model.index(0, 0);
    auto child = model.index(0, 0, firstDate);
    QCOMPARE(model.parent(child), firstDate);
}

void SnapshotModelTest::cleanupTestCase()
{
    m_database.close();
    QFile::remove(m_dbPath);
}

QTEST_GUILESS_MAIN(SnapshotModelTest)
#include "snapshotmodeltest.moc"
