#pragma once

#include <QObject>
#include <QString>

class DBusClient;

class IndexRunner : public QObject
{
    Q_OBJECT

public:
    explicit IndexRunner(DBusClient *client, QObject *parent = nullptr);

    void run(const QString &configPath, const QString &targetPath, const QString &dbPath);
    void abort();
    [[nodiscard]] bool isRunning() const;

Q_SIGNALS:
    void outputLine(const QString &line);
    void finished(bool success, const QString &errorMessage);

private:
    DBusClient *m_client;
    QString m_currentJobId;
    bool m_running = false;
};
