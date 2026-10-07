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
    // The helper accepted the cancel: the job stops at its next safe
    // boundary, which may be the end of a long btrbk send. Until its
    // JobFinished arrives the button says "Cancelling…" and stays disabled.
    void showCancelRequested();

private Q_SLOTS:
    void cancelJob();
    void toggleLog();

private:
    void resetPanel();
    // The button as it is while no cancel is pending.
    void showCancelIdle(bool enabled);

    DBusClient *m_client;
    QString m_currentJobId;
    bool m_userScrolledUp = false;

    QLabel *m_operationLabel = nullptr;
    QLabel *m_stageLabel = nullptr;
    QLabel *m_throughputLabel = nullptr;
    QLabel *m_etaLabel = nullptr;
    QProgressBar *m_progressBar = nullptr;
    QPushButton *m_cancelButton = nullptr;
    QToolButton *m_logToggle = nullptr;
    QPlainTextEdit *m_logView = nullptr;
};
