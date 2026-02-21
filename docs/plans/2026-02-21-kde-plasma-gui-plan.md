# ButteredDASD KDE Plasma GUI — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a native KDE Plasma application (`btrdasd-gui`) for searching, browsing, and restoring files from BTRFS backup snapshots indexed by the ButteredDASD content indexer.

**Architecture:** Hybrid KF6 widgets + custom-painted timeline widget. KXmlGuiWindow shell, QSqlDatabase (QSQLITE) for read-only database access, KIO for restores, QProcess for spawning `btrdasd walk`, QFileSystemWatcher for auto-detecting new snapshots.

**Tech Stack:** C++20, Qt6 6.10.2 (Core, Widgets, Sql, Test), KDE Frameworks 6.23.0 (XmlGui, I18n, KIO, CoreAddons, ConfigWidgets, IconThemes, Crash), ECM 6.23.0, CMake 4.2.3

**Design doc:** `docs/plans/2026-02-21-kde-plasma-gui-design.md`

**Build conventions:** `.claude/rules/build.md` — `-Wall -Wextra -Wpedantic -Werror`, new-style signal/slot `connect()`, prepared statements only, WAL journal mode

**Database schema (read-only, written by `btrdasd` indexer):**
- `snapshots`: id, name, ts, source, path (UNIQUE), indexed_at
- `files`: id, path (UNIQUE), name, size, mtime, type (0=regular, 1=dir, 2=symlink, 3=other)
- `spans`: file_id FK→files, first_snap FK→snapshots, last_snap FK→snapshots, PK(file_id, first_snap)
- `files_fts`: FTS5 virtual table on (name, path), content=files, synced via triggers
- Indexes: idx_snapshots_source_name, idx_snapshots_ts, idx_spans_file_id, idx_files_name, idx_files_path, idx_spans_last

**Default database path:** `/var/lib/das-backup/backup-index.db`

---

### Task 1: CMake scaffold and empty KDE application window

**Files:**
- Create: `gui/CMakeLists.txt`
- Create: `gui/src/main.cpp`
- Create: `gui/src/mainwindow.h`
- Create: `gui/src/mainwindow.cpp`
- Create: `gui/src/btrdasd-gui.rc`
- Modify: `CMakeLists.txt` (root — uncomment `add_subdirectory(gui)`)

**Step 1: Create `gui/CMakeLists.txt`**

```cmake
cmake_minimum_required(VERSION 3.25)

set(QT_MIN_VERSION "6.6.0")
set(KF_MIN_VERSION "6.0.0")

find_package(ECM ${KF_MIN_VERSION} REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings NO_POLICY_SCOPE)
include(FeatureSummary)

find_package(Qt6 ${QT_MIN_VERSION} CONFIG REQUIRED COMPONENTS
    Core
    Widgets
    Sql
)

find_package(KF6 ${KF_MIN_VERSION} REQUIRED COMPONENTS
    CoreAddons
    I18n
    XmlGui
    ConfigWidgets
    IconThemes
    Crash
    KIO
)

add_executable(btrdasd-gui)

target_sources(btrdasd-gui PRIVATE
    src/main.cpp
    src/mainwindow.cpp
)

target_compile_options(btrdasd-gui PRIVATE -Wall -Wextra -Wpedantic -Werror)

target_link_libraries(btrdasd-gui PRIVATE
    Qt6::Core
    Qt6::Widgets
    Qt6::Sql
    KF6::CoreAddons
    KF6::I18n
    KF6::XmlGui
    KF6::ConfigWidgets
    KF6::IconThemes
    KF6::Crash
    KF6::KIOWidgets
)

install(TARGETS btrdasd-gui ${KDE_INSTALL_TARGETS_DEFAULT_ARGS})
install(FILES src/btrdasd-gui.rc DESTINATION ${KDE_INSTALL_KXMLGUIDIR}/btrdasd-gui)

feature_summary(WHAT ALL INCLUDE_QUIET_PACKAGES FATAL_ON_MISSING_REQUIRED_PACKAGES)
```

**Step 2: Create `gui/src/mainwindow.h`**

```cpp
#pragma once

#include <KXmlGuiWindow>

class MainWindow : public KXmlGuiWindow
{
    Q_OBJECT

public:
    explicit MainWindow(QWidget *parent = nullptr);
    ~MainWindow() override;
};
```

**Step 3: Create `gui/src/mainwindow.cpp`**

```cpp
#include "mainwindow.h"

#include <QLabel>
#include <QStatusBar>

MainWindow::MainWindow(QWidget *parent)
    : KXmlGuiWindow(parent)
{
    auto *placeholder = new QLabel(QStringLiteral("ButteredDASD"), this);
    placeholder->setAlignment(Qt::AlignCenter);
    setCentralWidget(placeholder);

    statusBar()->showMessage(QStringLiteral("Ready"));

    setupGUI(Default, QStringLiteral("btrdasd-gui.rc"));
}

MainWindow::~MainWindow() = default;
```

**Step 4: Create `gui/src/main.cpp`**

```cpp
#include <QApplication>
#include <QCommandLineParser>

#include <KAboutData>
#include <KCrash>
#include <KLocalizedString>

#include "mainwindow.h"

using namespace Qt::Literals::StringLiterals;

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);

    KLocalizedString::setApplicationDomain("btrdasd-gui");

    KAboutData aboutData(
        u"btrdasd-gui"_s,
        i18n("ButteredDASD"),
        u"0.1.0"_s,
        i18n("Search, browse, and restore files from BTRFS backup snapshots"),
        KAboutLicense::GPL_V3,
        i18n("(c) 2026 TheBoscoClub"),
        QString(),
        u"https://github.com/TheBoscoClub/DAS-Backup-Manager"_s);

    aboutData.addAuthor(
        i18n("Bosco"),
        i18n("Developer"),
        u"bosco@theboscoclub.com"_s);

    KAboutData::setApplicationData(aboutData);

    KCrash::initialize();

    QCommandLineParser parser;
    aboutData.setupCommandLine(&parser);
    parser.process(app);
    aboutData.processCommandLine(&parser);

    auto *window = new MainWindow();
    window->show();

    return app.exec();
}
```

**Step 5: Create `gui/src/btrdasd-gui.rc`**

```xml
<?xml version="1.0" encoding="UTF-8"?>
<gui name="btrdasd-gui"
     version="1"
     xmlns="http://www.kde.org/standards/kxmlgui/1.0"
     xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
     xsi:schemaLocation="http://www.kde.org/standards/kxmlgui/1.0
                          http://www.kde.org/standards/kxmlgui/1.0/kxmlgui.xsd" >
  <MenuBar>
    <Menu name="file" >
      <text>&amp;File</text>
    </Menu>
  </MenuBar>
  <ToolBar name="mainToolBar" >
    <text>Main Toolbar</text>
  </ToolBar>
</gui>
```

**Step 6: Modify root `CMakeLists.txt`**

Replace:
```cmake
# add_subdirectory(gui)      # Qt6/KDE Plasma backup browser
```
With:
```cmake
add_subdirectory(gui)
```

**Step 7: Build and verify**

Run:
```bash
cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager
cmake -B build -DCMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build build --target btrdasd-gui
```
Expected: Compiles without errors or warnings. Binary at `build/gui/btrdasd-gui`.

Run the binary briefly to verify it opens:
```bash
./build/gui/btrdasd-gui &
sleep 2
kill %1
```
Expected: KDE window appears with "ButteredDASD" placeholder text, menu bar, toolbar, status bar.

**Step 8: Commit**

```bash
git add gui/ CMakeLists.txt
git commit -m "feat(gui): scaffold KDE Plasma application with KXmlGuiWindow"
```

---

### Task 2: Database connection wrapper

**Files:**
- Create: `gui/src/database.h`
- Create: `gui/src/database.cpp`
- Create: `gui/tests/databasetest.cpp`
- Modify: `gui/CMakeLists.txt` (add database.cpp to target, add test target)

**Context:** The GUI opens the existing SQLite database in read-only mode. All writes are done by the Rust `btrdasd` indexer. The Database class wraps QSqlDatabase and provides typed query methods for snapshots, files, spans, and FTS5 search.

**Step 1: Add test infrastructure to `gui/CMakeLists.txt`**

Append after the `install` lines:

```cmake
# Tests
if(BUILD_TESTING)
    find_package(Qt6 ${QT_MIN_VERSION} CONFIG REQUIRED COMPONENTS Test)
    include(ECMAddTests)

    ecm_add_test(tests/databasetest.cpp src/database.cpp
        TEST_NAME databasetest
        LINK_LIBRARIES Qt6::Test Qt6::Sql
    )
endif()
```

**Step 2: Write failing test `gui/tests/databasetest.cpp`**

```cpp
#include <QTest>
#include <QTemporaryFile>
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
    void cleanupTestCase();

private:
    QString m_dbPath;
};

void DatabaseTest::initTestCase()
{
    QTemporaryFile tmp;
    tmp.setAutoRemove(false);
    tmp.open();
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
    freshTmp.open();

    {
        QSqlDatabase db = QSqlDatabase::addDatabase(QStringLiteral("QSQLITE"), QStringLiteral("fresh"));
        db.setDatabaseName(freshTmp.fileName());
        db.open();
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
    // Ordered by ts DESC
    QCOMPARE(snapshots[0].name, QStringLiteral("root"));
    QCOMPARE(snapshots[0].ts, QStringLiteral("20260221T0304"));
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

void DatabaseTest::cleanupTestCase()
{
    QFile::remove(m_dbPath);
}

QTEST_GUILESS_MAIN(DatabaseTest)
#include "databasetest.moc"
```

