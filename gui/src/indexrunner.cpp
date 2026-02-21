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
