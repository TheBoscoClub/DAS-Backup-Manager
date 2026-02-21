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