**Step 3: Run test to verify it fails**

Run:
```bash
cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager
cmake -B build -DCMAKE_BUILD_TYPE=RelWithDebInfo -DBUILD_TESTING=ON
cmake --build build --target databasetest 2>&1
```
Expected: FAIL — `database.h` not found.

**Step 4: Create `gui/src/database.h`**

```cpp
#pragma once

#include <QSqlDatabase>
#include <QString>
#include <QVector>

struct SnapshotInfo {
    qint64 id = 0;
    QString name;
    QString ts;
    QString source;
    QString path;
    qint64 indexedAt = 0;
};

struct FileInfo {
    qint64 id = 0;
    QString path;
    QString name;
    qint64 size = 0;
    qint64 mtime = 0;
    int type = 0; // 0=regular, 1=dir, 2=symlink, 3=other
};

struct SearchResult {
    QString path;
    QString name;
    qint64 size = 0;
    qint64 mtime = 0;
    QString firstSnap;
    QString lastSnap;
};

struct DbStats {
    qint64 snapshotCount = 0;
    qint64 fileCount = 0;
    qint64 spanCount = 0;
    qint64 dbSizeBytes = 0;
};

class Database
{
public:
    Database();
    ~Database();

    bool open(const QString &path);
    void close();
    [[nodiscard]] bool isOpen() const;

    [[nodiscard]] QVector<SnapshotInfo> listSnapshots() const;
    [[nodiscard]] QVector<FileInfo> filesInSnapshot(qint64 snapshotId) const;
    [[nodiscard]] QVector<SearchResult> search(const QString &query, qint64 limit) const;
    [[nodiscard]] DbStats stats() const;

private:
    QSqlDatabase m_db;
    QString m_connectionName;
};
```

**Step 5: Create `gui/src/database.cpp`**

```cpp
#include "database.h"

#include <QFile>
#include <QSqlError>
#include <QSqlQuery>
#include <QUuid>

Database::Database()
    : m_connectionName(QUuid::createUuid().toString())
{
}

Database::~Database()
{
    close();
}

bool Database::open(const QString &path)
{
    m_db = QSqlDatabase::addDatabase(QStringLiteral("QSQLITE"), m_connectionName);
    m_db.setDatabaseName(path);

    if (!m_db.open()) {
        return false;
    }

    QSqlQuery q(m_db);
    q.exec(QStringLiteral("PRAGMA journal_mode = WAL"));
    q.exec(QStringLiteral("PRAGMA foreign_keys = ON"));
    q.exec(QStringLiteral("PRAGMA query_only = ON"));

    return true;
}

void Database::close()
{
    if (m_db.isOpen()) {
        QSqlQuery q(m_db);
        q.exec(QStringLiteral("PRAGMA optimize"));
        m_db.close();
    }
    if (QSqlDatabase::contains(m_connectionName)) {
        QSqlDatabase::removeDatabase(m_connectionName);
    }
}

bool Database::isOpen() const
{
    return m_db.isOpen();
}

QVector<SnapshotInfo> Database::listSnapshots() const
{
    QVector<SnapshotInfo> result;
    QSqlQuery q(m_db);
    q.prepare(QStringLiteral(
        "SELECT id, name, ts, source, path, indexed_at "
        "FROM snapshots ORDER BY ts DESC, source, name"));
    if (!q.exec()) {
        return result;
    }
    while (q.next()) {
        result.append({
            .id = q.value(0).toLongLong(),
            .name = q.value(1).toString(),
            .ts = q.value(2).toString(),
            .source = q.value(3).toString(),
            .path = q.value(4).toString(),
            .indexedAt = q.value(5).toLongLong(),
        });
    }
    return result;
}

QVector<FileInfo> Database::filesInSnapshot(qint64 snapshotId) const
{
    QVector<FileInfo> result;
    QSqlQuery q(m_db);
    q.prepare(QStringLiteral(
        "SELECT f.id, f.path, f.name, f.size, f.mtime, f.type "
        "FROM files f "
        "JOIN spans s ON s.file_id = f.id "
        "WHERE s.first_snap <= :snapId AND s.last_snap >= :snapId "
        "ORDER BY f.path"));
    q.bindValue(QStringLiteral(":snapId"), snapshotId);
    if (!q.exec()) {
        return result;
    }
    while (q.next()) {
        result.append({
            .id = q.value(0).toLongLong(),
            .path = q.value(1).toString(),
            .name = q.value(2).toString(),
            .size = q.value(3).toLongLong(),
            .mtime = q.value(4).toLongLong(),
            .type = q.value(5).toInt(),
        });
    }
    return result;
}

QVector<SearchResult> Database::search(const QString &query, qint64 limit) const
{
    QVector<SearchResult> result;

    // Wrap bare terms in quotes so FTS5 treats dots/hyphens as literals.
    // Preserve explicit FTS5 syntax: prefix (*), column filters (:), quoted phrases.
    QString ftsQuery = query;
    if (!query.contains(QLatin1Char('*')) &&
        !query.contains(QLatin1Char(':')) &&
        !query.contains(QLatin1Char('"'))) {
        ftsQuery = QLatin1Char('"') + query + QLatin1Char('"');
    }

    QSqlQuery q(m_db);
    q.prepare(QStringLiteral(
        "SELECT f.path, f.name, f.size, f.mtime, "
        "  s1.source || '/' || s1.name || '.' || s1.ts AS first_snap, "
        "  s2.source || '/' || s2.name || '.' || s2.ts AS last_snap "
        "FROM files_fts "
        "JOIN files f ON f.id = files_fts.rowid "
        "JOIN spans sp ON sp.file_id = f.id "
        "JOIN snapshots s1 ON s1.id = sp.first_snap "
        "JOIN snapshots s2 ON s2.id = sp.last_snap "
        "WHERE files_fts MATCH :query "
        "ORDER BY rank "
        "LIMIT :limit"));
    q.bindValue(QStringLiteral(":query"), ftsQuery);
    q.bindValue(QStringLiteral(":limit"), limit);
    if (!q.exec()) {
        return result;
    }
    while (q.next()) {
        result.append({
            .path = q.value(0).toString(),
            .name = q.value(1).toString(),
            .size = q.value(2).toLongLong(),
            .mtime = q.value(3).toLongLong(),
            .firstSnap = q.value(4).toString(),
            .lastSnap = q.value(5).toString(),
        });
    }
    return result;
}

DbStats Database::stats() const
{
    DbStats s;
    QSqlQuery q(m_db);

    if (q.exec(QStringLiteral("SELECT COUNT(*) FROM snapshots")) && q.next())
        s.snapshotCount = q.value(0).toLongLong();
    if (q.exec(QStringLiteral("SELECT COUNT(*) FROM files")) && q.next())
        s.fileCount = q.value(0).toLongLong();
    if (q.exec(QStringLiteral("SELECT COUNT(*) FROM spans")) && q.next())
        s.spanCount = q.value(0).toLongLong();
    if (q.exec(QStringLiteral("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()")) && q.next())
        s.dbSizeBytes = q.value(0).toLongLong();

    return s;
}
```

**Step 6: Add database.cpp to the main target in `gui/CMakeLists.txt`**

In `target_sources(btrdasd-gui PRIVATE`, add `src/database.cpp` after `src/mainwindow.cpp`.

**Step 7: Build and run tests**

```bash
cmake -B build -DCMAKE_BUILD_TYPE=RelWithDebInfo -DBUILD_TESTING=ON
cmake --build build --target databasetest
cd build/gui && ctest --test-dir . --output-on-failure -R databasetest
```
Expected: All 6 tests PASS.

**Step 8: Commit**

```bash
git add gui/src/database.h gui/src/database.cpp gui/tests/databasetest.cpp gui/CMakeLists.txt
git commit -m "feat(gui): database connection wrapper with read-only SQLite access"
```

---

### Task 3: Snapshot model (QAbstractItemModel for tree/timeline)

**Files:**
- Create: `gui/src/snapshotmodel.h`
- Create: `gui/src/snapshotmodel.cpp`
- Create: `gui/tests/snapshotmodeltest.cpp`
- Modify: `gui/CMakeLists.txt` (add sources and test)

**Context:** The SnapshotModel provides a two-level tree structure: top-level rows are dates (grouping key = ts substring before 'T'), child rows are individual snapshots under that date. This backs both the custom timeline widget and any tree-based views. It inherits `QAbstractItemModel` and returns date groups as parent items and snapshots as child items.

**Step 1: Write failing test `gui/tests/snapshotmodeltest.cpp`**

```cpp
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
    tmp.open();
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
```

**Step 2: Run test to verify it fails**

```bash
cmake --build build --target snapshotmodeltest 2>&1
```
Expected: FAIL — `snapshotmodel.h` not found.

