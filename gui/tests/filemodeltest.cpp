#include <QTest>
#include <QTemporaryFile>
#include <QSqlDatabase>
#include <QSqlQuery>
#include "../src/filemodel.h"
#include "../src/database.h"

class FileModelTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void initTestCase();
    void hasCorrectColumnCount();
    void loadsFilesForSnapshot();
    void returnsCorrectColumnData();
    void headerDataReturnsLabels();
    void cleanupTestCase();

private:
    QString m_dbPath;
    Database m_database;
};

void FileModelTest::initTestCase()
{
    QTemporaryFile tmp;
    tmp.setAutoRemove(false);
    QVERIFY(tmp.open());
    m_dbPath = tmp.fileName();
    tmp.close();

    QSqlDatabase db = QSqlDatabase::addDatabase(QStringLiteral("QSQLITE"), QStringLiteral("fmodel_setup"));
    db.setDatabaseName(m_dbPath);
    QVERIFY(db.open());
    QSqlQuery q(db);
    q.exec(QStringLiteral("CREATE TABLE snapshots (id INTEGER PRIMARY KEY, name TEXT, ts TEXT, source TEXT, path TEXT UNIQUE, indexed_at INTEGER)"));
    q.exec(QStringLiteral("CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, name TEXT, size INTEGER DEFAULT 0, mtime INTEGER DEFAULT 0, type INTEGER DEFAULT 0)"));
    q.exec(QStringLiteral("CREATE UNIQUE INDEX idx_files_path ON files(path)"));
    q.exec(QStringLiteral("CREATE TABLE spans (file_id INTEGER, first_snap INTEGER, last_snap INTEGER, PRIMARY KEY(file_id, first_snap))"));
    q.exec(QStringLiteral("INSERT INTO snapshots VALUES (1, 'root', '20260221T0304', 'nvme', '/snap1', 1740000000)"));
    q.exec(QStringLiteral("INSERT INTO files VALUES (1, 'etc/fstab', 'fstab', 1024, 1740000000, 0)"));
    q.exec(QStringLiteral("INSERT INTO files VALUES (2, 'home/bosco/.zshrc', '.zshrc', 512, 1740050000, 0)"));
    q.exec(QStringLiteral("INSERT INTO spans VALUES (1, 1, 1), (2, 1, 1)"));
    db.close();
    QSqlDatabase::removeDatabase(QStringLiteral("fmodel_setup"));

    QVERIFY(m_database.open(m_dbPath));
}

void FileModelTest::hasCorrectColumnCount()
{
    FileModel model(&m_database);
    QCOMPARE(model.columnCount(), 5);
}

void FileModelTest::loadsFilesForSnapshot()
{
    FileModel model(&m_database);
    model.loadSnapshot(1);
    QCOMPARE(model.rowCount(), 2);
}

void FileModelTest::returnsCorrectColumnData()
{
    FileModel model(&m_database);
    model.loadSnapshot(1);
    QCOMPARE(model.data(model.index(0, 0), Qt::DisplayRole).toString(), QStringLiteral("fstab"));
    QCOMPARE(model.data(model.index(0, 1), Qt::DisplayRole).toString(), QStringLiteral("etc/fstab"));
    QCOMPARE(model.data(model.index(0, 2), Qt::DisplayRole).toString(), QStringLiteral("1.0 KiB"));
}

void FileModelTest::headerDataReturnsLabels()
{
    FileModel model(&m_database);
    QCOMPARE(model.headerData(0, Qt::Horizontal).toString(), QStringLiteral("Name"));
    QCOMPARE(model.headerData(1, Qt::Horizontal).toString(), QStringLiteral("Path"));
    QCOMPARE(model.headerData(2, Qt::Horizontal).toString(), QStringLiteral("Size"));
    QCOMPARE(model.headerData(3, Qt::Horizontal).toString(), QStringLiteral("Modified"));
    QCOMPARE(model.headerData(4, Qt::Horizontal).toString(), QStringLiteral("Type"));
}

void FileModelTest::cleanupTestCase()
{
    m_database.close();
    QFile::remove(m_dbPath);
}

QTEST_GUILESS_MAIN(FileModelTest)
#include "filemodeltest.moc"
