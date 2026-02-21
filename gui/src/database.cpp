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