**Step 3: Create `gui/src/snapshotmodel.h`**

```cpp
#pragma once

#include <QAbstractItemModel>
#include <QVector>
#include "database.h"

class SnapshotModel : public QAbstractItemModel
{
    Q_OBJECT

public:
    enum Roles {
        SnapshotIdRole = Qt::UserRole + 1,
        SnapshotPathRole,
        SnapshotSourceRole,
        IsDateGroupRole,
    };

    explicit SnapshotModel(Database *database, QObject *parent = nullptr);

    void reload();

    [[nodiscard]] QModelIndex index(int row, int column,
                                     const QModelIndex &parent = {}) const override;
    [[nodiscard]] QModelIndex parent(const QModelIndex &index) const override;
    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;

private:
    struct DateGroup {
        QString date;
        QVector<int> snapIndices;
    };

    Database *m_database;
    QVector<SnapshotInfo> m_snapshots;
    QVector<DateGroup> m_groups;

    static QString tsToDate(const QString &ts);
};
```

**Step 4: Create `gui/src/snapshotmodel.cpp`**

```cpp
#include "snapshotmodel.h"

SnapshotModel::SnapshotModel(Database *database, QObject *parent)
    : QAbstractItemModel(parent)
    , m_database(database)
{
}

QString SnapshotModel::tsToDate(const QString &ts)
{
    // ts format: "20260221T0304" -> "2026-02-21"
    if (ts.length() < 8) return ts;
    return ts.left(4) + QLatin1Char('-') + ts.mid(4, 2) + QLatin1Char('-') + ts.mid(6, 2);
}

void SnapshotModel::reload()
{
    beginResetModel();
    m_snapshots = m_database->listSnapshots();
    m_groups.clear();

    for (int i = 0; i < m_snapshots.size(); ++i) {
        QString date = tsToDate(m_snapshots[i].ts);
        if (m_groups.isEmpty() || m_groups.last().date != date) {
            m_groups.append({.date = date, .snapIndices = {}});
        }
        m_groups.last().snapIndices.append(i);
    }
    endResetModel();
}

QModelIndex SnapshotModel::index(int row, int column, const QModelIndex &parent) const
{
    if (!hasIndex(row, column, parent))
        return {};

    if (!parent.isValid()) {
        // Top-level: date group. internalId 0 = top-level.
        return createIndex(row, column, quintptr(0));
    }

    // Child: snapshot. Encode parent group index in internal id.
    int groupIdx = parent.row();
    return createIndex(row, column, quintptr(groupIdx + 1));
}

QModelIndex SnapshotModel::parent(const QModelIndex &index) const
{
    if (!index.isValid())
        return {};

    quintptr id = index.internalId();
    if (id == 0) {
        return {};
    }

    int groupIdx = static_cast<int>(id) - 1;
    return createIndex(groupIdx, 0, quintptr(0));
}

int SnapshotModel::rowCount(const QModelIndex &parent) const
{
    if (!parent.isValid()) {
        return m_groups.size();
    }

    if (parent.internalId() == 0 && parent.row() < m_groups.size()) {
        return m_groups[parent.row()].snapIndices.size();
    }

    return 0;
}

int SnapshotModel::columnCount(const QModelIndex & /*parent*/) const
{
    return 1;
}

QVariant SnapshotModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid())
        return {};

    quintptr id = index.internalId();

    if (id == 0) {
        if (index.row() >= m_groups.size()) return {};
        const auto &group = m_groups[index.row()];

        switch (role) {
        case Qt::DisplayRole:
            return group.date;
        case IsDateGroupRole:
            return true;
        default:
            return {};
        }
    }

    int groupIdx = static_cast<int>(id) - 1;
    if (groupIdx >= m_groups.size()) return {};
    const auto &group = m_groups[groupIdx];
    if (index.row() >= group.snapIndices.size()) return {};

    const auto &snap = m_snapshots[group.snapIndices[index.row()]];

    switch (role) {
    case Qt::DisplayRole:
        return snap.source + QLatin1Char('/') + snap.name + QLatin1Char('.') + snap.ts;
    case SnapshotIdRole:
        return snap.id;
    case SnapshotPathRole:
        return snap.path;
    case SnapshotSourceRole:
        return snap.source;
    case IsDateGroupRole:
        return false;
    default:
        return {};
    }
}
```

**Step 5: Add sources and test to CMakeLists.txt**

Add `src/snapshotmodel.cpp` to `target_sources(btrdasd-gui PRIVATE`.

Add test:
```cmake
ecm_add_test(tests/snapshotmodeltest.cpp src/snapshotmodel.cpp src/database.cpp
    TEST_NAME snapshotmodeltest
    LINK_LIBRARIES Qt6::Test Qt6::Sql
)
```

**Step 6: Build and run tests**

```bash
cmake --build build
cd build/gui && ctest --test-dir . --output-on-failure
```
Expected: All tests PASS (database + snapshotmodel).

**Step 7: Commit**

```bash
git add gui/src/snapshotmodel.h gui/src/snapshotmodel.cpp gui/tests/snapshotmodeltest.cpp gui/CMakeLists.txt
git commit -m "feat(gui): snapshot model with date-grouped tree structure"
```

---

### Task 4: File model (QAbstractTableModel for file list)

**Files:**
- Create: `gui/src/filemodel.h`
- Create: `gui/src/filemodel.cpp`
- Create: `gui/tests/filemodeltest.cpp`
- Modify: `gui/CMakeLists.txt`

**Context:** FileModel provides the data for the right-side file list QTableView. When the user selects a snapshot, we call `loadSnapshot(snapshotId)` which queries the database for all files in that snapshot. Columns: Name, Path, Size, Modified, Type.

**Step 1: Write failing test `gui/tests/filemodeltest.cpp`**

```cpp
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
    tmp.open();
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
```

**Step 2: Create `gui/src/filemodel.h`**

```cpp
#pragma once

#include <QAbstractTableModel>
#include <QVector>
#include "database.h"

class FileModel : public QAbstractTableModel
{
    Q_OBJECT

public:
    enum Column { Name = 0, Path, Size, Modified, Type, ColumnCount };
    enum Roles { FileIdRole = Qt::UserRole + 1, FilePathRole };

    explicit FileModel(Database *database, QObject *parent = nullptr);

    void loadSnapshot(qint64 snapshotId);
    void clear();

    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    [[nodiscard]] QVariant headerData(int section, Qt::Orientation orientation,
                                       int role = Qt::DisplayRole) const override;

    static QString formatSize(qint64 bytes);

private:
    Database *m_database;
    QVector<FileInfo> m_files;
};
```

**Step 3: Create `gui/src/filemodel.cpp`**

```cpp
#include "filemodel.h"

#include <QDateTime>

FileModel::FileModel(Database *database, QObject *parent)
    : QAbstractTableModel(parent)
    , m_database(database)
{
}

void FileModel::loadSnapshot(qint64 snapshotId)
{
    beginResetModel();
    m_files = m_database->filesInSnapshot(snapshotId);
    endResetModel();
}

void FileModel::clear()
{
    beginResetModel();
    m_files.clear();
    endResetModel();
}

int FileModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_files.size();
}

int FileModel::columnCount(const QModelIndex & /*parent*/) const
{
    return ColumnCount;
}

QVariant FileModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() >= m_files.size())
        return {};

    const auto &file = m_files[index.row()];

    if (role == FileIdRole) return file.id;
    if (role == FilePathRole) return file.path;

    if (role != Qt::DisplayRole)
        return {};

    switch (index.column()) {
    case Name:     return file.name;
    case Path:     return file.path;
    case Size:     return formatSize(file.size);
    case Modified: return QDateTime::fromSecsSinceEpoch(file.mtime).toString(QStringLiteral("yyyy-MM-dd hh:mm"));
    case Type: {
        switch (file.type) {
        case 0: return QStringLiteral("File");
        case 1: return QStringLiteral("Directory");
        case 2: return QStringLiteral("Symlink");
        default: return QStringLiteral("Other");
        }
    }
    default: return {};
    }
}

QVariant FileModel::headerData(int section, Qt::Orientation orientation, int role) const
{
    if (orientation != Qt::Horizontal || role != Qt::DisplayRole)
        return {};

    switch (section) {
    case Name:     return QStringLiteral("Name");
    case Path:     return QStringLiteral("Path");
    case Size:     return QStringLiteral("Size");
    case Modified: return QStringLiteral("Modified");
    case Type:     return QStringLiteral("Type");
    default:       return {};
    }
}

QString FileModel::formatSize(qint64 bytes)
{
    if (bytes < 1024) return QString::number(bytes) + QStringLiteral(" B");
    if (bytes < 1024 * 1024) return QString::number(bytes / 1024.0, 'f', 1) + QStringLiteral(" KiB");
    if (bytes < 1024LL * 1024 * 1024) return QString::number(bytes / (1024.0 * 1024.0), 'f', 1) + QStringLiteral(" MiB");
    return QString::number(bytes / (1024.0 * 1024.0 * 1024.0), 'f', 1) + QStringLiteral(" GiB");
}
```

**Step 4: Add to CMakeLists.txt and build/test**

