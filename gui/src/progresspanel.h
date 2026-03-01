#pragma once

#include <QDockWidget>

class QProgressBar;
class QPlainTextEdit;
class QLabel;
class QPushButton;
class QToolButton;
class DBusClient;

class ProgressPanel : public QDockWidget
{
    Q_OBJECT

public:
    explicit ProgressPanel(DBusClient *client, QWidget *parent = nullptr);

public Q_SLOTS:
    void onJobStarted(const QString &jobId, const QString &operation);
    void onJobProgress(const QString &jobId, const QString &stage,
                       int percent, const QString &message);
    void onJobLog(const QString &jobId, const QString &level,
                  const QString &message);
    void onJobFinished(const QString &jobId, bool success,
                       const QString &summary);

private Q_SLOTS:
    void cancelJob();
    void toggleLog();

private:
    void setIdle();

    DBusClient *m_client;
    QString m_currentJobId;

    QLabel *m_operationLabel = nullptr;
    QLabel *m_stageLabel = nullptr;
    QLabel *m_throughputLabel = nullptr;
    QLabel *m_etaLabel = nullptr;
    QProgressBar *m_progressBar = nullptr;
    QPushButton *m_cancelButton = nullptr;
    QToolButton *m_logToggle = nullptr;
    QPlainTextEdit *m_logView = nullptr;
};
