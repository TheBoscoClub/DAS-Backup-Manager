#include <QTest>
#include <QTemporaryFile>
#include <QSqlDatabase>
#include <QSqlQuery>
#include "../src/searchmodel.h"
#include "../src/database.h"

class SearchModelTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void initTestCase();
    void hasCorrectColumnCount();
    void searchFindsResults();
    void searchReturnsEmpty();
    void headerLabelsCorrect();
    void cleanupTestCase();

private:
    QString m_dbPath;
    Database m_database;
};

void SearchModelTest::initTestCase()
{
    QTemporaryFile tmp;
    tmp.setAutoRemove(false);
    QVERIFY(tmp.open());
    m_dbPath = tmp.fileName();
    tmp.close();

    QSqlDatabase db = QSqlDatabase::addDatabase(QStringLiteral("QSQLITE"), QStringLiteral("smodel_setup"));
    db.setDatabaseName(m_dbPath);
    QVERIFY(db.open());
    QSqlQuery q(db);
    q.exec(QStringLiteral("CREATE TABLE snapshots (id INTEGER PRIMARY KEY, name TEXT, ts TEXT, source TEXT, path TEXT UNIQUE, indexed_at INTEGER)"));
    q.exec(QStringLiteral("CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, name TEXT, size INTEGER DEFAULT 0, mtime INTEGER DEFAULT 0, type INTEGER DEFAULT 0)"));
    q.exec(QStringLiteral("CREATE UNIQUE INDEX idx_files_path ON files(path)"));
    q.exec(QStringLiteral("CREATE TABLE spans (file_id INTEGER, first_snap INTEGER, last_snap INTEGER, PRIMARY KEY(file_id, first_snap))"));
    q.exec(QStringLiteral("CREATE VIRTUAL TABLE files_fts USING fts5(name, path, content=files, content_rowid=id)"));
    q.exec(QStringLiteral("CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path); END"));
    q.exec(QStringLiteral("INSERT INTO snapshots VALUES (1, 'root', '20260221T0304', 'nvme', '/snap1', 1740000000)"));
    q.exec(QStringLiteral("INSERT INTO files VALUES (1, 'docs/report.pdf', 'report.pdf', 15234, 1740000000, 0)"));
    q.exec(QStringLiteral("INSERT INTO files VALUES (2, 'photos/cat.jpg', 'cat.jpg', 50000, 1740000000, 0)"));
    q.exec(QStringLiteral("INSERT INTO spans VALUES (1, 1, 1), (2, 1, 1)"));
    db.close();
    QSqlDatabase::removeDatabase(QStringLiteral("smodel_setup"));

    QVERIFY(m_database.open(m_dbPath));
}

void SearchModelTest::hasCorrectColumnCount()
{
    SearchModel model(&m_database);
    QCOMPARE(model.columnCount(), 6);
}

void SearchModelTest::searchFindsResults()
{
    SearchModel model(&m_database);
    model.executeSearch(QStringLiteral("report"), 50);
    QCOMPARE(model.rowCount(), 1);
    QCOMPARE(model.data(model.index(0, 0), Qt::DisplayRole).toString(), QStringLiteral("docs/report.pdf"));
}

void SearchModelTest::searchReturnsEmpty()
{
    SearchModel model(&m_database);
    model.executeSearch(QStringLiteral("nonexistent"), 50);
    QCOMPARE(model.rowCount(), 0);
}

void SearchModelTest::headerLabelsCorrect()
{
    SearchModel model(&m_database);
    QCOMPARE(model.headerData(0, Qt::Horizontal).toString(), QStringLiteral("Path"));
    QCOMPARE(model.headerData(4, Qt::Horizontal).toString(), QStringLiteral("First Snapshot"));
    QCOMPARE(model.headerData(5, Qt::Horizontal).toString(), QStringLiteral("Last Snapshot"));
}

void SearchModelTest::cleanupTestCase()
{
    m_database.close();
    QFile::remove(m_dbPath);
}

QTEST_GUILESS_MAIN(SearchModelTest)
#include "searchmodeltest.moc"