Add `src/filemodel.cpp` to main target. Add test:
```cmake
ecm_add_test(tests/filemodeltest.cpp src/filemodel.cpp src/database.cpp
    TEST_NAME filemodeltest
    LINK_LIBRARIES Qt6::Test Qt6::Sql
)
```

```bash
cmake --build build
cd build/gui && ctest --test-dir . --output-on-failure
```
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add gui/src/filemodel.h gui/src/filemodel.cpp gui/tests/filemodeltest.cpp gui/CMakeLists.txt
git commit -m "feat(gui): file model with snapshot loading and size formatting"
```

---

### Task 5: Search model (FTS5 results table model)

**Files:**
- Create: `gui/src/searchmodel.h`
- Create: `gui/src/searchmodel.cpp`
- Create: `gui/tests/searchmodeltest.cpp`
- Modify: `gui/CMakeLists.txt`

**Context:** SearchModel wraps FTS5 search results in a QAbstractTableModel. Columns: Path, Name, Size, Modified, First Snapshot, Last Snapshot. It calls `database.search()` and presents results.

**Step 1: Write failing test `gui/tests/searchmodeltest.cpp`**

```cpp
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
    tmp.open();
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
```

**Step 2: Create `gui/src/searchmodel.h`**

```cpp
#pragma once

#include <QAbstractTableModel>
#include <QVector>
#include "database.h"

class SearchModel : public QAbstractTableModel
{
    Q_OBJECT

public:
    enum Column { Path = 0, Name, Size, Modified, FirstSnapshot, LastSnapshot, ColumnCount };

    explicit SearchModel(Database *database, QObject *parent = nullptr);

    void executeSearch(const QString &query, qint64 limit);
    void clear();

    [[nodiscard]] int rowCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] int columnCount(const QModelIndex &parent = {}) const override;
    [[nodiscard]] QVariant data(const QModelIndex &index, int role = Qt::DisplayRole) const override;
    [[nodiscard]] QVariant headerData(int section, Qt::Orientation orientation,
                                       int role = Qt::DisplayRole) const override;

private:
    Database *m_database;
    QVector<SearchResult> m_results;
};
```

**Step 3: Create `gui/src/searchmodel.cpp`**

```cpp
#include "searchmodel.h"
#include "filemodel.h"

#include <QDateTime>

SearchModel::SearchModel(Database *database, QObject *parent)
    : QAbstractTableModel(parent)
    , m_database(database)
{
}

void SearchModel::executeSearch(const QString &query, qint64 limit)
{
    beginResetModel();
    m_results = m_database->search(query, limit);
    endResetModel();
}

void SearchModel::clear()
{
    beginResetModel();
    m_results.clear();
    endResetModel();
}

int SearchModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_results.size();
}

int SearchModel::columnCount(const QModelIndex & /*parent*/) const
{
    return ColumnCount;
}

QVariant SearchModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() >= m_results.size())
        return {};
    if (role != Qt::DisplayRole)
        return {};

    const auto &r = m_results[index.row()];
    switch (index.column()) {
    case Path:          return r.path;
    case Name:          return r.name;
    case Size:          return FileModel::formatSize(r.size);
    case Modified:      return QDateTime::fromSecsSinceEpoch(r.mtime).toString(QStringLiteral("yyyy-MM-dd hh:mm"));
    case FirstSnapshot: return r.firstSnap;
    case LastSnapshot:  return r.lastSnap;
    default:            return {};
    }
}

QVariant SearchModel::headerData(int section, Qt::Orientation orientation, int role) const
{
    if (orientation != Qt::Horizontal || role != Qt::DisplayRole)
        return {};

    switch (section) {
    case Path:          return QStringLiteral("Path");
    case Name:          return QStringLiteral("Name");
    case Size:          return QStringLiteral("Size");
    case Modified:      return QStringLiteral("Modified");
    case FirstSnapshot: return QStringLiteral("First Snapshot");
    case LastSnapshot:  return QStringLiteral("Last Snapshot");
    default:            return {};
    }
}
```

**Step 4: Add to CMakeLists.txt and build/test**

Add `src/searchmodel.cpp` to main target. Add test:
```cmake
ecm_add_test(tests/searchmodeltest.cpp src/searchmodel.cpp src/filemodel.cpp src/database.cpp
    TEST_NAME searchmodeltest
    LINK_LIBRARIES Qt6::Test Qt6::Sql
)
```

```bash
cmake --build build
cd build/gui && ctest --test-dir . --output-on-failure
```
Expected: All tests PASS.

**Step 5: Commit**

```bash
git add gui/src/searchmodel.h gui/src/searchmodel.cpp gui/tests/searchmodeltest.cpp gui/CMakeLists.txt
git commit -m "feat(gui): search model with FTS5 query execution"
```

---

### Task 6: Custom snapshot timeline widget

**Files:**
- Create: `gui/src/snapshottimeline.h`
- Create: `gui/src/snapshottimeline.cpp`
- Modify: `gui/CMakeLists.txt`

**Context:** This is the custom-painted timeline widget (the signature visual element). It takes a SnapshotModel and renders date groups as rounded pills with snapshot nodes below each. Click handling emits `snapshotSelected(qint64 id)`. This task focuses on the widget itself — wiring it into MainWindow is Task 8.

**Step 1: Create `gui/src/snapshottimeline.h`**

```cpp
#pragma once

#include <QWidget>
#include <QVector>
#include "snapshotmodel.h"

class SnapshotTimeline : public QWidget
{
    Q_OBJECT

public:
    explicit SnapshotTimeline(SnapshotModel *model, QWidget *parent = nullptr);

    void setModel(SnapshotModel *model);

Q_SIGNALS:
    void snapshotSelected(qint64 snapshotId);

protected:
    void paintEvent(QPaintEvent *event) override;
    void mousePressEvent(QMouseEvent *event) override;
    QSize sizeHint() const override;
    QSize minimumSizeHint() const override;

private:
    struct HitRect {
        QRect rect;
        qint64 snapshotId = -1;
        bool isDateGroup = false;
    };

    SnapshotModel *m_model = nullptr;
    qint64 m_selectedId = -1;
    QVector<HitRect> m_hitRects;

    void recalculate();

    static constexpr int TimelineX = 20;
    static constexpr int NodeRadius = 5;
    static constexpr int DatePillHeight = 28;
    static constexpr int SnapRowHeight = 24;
    static constexpr int DateGap = 16;
    static constexpr int LeftPadding = 12;
    static constexpr int TopPadding = 12;
};
```

**Step 2: Create `gui/src/snapshottimeline.cpp`**

```cpp
#include "snapshottimeline.h"

#include <QPainter>
#include <QPalette>
#include <QMouseEvent>

SnapshotTimeline::SnapshotTimeline(SnapshotModel *model, QWidget *parent)
    : QWidget(parent)
    , m_model(model)
{
    setMouseTracking(true);
    setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Expanding);

    if (m_model) {
        connect(m_model, &QAbstractItemModel::modelReset, this, [this]() {
            recalculate();
            update();
        });
    }
}

void SnapshotTimeline::setModel(SnapshotModel *model)
{
    m_model = model;
    if (m_model) {
        connect(m_model, &QAbstractItemModel::modelReset, this, [this]() {
            recalculate();
            update();
        });
    }
    recalculate();
    update();
}

void SnapshotTimeline::recalculate()
{
    m_hitRects.clear();
    if (!m_model) return;

    int y = TopPadding;
    int numGroups = m_model->rowCount();

    for (int g = 0; g < numGroups; ++g) {
        auto groupIdx = m_model->index(g, 0);

        QRect pillRect(LeftPadding, y, width() - LeftPadding * 2, DatePillHeight);
        m_hitRects.append({.rect = pillRect, .snapshotId = -1, .isDateGroup = true});
        y += DatePillHeight + 4;

        int childCount = m_model->rowCount(groupIdx);
        for (int c = 0; c < childCount; ++c) {
            auto childIdx = m_model->index(c, 0, groupIdx);
            qint64 snapId = m_model->data(childIdx, SnapshotModel::SnapshotIdRole).toLongLong();

            QRect nodeRect(LeftPadding, y, width() - LeftPadding * 2, SnapRowHeight);
            m_hitRects.append({.rect = nodeRect, .snapshotId = snapId, .isDateGroup = false});
            y += SnapRowHeight;
        }

        y += DateGap;
    }

    setMinimumHeight(y);
}

