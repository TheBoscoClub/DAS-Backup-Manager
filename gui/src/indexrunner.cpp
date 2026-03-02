#include "indexrunner.h"
#include "dbusclient.h"

IndexRunner::IndexRunner(DBusClient *client, QObject *parent)
    : QObject(parent)
    , m_client(client)
{
    connect(m_client, &DBusClient::jobStarted,
            this, [this](const QString &jobId, const QString &operation) {
        if (operation == QLatin1String("IndexWalk")) {
            m_currentJobId = jobId;
            m_running = true;
        }
    });

    connect(m_client, &DBusClient::jobLog,
            this, [this](const QString &jobId, const QString & /*level*/,
                         const QString &message) {
        if (jobId == m_currentJobId)
            Q_EMIT outputLine(message);
    });

    connect(m_client, &DBusClient::jobFinished,
            this, [this](const QString &jobId, bool success, const QString &summary) {
        if (jobId == m_currentJobId) {
            m_running = false;
            m_currentJobId.clear();
            Q_EMIT finished(success, success ? QString() : summary);
        }
    });
}

void IndexRunner::run(const QString &configPath, const QString &targetPath,
                      const QString &dbPath)
{
    if (m_running)
        return;

    m_client->indexWalk(configPath, targetPath, dbPath);
}

void IndexRunner::abort()
{
    if (m_running && !m_currentJobId.isEmpty()) {
        m_client->jobCancel(m_currentJobId);
    }
}

bool IndexRunner::isRunning() const
{
    return m_running;
}
