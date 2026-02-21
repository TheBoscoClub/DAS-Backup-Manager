#pragma once

#include <QObject>
#include <QString>

class RestoreAction : public QObject
{
    Q_OBJECT

public:
    explicit RestoreAction(QObject *parent = nullptr);

    void restore(const QString &sourcePath, const QString &destinationDir);

Q_SIGNALS:
    void finished(bool success, const QString &errorMessage);
};