void SnapshotTimeline::paintEvent(QPaintEvent * /*event*/)
{
    QPainter painter(this);
    painter.setRenderHint(QPainter::Antialiasing);

    const auto &pal = palette();
    QColor accentColor = pal.color(QPalette::Highlight);
    QColor textColor = pal.color(QPalette::WindowText);
    QColor surfaceColor = pal.color(QPalette::AlternateBase);
    QColor selectedBg = pal.color(QPalette::Highlight).lighter(180);

    if (!m_model) return;

    int y = TopPadding;
    int lineX = LeftPadding + TimelineX;
    int numGroups = m_model->rowCount();

    for (int g = 0; g < numGroups; ++g) {
        auto groupIdx = m_model->index(g, 0);
        QString dateLabel = m_model->data(groupIdx, Qt::DisplayRole).toString();

        // Date pill
        QRect pillRect(LeftPadding, y, width() - LeftPadding * 2, DatePillHeight);
        painter.setBrush(surfaceColor);
        painter.setPen(Qt::NoPen);
        painter.drawRoundedRect(pillRect, 6, 6);

        // Date pill circle
        painter.setBrush(accentColor);
        painter.drawEllipse(QPoint(lineX, y + DatePillHeight / 2), NodeRadius + 2, NodeRadius + 2);

        // Date text
        painter.setPen(textColor);
        QFont boldFont = font();
        boldFont.setBold(true);
        painter.setFont(boldFont);
        painter.drawText(pillRect.adjusted(TimelineX + NodeRadius + 12, 0, 0, 0),
                         Qt::AlignVCenter, dateLabel);
        painter.setFont(font());
        y += DatePillHeight + 4;

        // Timeline line segment for children
        int childCount = m_model->rowCount(groupIdx);
        if (childCount > 0) {
            int lineTop = y;
            int lineBottom = y + childCount * SnapRowHeight - SnapRowHeight / 2;
            painter.setPen(QPen(accentColor, 2));
            painter.drawLine(lineX, lineTop, lineX, lineBottom);
        }

        // Snapshot nodes
        for (int c = 0; c < childCount; ++c) {
            auto childIdx = m_model->index(c, 0, groupIdx);
            qint64 snapId = m_model->data(childIdx, SnapshotModel::SnapshotIdRole).toLongLong();
            QString label = m_model->data(childIdx, Qt::DisplayRole).toString();

            QRect rowRect(LeftPadding, y, width() - LeftPadding * 2, SnapRowHeight);

            if (snapId == m_selectedId) {
                painter.fillRect(rowRect, selectedBg);
            }

            // Branch connector
            painter.setPen(QPen(accentColor, 1));
            int nodeY = y + SnapRowHeight / 2;
            painter.drawLine(lineX, nodeY, lineX + 10, nodeY);

            // Node circle
            bool selected = (snapId == m_selectedId);
            painter.setBrush(selected ? accentColor : pal.color(QPalette::Window));
            painter.setPen(QPen(accentColor, 2));
            painter.drawEllipse(QPoint(lineX + 14, nodeY), NodeRadius, NodeRadius);

            // Label
            painter.setPen(textColor);
            painter.drawText(QRect(lineX + 24, y, width() - lineX - 30, SnapRowHeight),
                             Qt::AlignVCenter, label);

            y += SnapRowHeight;
        }

        y += DateGap;
    }
}

void SnapshotTimeline::mousePressEvent(QMouseEvent *event)
{
    for (const auto &hit : m_hitRects) {
        if (hit.rect.contains(event->pos()) && !hit.isDateGroup && hit.snapshotId >= 0) {
            m_selectedId = hit.snapshotId;
            update();
            Q_EMIT snapshotSelected(hit.snapshotId);
            return;
        }
    }
    QWidget::mousePressEvent(event);
}

QSize SnapshotTimeline::sizeHint() const
{
    return {220, 400};
}

