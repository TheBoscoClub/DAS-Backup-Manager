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