QSize SnapshotTimeline::minimumSizeHint() const
{
    return {180, 200};
}
```

**Step 3: Add to CMakeLists.txt**

Add `src/snapshottimeline.cpp` to `target_sources(btrdasd-gui PRIVATE`.

**Step 4: Build and verify**

```bash
cmake --build build --target btrdasd-gui
```
Expected: Compiles without errors. (Visual testing happens in Task 8 when wired into MainWindow.)

**Step 5: Commit**

```bash
git add gui/src/snapshottimeline.h gui/src/snapshottimeline.cpp gui/CMakeLists.txt
git commit -m "feat(gui): custom-painted snapshot timeline widget"
```

---

### Task 7: Index runner (QProcess wrapper for btrdasd)

**Files:**
- Create: `gui/src/indexrunner.h`
- Create: `gui/src/indexrunner.cpp`
- Modify: `gui/CMakeLists.txt`

**Context:** IndexRunner wraps `QProcess` to spawn `btrdasd walk <target>`. It emits progress signals as stdout lines arrive and a `finished` signal when done. This enables the re-index button and auto-watch trigger.

**Step 1: Create `gui/src/indexrunner.h`**

```cpp
#pragma once

#include <QObject>
#include <QProcess>
#include <QString>

class IndexRunner : public QObject
{
    Q_OBJECT

public:
    explicit IndexRunner(QObject *parent = nullptr);

    void run(const QString &targetPath, const QString &dbPath);
    void abort();
    [[nodiscard]] bool isRunning() const;

Q_SIGNALS:
    void outputLine(const QString &line);
    void finished(bool success, const QString &errorMessage);

private Q_SLOTS:
    void onReadyReadStdout();
    void onProcessFinished(int exitCode, QProcess::ExitStatus exitStatus);

private:
    QProcess *m_process = nullptr;
    QString m_binaryPath;

    static QString findBtrdasd();
};
```

**Step 2: Create `gui/src/indexrunner.cpp`**

```cpp
#include "indexrunner.h"

#include <QStandardPaths>

IndexRunner::IndexRunner(QObject *parent)
    : QObject(parent)
    , m_binaryPath(findBtrdasd())
{
}

QString IndexRunner::findBtrdasd()
{
    QByteArray envPath = qgetenv("BTRDASD_BIN");
    if (!envPath.isEmpty()) {
        return QString::fromLocal8Bit(envPath);
    }

    QString path = QStandardPaths::findExecutable(QStringLiteral("btrdasd"));
    if (!path.isEmpty()) return path;

    return QStringLiteral("/usr/local/bin/btrdasd");
}

void IndexRunner::run(const QString &targetPath, const QString &dbPath)
{
    if (m_process && m_process->state() != QProcess::NotRunning) {
        return;
    }

    m_process = new QProcess(this);

    connect(m_process, &QProcess::readyReadStandardOutput,
            this, &IndexRunner::onReadyReadStdout);
    connect(m_process, &QProcess::finished,
            this, &IndexRunner::onProcessFinished);

    QStringList args;
    args << QStringLiteral("walk") << targetPath;
    if (!dbPath.isEmpty()) {
        args << QStringLiteral("--db") << dbPath;
    }

    m_process->start(m_binaryPath, args);
}

void IndexRunner::abort()
{
    if (m_process && m_process->state() != QProcess::NotRunning) {
        m_process->terminate();
        if (!m_process->waitForFinished(3000)) {
            m_process->kill();
        }
    }
}

bool IndexRunner::isRunning() const
{
    return m_process && m_process->state() != QProcess::NotRunning;
}

void IndexRunner::onReadyReadStdout()
{
    while (m_process->canReadLine()) {
        QString line = QString::fromUtf8(m_process->readLine()).trimmed();
        if (!line.isEmpty()) {
            Q_EMIT outputLine(line);
        }
    }
}

void IndexRunner::onProcessFinished(int exitCode, QProcess::ExitStatus exitStatus)
{
    QString errorMsg;
    if (exitStatus == QProcess::CrashExit) {
        errorMsg = QStringLiteral("btrdasd process crashed");
    } else if (exitCode != 0) {
        errorMsg = QStringLiteral("btrdasd exited with code ") + QString::number(exitCode);
        QString stderrOutput = QString::fromUtf8(m_process->readAllStandardError()).trimmed();
        if (!stderrOutput.isEmpty()) {
            errorMsg += QStringLiteral(": ") + stderrOutput;
        }
    }

    Q_EMIT finished(exitCode == 0 && exitStatus == QProcess::NormalExit, errorMsg);
    m_process->deleteLater();
    m_process = nullptr;
}
```

**Step 3: Add to CMakeLists.txt**

Add `src/indexrunner.cpp` to main target sources.

**Step 4: Build**

```bash
cmake --build build --target btrdasd-gui
```
Expected: Compiles without errors.

**Step 5: Commit**

```bash
git add gui/src/indexrunner.h gui/src/indexrunner.cpp gui/CMakeLists.txt
git commit -m "feat(gui): index runner QProcess wrapper for btrdasd walk"
```

---

### Task 8: Wire up MainWindow with all components

**Files:**
- Modify: `gui/src/mainwindow.h`
- Modify: `gui/src/mainwindow.cpp`
- Modify: `gui/src/main.cpp` (add `--db` CLI argument)
- Modify: `gui/src/btrdasd-gui.rc` (add actions)
- Modify: `gui/CMakeLists.txt` (ensure all sources listed)

**Context:** This is the integration task: connect the timeline, file list, search, and toolbar into the main window. After this task, the application is usable.

**Step 1: Update `gui/src/main.cpp`** — add `--db` argument

Replace the parser and window creation section with:
```cpp
    QCommandLineParser parser;
    QCommandLineOption dbOption(
        QStringLiteral("db"),
        i18n("Path to SQLite database"),
        QStringLiteral("path"),
        QStringLiteral("/var/lib/das-backup/backup-index.db"));
    parser.addOption(dbOption);
    aboutData.setupCommandLine(&parser);
    parser.process(app);
    aboutData.processCommandLine(&parser);

    QString dbPath = parser.value(dbOption);

    auto *window = new MainWindow(dbPath);
    window->show();
```

**Step 2: Rewrite `gui/src/mainwindow.h`**

```cpp
#pragma once

#include <KXmlGuiWindow>

class QSplitter;
class QTableView;
class QLineEdit;
class QSortFilterProxyModel;
class QLabel;
class QTimer;

class Database;
class SnapshotModel;
class SnapshotTimeline;
class FileModel;
class SearchModel;
class IndexRunner;

class MainWindow : public KXmlGuiWindow
{
    Q_OBJECT

public:
    explicit MainWindow(const QString &dbPath, QWidget *parent = nullptr);
    ~MainWindow() override;

private Q_SLOTS:
    void onSnapshotSelected(qint64 snapshotId);
    void onSearchTextChanged(const QString &text);
    void executeSearch();
    void triggerReindex();
    void showStats();
    void updateStatusBar();

private:
    void setupActions();
    void setupUi();
    void openDatabase(const QString &path);

    Database *m_database = nullptr;
    SnapshotModel *m_snapshotModel = nullptr;
    SnapshotTimeline *m_timeline = nullptr;
    FileModel *m_fileModel = nullptr;
    SearchModel *m_searchModel = nullptr;
    IndexRunner *m_indexRunner = nullptr;

    QTableView *m_fileView = nullptr;
    QTableView *m_searchView = nullptr;
    QLineEdit *m_searchBar = nullptr;
    QTimer *m_searchTimer = nullptr;
    QSortFilterProxyModel *m_fileProxy = nullptr;
    QLabel *m_statusLabel = nullptr;

    QString m_dbPath;
};
```

**Step 3: Rewrite `gui/src/mainwindow.cpp`**

```cpp
#include "mainwindow.h"
#include "database.h"
#include "snapshotmodel.h"
#include "snapshottimeline.h"
#include "filemodel.h"
#include "searchmodel.h"
#include "indexrunner.h"

#include <KActionCollection>
#include <KLocalizedString>
#include <KMessageBox>

#include <QAction>
#include <QHeaderView>
#include <QLabel>
#include <QLineEdit>
#include <QScrollArea>
#include <QSortFilterProxyModel>
#include <QSplitter>
#include <QStatusBar>
#include <QTableView>
#include <QTimer>
#include <QVBoxLayout>

MainWindow::MainWindow(const QString &dbPath, QWidget *parent)
    : KXmlGuiWindow(parent)
    , m_dbPath(dbPath)
{
    m_database = new Database();
    m_indexRunner = new IndexRunner(this);

    setupUi();
    setupActions();
    setupGUI(Default, QStringLiteral("btrdasd-gui.rc"));

    openDatabase(m_dbPath);
}

MainWindow::~MainWindow()
{
    delete m_database;
}

void MainWindow::setupUi()
{
    m_snapshotModel = new SnapshotModel(m_database, this);
    m_fileModel = new FileModel(m_database, this);
    m_searchModel = new SearchModel(m_database, this);

    // Search bar with debounce
    m_searchBar = new QLineEdit(this);
    m_searchBar->setPlaceholderText(i18n("Search files (FTS5)..."));
    m_searchBar->setClearButtonEnabled(true);

    m_searchTimer = new QTimer(this);
    m_searchTimer->setSingleShot(true);
    m_searchTimer->setInterval(300);
    connect(m_searchBar, &QLineEdit::textChanged, this, &MainWindow::onSearchTextChanged);
    connect(m_searchTimer, &QTimer::timeout, this, &MainWindow::executeSearch);

    // Timeline in scroll area
    m_timeline = new SnapshotTimeline(m_snapshotModel, this);
    connect(m_timeline, &SnapshotTimeline::snapshotSelected, this, &MainWindow::onSnapshotSelected);

    auto *scrollArea = new QScrollArea(this);
    scrollArea->setWidget(m_timeline);
    scrollArea->setWidgetResizable(true);
    scrollArea->setMinimumWidth(200);

    // File list
    m_fileProxy = new QSortFilterProxyModel(this);
    m_fileProxy->setSourceModel(m_fileModel);
    m_fileView = new QTableView(this);
    m_fileView->setModel(m_fileProxy);
    m_fileView->setSortingEnabled(true);
    m_fileView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_fileView->setAlternatingRowColors(true);
    m_fileView->horizontalHeader()->setStretchLastSection(true);

    // Search results
    m_searchView = new QTableView(this);
    m_searchView->setModel(m_searchModel);
    m_searchView->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_searchView->setAlternatingRowColors(true);
    m_searchView->horizontalHeader()->setStretchLastSection(true);
    m_searchView->setVisible(false);

    // Right splitter
    auto *rightSplitter = new QSplitter(Qt::Vertical, this);
    rightSplitter->addWidget(m_fileView);
    rightSplitter->addWidget(m_searchView);
    rightSplitter->setStretchFactor(0, 3);
    rightSplitter->setStretchFactor(1, 1);

    // Main splitter
    auto *mainSplitter = new QSplitter(Qt::Horizontal, this);
    mainSplitter->addWidget(scrollArea);
    mainSplitter->addWidget(rightSplitter);
    mainSplitter->setStretchFactor(0, 1);
    mainSplitter->setStretchFactor(1, 3);

    // Central layout
    auto *centralWidget = new QWidget(this);
    auto *layout = new QVBoxLayout(centralWidget);
    layout->setContentsMargins(4, 4, 4, 4);
    layout->addWidget(m_searchBar);
    layout->addWidget(mainSplitter, 1);
    setCentralWidget(centralWidget);

    // Status bar
    m_statusLabel = new QLabel(this);
    statusBar()->addPermanentWidget(m_statusLabel);

    // Index runner signals
    connect(m_indexRunner, &IndexRunner::finished, this, [this](bool success, const QString &error) {
        if (success) {
            m_snapshotModel->reload();
            updateStatusBar();
            statusBar()->showMessage(i18n("Re-indexing complete"), 5000);
        } else {
            KMessageBox::error(this, i18n("Indexing failed: %1", error));
        }
    });
}

void MainWindow::setupActions()
{
    auto *reindexAction = new QAction(QIcon::fromTheme(QStringLiteral("view-refresh")),
                                       i18n("Re-index"), this);
    reindexAction->setToolTip(i18n("Run btrdasd walk to index new snapshots"));
    actionCollection()->addAction(QStringLiteral("reindex"), reindexAction);
    connect(reindexAction, &QAction::triggered, this, &MainWindow::triggerReindex);

    auto *statsAction = new QAction(QIcon::fromTheme(QStringLiteral("document-properties")),
                                     i18n("Statistics"), this);
    statsAction->setToolTip(i18n("Show database statistics"));
    actionCollection()->addAction(QStringLiteral("stats"), statsAction);
    connect(statsAction, &QAction::triggered, this, &MainWindow::showStats);
}

void MainWindow::openDatabase(const QString &path)
{
    if (!m_database->open(path)) {
        KMessageBox::error(this, i18n("Failed to open database: %1", path));
        return;
    }

    m_snapshotModel->reload();
    updateStatusBar();
}

void MainWindow::onSnapshotSelected(qint64 snapshotId)
{
    m_fileModel->loadSnapshot(snapshotId);
}

void MainWindow::onSearchTextChanged(const QString &text)
{
    m_searchTimer->stop();
    if (text.trimmed().isEmpty()) {
        m_searchModel->clear();
        m_searchView->setVisible(false);
        return;
    }
    m_searchTimer->start();
}

void MainWindow::executeSearch()
{
    QString query = m_searchBar->text().trimmed();
    if (query.isEmpty()) return;

    m_searchModel->executeSearch(query, 50);
    m_searchView->setVisible(true);
    statusBar()->showMessage(i18n("%1 results", m_searchModel->rowCount()), 3000);
}

void MainWindow::triggerReindex()
{
    if (m_indexRunner->isRunning()) {
        KMessageBox::information(this, i18n("Indexing is already running."));
        return;
    }

    auto snapshots = m_database->listSnapshots();
    QString targetPath = QStringLiteral("/mnt/backup-hdd");
    if (!snapshots.isEmpty()) {
        QString snapPath = snapshots.first().path;
        int lastSlash = snapPath.lastIndexOf(QLatin1Char('/'));
        if (lastSlash > 0) {
            int secondLastSlash = snapPath.lastIndexOf(QLatin1Char('/'), lastSlash - 1);
            if (secondLastSlash > 0) {
                targetPath = snapPath.left(secondLastSlash);
            }
        }
    }

    statusBar()->showMessage(i18n("Re-indexing %1...", targetPath));
    m_indexRunner->run(targetPath, m_dbPath);
}

void MainWindow::showStats()
{
    auto stats = m_database->stats();
    KMessageBox::information(this, i18n(
        "Snapshots: %1\nFiles: %2\nSpans: %3\nDatabase size: %4 bytes",
        stats.snapshotCount, stats.fileCount, stats.spanCount, stats.dbSizeBytes));
}

void MainWindow::updateStatusBar()
{
    auto stats = m_database->stats();
    m_statusLabel->setText(i18n("%1 snapshots | %2 files | DB: %3",
        stats.snapshotCount, stats.fileCount,
        FileModel::formatSize(stats.dbSizeBytes)));
}
```

**Step 4: Update `gui/src/btrdasd-gui.rc`**

```xml
<?xml version="1.0" encoding="UTF-8"?>
<gui name="btrdasd-gui"
     version="2"
     xmlns="http://www.kde.org/standards/kxmlgui/1.0"
     xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
     xsi:schemaLocation="http://www.kde.org/standards/kxmlgui/1.0
                          http://www.kde.org/standards/kxmlgui/1.0/kxmlgui.xsd" >
  <MenuBar>
    <Menu name="file" >
      <text>&amp;File</text>
      <Action name="reindex" />
      <Action name="stats" />
    </Menu>
  </MenuBar>
  <ToolBar name="mainToolBar" >
    <text>Main Toolbar</text>
    <Action name="reindex" />
    <Action name="stats" />
  </ToolBar>
</gui>
```

**Step 5: Ensure all sources are in CMakeLists.txt**

```cmake
target_sources(btrdasd-gui PRIVATE
    src/main.cpp
    src/mainwindow.cpp
    src/database.cpp
    src/snapshotmodel.cpp
    src/snapshottimeline.cpp
    src/filemodel.cpp
    src/searchmodel.cpp
    src/indexrunner.cpp
)
```

**Step 6: Build and manual test**

```bash
cmake --build build --target btrdasd-gui
./build/gui/btrdasd-gui --db /var/lib/das-backup/backup-index.db
```
Expected: Window opens with timeline (left), file list (right), search bar (top), toolbar with Re-index and Statistics buttons, status bar showing stats.

**Step 7: Commit**

```bash
git add gui/src/ gui/CMakeLists.txt
git commit -m "feat(gui): wire up main window with timeline, file list, search, and toolbar"
```

---

### Task 9: Snapshot watcher (auto-detect new snapshots)

**Files:**
- Create: `gui/src/snapshotwatcher.h`
- Create: `gui/src/snapshotwatcher.cpp`
- Modify: `gui/src/mainwindow.h` (add watcher member)
- Modify: `gui/src/mainwindow.cpp` (wire watcher)
- Modify: `gui/CMakeLists.txt`

**Context:** SnapshotWatcher uses QFileSystemWatcher to monitor the backup target directory. When a new subdirectory appears (new snapshot), it waits 30 seconds then triggers IndexRunner.

**Step 1: Create `gui/src/snapshotwatcher.h`**

```cpp
#pragma once

#include <QObject>
#include <QFileSystemWatcher>
#include <QTimer>

class IndexRunner;

class SnapshotWatcher : public QObject
{
    Q_OBJECT

public:
    explicit SnapshotWatcher(IndexRunner *runner, QObject *parent = nullptr);

    void setWatchPath(const QString &path);
    void setDbPath(const QString &dbPath);
    void setEnabled(bool enabled);
    [[nodiscard]] bool isEnabled() const;

Q_SIGNALS:
    void newSnapshotDetected(const QString &path);
    void indexingTriggered();

private Q_SLOTS:
    void onDirectoryChanged(const QString &path);
    void triggerIndex();

private:
    QFileSystemWatcher *m_watcher = nullptr;
    IndexRunner *m_runner = nullptr;
    QTimer *m_delayTimer = nullptr;
    QString m_watchPath;
    QString m_dbPath;
    bool m_enabled = false;

    static constexpr int IndexDelayMs = 30000;
};
```

**Step 2: Create `gui/src/snapshotwatcher.cpp`**

```cpp
#include "snapshotwatcher.h"
#include "indexrunner.h"

#include <QDir>

SnapshotWatcher::SnapshotWatcher(IndexRunner *runner, QObject *parent)
    : QObject(parent)
    , m_runner(runner)
{
    m_watcher = new QFileSystemWatcher(this);
    m_delayTimer = new QTimer(this);
    m_delayTimer->setSingleShot(true);
    m_delayTimer->setInterval(IndexDelayMs);

    connect(m_watcher, &QFileSystemWatcher::directoryChanged,
            this, &SnapshotWatcher::onDirectoryChanged);
    connect(m_delayTimer, &QTimer::timeout,
            this, &SnapshotWatcher::triggerIndex);
}

void SnapshotWatcher::setWatchPath(const QString &path)
{
    if (!m_watchPath.isEmpty()) {
        m_watcher->removePath(m_watchPath);
        QDir dir(m_watchPath);
        for (const auto &entry : dir.entryList(QDir::Dirs | QDir::NoDotAndDotDot)) {
            m_watcher->removePath(dir.filePath(entry));
        }
    }

    m_watchPath = path;

    if (m_enabled && !path.isEmpty()) {
        m_watcher->addPath(path);
        QDir dir(path);
        for (const auto &entry : dir.entryList(QDir::Dirs | QDir::NoDotAndDotDot)) {
            m_watcher->addPath(dir.filePath(entry));
        }
    }
}

void SnapshotWatcher::setDbPath(const QString &dbPath)
{
    m_dbPath = dbPath;
}

void SnapshotWatcher::setEnabled(bool enabled)
{
    m_enabled = enabled;
    if (enabled && !m_watchPath.isEmpty()) {
        setWatchPath(m_watchPath);
    } else if (!enabled) {
        m_watcher->removePaths(m_watcher->directories());
        m_delayTimer->stop();
    }
}

bool SnapshotWatcher::isEnabled() const
{
    return m_enabled;
}

void SnapshotWatcher::onDirectoryChanged(const QString &path)
{
    Q_EMIT newSnapshotDetected(path);
    m_delayTimer->start();
}

void SnapshotWatcher::triggerIndex()
{
    if (m_runner->isRunning()) return;
    Q_EMIT indexingTriggered();
    m_runner->run(m_watchPath, m_dbPath);
}
```

**Step 3: Wire into MainWindow**

In `mainwindow.h`, add forward declaration and member:
```cpp
class SnapshotWatcher;
// In private:
SnapshotWatcher *m_watcher = nullptr;
```

In `mainwindow.cpp` `setupUi()`, after IndexRunner setup add:
```cpp
    m_watcher = new SnapshotWatcher(m_indexRunner, this);
    m_watcher->setDbPath(m_dbPath);
    connect(m_watcher, &SnapshotWatcher::indexingTriggered, this, [this]() {
        statusBar()->showMessage(i18n("Auto-indexing new snapshots..."));
    });
```

**Step 4: Add to CMakeLists.txt, build**

Add `src/snapshotwatcher.cpp` to target sources.

```bash
cmake --build build --target btrdasd-gui
```

**Step 5: Commit**

```bash
git add gui/src/snapshotwatcher.h gui/src/snapshotwatcher.cpp gui/src/mainwindow.h gui/src/mainwindow.cpp gui/CMakeLists.txt
git commit -m "feat(gui): snapshot watcher with QFileSystemWatcher auto-detection"
```

---

### Task 10: Restore action (KIO file copy)

**Files:**
- Create: `gui/src/restoreaction.h`
- Create: `gui/src/restoreaction.cpp`
- Modify: `gui/src/mainwindow.h` (add restore slot)
- Modify: `gui/src/mainwindow.cpp` (add restore action, context menu)
- Modify: `gui/CMakeLists.txt`

**Context:** RestoreAction uses KIO::copy() to copy a file from a snapshot path to a user-selected destination. Also provides "Copy path" clipboard action.

**Step 1: Create `gui/src/restoreaction.h`**

```cpp
#pragma once

#include <QObject>
#include <QString>

class QWidget;

class RestoreAction : public QObject
{
    Q_OBJECT

public:
    explicit RestoreAction(QObject *parent = nullptr);

    void restoreFile(const QString &snapshotPath, const QString &relativePath,
                     QWidget *parentWidget);
    static void copyPathToClipboard(const QString &fullPath);

Q_SIGNALS:
    void restoreComplete(const QString &destPath);
    void restoreFailed(const QString &errorMessage);
};
```

**Step 2: Create `gui/src/restoreaction.cpp`**

```cpp
#include "restoreaction.h"

#include <QClipboard>
#include <QDir>
#include <QFile>
#include <QFileDialog>
#include <QGuiApplication>
#include <QUrl>

#include <KIO/CopyJob>
#include <KJobWidgets>

RestoreAction::RestoreAction(QObject *parent)
    : QObject(parent)
{
}

void RestoreAction::restoreFile(const QString &snapshotPath, const QString &relativePath,
                                 QWidget *parentWidget)
{
    QString sourcePath = snapshotPath + QLatin1Char('/') + relativePath;
    QUrl sourceUrl = QUrl::fromLocalFile(sourcePath);

    if (!QFile::exists(sourcePath)) {
        Q_EMIT restoreFailed(QStringLiteral("Source file not found (snapshot may not be mounted): ") + sourcePath);
        return;
    }

    QString destDir = QFileDialog::getExistingDirectory(
        parentWidget,
        QStringLiteral("Restore to..."),
        QDir::homePath());

    if (destDir.isEmpty()) return;

    QUrl destUrl = QUrl::fromLocalFile(destDir);
    auto *job = KIO::copy(sourceUrl, destUrl, KIO::DefaultFlags);
    KJobWidgets::setWindow(job, parentWidget);

    connect(job, &KJob::result, this, [this, destDir](KJob *j) {
        if (j->error()) {
            Q_EMIT restoreFailed(j->errorString());
        } else {
            Q_EMIT restoreComplete(destDir);
        }
    });
}

void RestoreAction::copyPathToClipboard(const QString &fullPath)
{
    QGuiApplication::clipboard()->setText(fullPath);
}
```

**Step 3: Wire into MainWindow**

In `mainwindow.h`, add:
```cpp
class RestoreAction;
// In private:
RestoreAction *m_restoreAction = nullptr;
// In private slots:
void restoreSelectedFile();
void copySelectedPath();
```

In `mainwindow.cpp`, in `setupUi()`:
```cpp
    m_restoreAction = new RestoreAction(this);
    connect(m_restoreAction, &RestoreAction::restoreComplete, this, [this](const QString &dest) {
        statusBar()->showMessage(i18n("File restored to %1", dest), 5000);
    });
    connect(m_restoreAction, &RestoreAction::restoreFailed, this, [this](const QString &err) {
        KMessageBox::error(this, i18n("Restore failed: %1", err));
    });
```

In `setupActions()`, add restore and copy-path actions:
```cpp
    auto *restoreFileAction = new QAction(QIcon::fromTheme(QStringLiteral("document-save")),
                                       i18n("Restore to..."), this);
    restoreFileAction->setToolTip(i18n("Restore selected file to a chosen directory"));
    actionCollection()->addAction(QStringLiteral("restore"), restoreFileAction);
    connect(restoreFileAction, &QAction::triggered, this, &MainWindow::restoreSelectedFile);

    auto *copyPathAction = new QAction(QIcon::fromTheme(QStringLiteral("edit-copy")),
                                        i18n("Copy Path"), this);
    copyPathAction->setToolTip(i18n("Copy the full snapshot path to clipboard"));
    actionCollection()->addAction(QStringLiteral("copypath"), copyPathAction);
    connect(copyPathAction, &QAction::triggered, this, &MainWindow::copySelectedPath);
```

Implement the slots (reference selected snapshot and file from models — the implementer should look at the current m_fileView selection and selected snapshot context to build the full path).

**Step 4: Add to CMakeLists.txt, build**

Add `src/restoreaction.cpp`. Build.

**Step 5: Commit**

```bash
git add gui/src/restoreaction.h gui/src/restoreaction.cpp gui/src/mainwindow.h gui/src/mainwindow.cpp gui/CMakeLists.txt
git commit -m "feat(gui): restore action with KIO file copy and clipboard support"
```

---

### Task 11: Settings dialog (KConfigDialog)

**Files:**
- Create: `gui/src/settingsdialog.h`
- Create: `gui/src/settingsdialog.cpp`
- Modify: `gui/src/mainwindow.cpp` (add settings action)
- Modify: `gui/CMakeLists.txt`

**Context:** Settings for database path, backup target path, auto-watch toggle, default restore destination. Uses KConfigDialog.

**Step 1: Create `gui/src/settingsdialog.h`**

```cpp
#pragma once

#include <KConfigDialog>

class QCheckBox;
class KUrlRequester;

class SettingsDialog : public KConfigDialog
{
    Q_OBJECT

public:
    explicit SettingsDialog(QWidget *parent = nullptr);

    [[nodiscard]] QString databasePath() const;
    [[nodiscard]] QString backupTargetPath() const;
    [[nodiscard]] bool autoWatchEnabled() const;
    [[nodiscard]] QString defaultRestorePath() const;

private:
    KUrlRequester *m_dbPathEdit = nullptr;
    KUrlRequester *m_targetPathEdit = nullptr;
    QCheckBox *m_autoWatchCheck = nullptr;
    KUrlRequester *m_restorePathEdit = nullptr;
};
```

**Step 2: Create `gui/src/settingsdialog.cpp`**

```cpp
#include "settingsdialog.h"

#include <QCheckBox>
#include <QDir>
#include <QFormLayout>
#include <QWidget>

#include <KFile>
#include <KLocalizedString>
#include <KUrlRequester>

SettingsDialog::SettingsDialog(QWidget *parent)
    : KConfigDialog(parent, QStringLiteral("settings"), nullptr)
{
    auto *page = new QWidget(this);
    auto *layout = new QFormLayout(page);

    m_dbPathEdit = new KUrlRequester(page);
    m_dbPathEdit->setMode(KFile::File | KFile::LocalOnly);
    m_dbPathEdit->setText(QStringLiteral("/var/lib/das-backup/backup-index.db"));
    layout->addRow(i18n("Database path:"), m_dbPathEdit);

    m_targetPathEdit = new KUrlRequester(page);
    m_targetPathEdit->setMode(KFile::Directory | KFile::LocalOnly);
    m_targetPathEdit->setText(QStringLiteral("/mnt/backup-hdd"));
    layout->addRow(i18n("Backup target:"), m_targetPathEdit);

    m_autoWatchCheck = new QCheckBox(i18n("Auto-detect new snapshots"), page);
    layout->addRow(QString(), m_autoWatchCheck);

    m_restorePathEdit = new KUrlRequester(page);
    m_restorePathEdit->setMode(KFile::Directory | KFile::LocalOnly);
    m_restorePathEdit->setText(QDir::homePath());
    layout->addRow(i18n("Default restore to:"), m_restorePathEdit);

    addPage(page, i18n("General"), QStringLiteral("preferences-system"));
}

QString SettingsDialog::databasePath() const { return m_dbPathEdit->text(); }
QString SettingsDialog::backupTargetPath() const { return m_targetPathEdit->text(); }
bool SettingsDialog::autoWatchEnabled() const { return m_autoWatchCheck->isChecked(); }
QString SettingsDialog::defaultRestorePath() const { return m_restorePathEdit->text(); }
```

**Step 3: Wire into MainWindow**

In `mainwindow.cpp`, add to `setupActions()`:
```cpp
    auto *settingsAction = KStandardAction::preferences(this, [this]() {
        auto *dlg = new SettingsDialog(this);
        dlg->show();
    }, actionCollection());
```

(Requires `#include <KStandardAction>` at the top of mainwindow.cpp.)

**Step 4: Add to CMakeLists.txt, build**

Add `src/settingsdialog.cpp`. Build.

**Step 5: Commit**

```bash
git add gui/src/settingsdialog.h gui/src/settingsdialog.cpp gui/src/mainwindow.cpp gui/CMakeLists.txt
git commit -m "feat(gui): settings dialog with KConfigDialog"
```

---

### Task 12: Desktop entry, final polish, and verification

**Files:**
- Create: `gui/org.theboscoclub.btrdasd-gui.desktop`
- Modify: `gui/CMakeLists.txt` (install desktop entry)

**Step 1: Create desktop entry**

File: `gui/org.theboscoclub.btrdasd-gui.desktop`
```ini
[Desktop Entry]
Type=Application
Name=ButteredDASD
GenericName=Backup Browser
Comment=Search, browse, and restore files from BTRFS backup snapshots
Exec=btrdasd-gui
Icon=drive-harddisk
Terminal=false
Categories=Qt;KDE;System;Utility;
Keywords=backup;btrfs;snapshot;restore;search;
```

**Step 2: Install desktop entry in CMakeLists.txt**

```cmake
install(FILES org.theboscoclub.btrdasd-gui.desktop DESTINATION ${KDE_INSTALL_APPDIR})
```

**Step 3: Full build and test**

```bash
cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager
cmake -B build -DCMAKE_BUILD_TYPE=RelWithDebInfo -DBUILD_TESTING=ON
cmake --build build
cd build/gui && ctest --test-dir . --output-on-failure
```
Expected: All QTest tests PASS. Binary compiles clean.

**Step 4: Visual verification**

```bash
./build/gui/btrdasd-gui --db /var/lib/das-backup/backup-index.db
```

Verify:
- Window opens with correct title "ButteredDASD"
- Timeline panel on the left (may be empty if no indexed data)
- File list on the right
- Search bar at the top
- Toolbar: Re-index, Statistics buttons
- Menu: File > Re-index, Statistics, Settings
- Help > About ButteredDASD shows correct version and license
- Status bar shows stats

**Step 5: Commit**

```bash
git add gui/ CMakeLists.txt
git commit -m "feat(gui): desktop entry and final polish for ButteredDASD GUI"
```

---

## Summary

| Task | Component | Files | Tests |
|------|-----------|-------|-------|
| 1 | CMake scaffold + empty window | 5 create, 1 modify | Build test |
| 2 | Database wrapper | 3 create, 1 modify | 6 QTest |
| 3 | Snapshot model | 3 create, 1 modify | 4 QTest |
| 4 | File model | 3 create, 1 modify | 4 QTest |
| 5 | Search model | 3 create, 1 modify | 4 QTest |
| 6 | Timeline widget | 2 create, 1 modify | Visual |
| 7 | Index runner | 2 create, 1 modify | Build test |
| 8 | MainWindow integration | 4 modify | Visual |
| 9 | Snapshot watcher | 2 create, 2 modify | Build test |
| 10 | Restore action | 2 create, 2 modify | Build test |
| 11 | Settings dialog | 2 create, 1 modify | Build test |
| 12 | Desktop entry + polish | 1 create, 1 modify | Full suite |
