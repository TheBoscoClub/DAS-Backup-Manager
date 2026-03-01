#include "progresspanel.h"
#include "dbusclient.h"

#include <QFontDatabase>
#include <QHBoxLayout>
#include <QLabel>
#include <QPlainTextEdit>
#include <QProgressBar>
#include <QPushButton>
#include <QRegularExpression>
#include <QScrollBar>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>

ProgressPanel::ProgressPanel(DBusClient *client, QWidget *parent)
    : QDockWidget(tr("Progress"), parent)
    , m_client(client)
{
    setAllowedAreas(Qt::BottomDockWidgetArea | Qt::TopDockWidgetArea);
    setFeatures(QDockWidget::DockWidgetClosable);

    auto *container = new QWidget(this);
    auto *mainLayout = new QVBoxLayout(container);
    mainLayout->setContentsMargins(8, 6, 8, 6);
    mainLayout->setSpacing(4);

    // Row 1: operation label + stage label + cancel button
    auto *row1 = new QHBoxLayout();
    row1->setSpacing(8);

    m_operationLabel = new QLabel(this);
    QFont boldFont = m_operationLabel->font();
    boldFont.setBold(true);
    m_operationLabel->setFont(boldFont);

    m_stageLabel = new QLabel(this);

    m_cancelButton = new QPushButton(
        QIcon::fromTheme(QStringLiteral("process-stop")),
        tr("Cancel"), this);
    m_cancelButton->setToolTip(tr("Cancel the current operation"));
    m_cancelButton->setEnabled(false);

    row1->addWidget(m_operationLabel);
    row1->addWidget(m_stageLabel, 1);
    row1->addWidget(m_cancelButton);
    mainLayout->addLayout(row1);

    // Row 2: progress bar
    m_progressBar = new QProgressBar(this);
    m_progressBar->setRange(0, 100);
    m_progressBar->setValue(0);
    m_progressBar->setTextVisible(true);
    mainLayout->addWidget(m_progressBar);

    // Row 3: throughput label + ETA label + log toggle
    auto *row3 = new QHBoxLayout();
    row3->setSpacing(8);

    m_throughputLabel = new QLabel(this);
    m_etaLabel = new QLabel(this);

    m_logToggle = new QToolButton(this);
    m_logToggle->setIcon(QIcon::fromTheme(QStringLiteral("arrow-up")));
    m_logToggle->setText(tr("Log"));
    m_logToggle->setToolTip(tr("Toggle log output visibility"));
    m_logToggle->setToolButtonStyle(Qt::ToolButtonTextBesideIcon);
    m_logToggle->setCheckable(true);
    m_logToggle->setChecked(false);

    row3->addWidget(m_throughputLabel);
    row3->addWidget(m_etaLabel);
    row3->addStretch(1);
    row3->addWidget(m_logToggle);
    mainLayout->addLayout(row3);

    // Row 4: log view (initially hidden)
    m_logView = new QPlainTextEdit(this);
    m_logView->setReadOnly(true);
    m_logView->setWordWrapMode(QTextOption::NoWrap);
    m_logView->setMaximumBlockCount(5000);
    m_logView->setFont(QFontDatabase::systemFont(QFontDatabase::FixedFont));
    m_logView->setVisible(false);
    mainLayout->addWidget(m_logView, 1);

    setWidget(container);

    // Connect DBusClient signals
    connect(m_client, &DBusClient::jobStarted,
            this, &ProgressPanel::onJobStarted);
    connect(m_client, &DBusClient::jobProgress,
            this, &ProgressPanel::onJobProgress);
    connect(m_client, &DBusClient::jobLog,
            this, &ProgressPanel::onJobLog);
    connect(m_client, &DBusClient::jobFinished,
            this, &ProgressPanel::onJobFinished);

    // Private slot connections
    connect(m_cancelButton, &QPushButton::clicked,
            this, &ProgressPanel::cancelJob);
    connect(m_logToggle, &QToolButton::toggled,
            this, &ProgressPanel::toggleLog);

    // Start hidden
    setIdle();
}

void ProgressPanel::onJobStarted(const QString &jobId, const QString &operation)
{
    m_currentJobId = jobId;
    m_operationLabel->setText(operation);
    m_stageLabel->clear();
    m_throughputLabel->clear();
    m_etaLabel->clear();
    m_progressBar->setValue(0);
    m_cancelButton->setEnabled(true);
    m_logView->clear();

    show();
}

void ProgressPanel::onJobProgress(const QString &jobId, const QString &stage,
                                   int percent, const QString &message)
{
    if (jobId != m_currentJobId) {
        return;
    }

    m_progressBar->setValue(percent);
    m_stageLabel->setText(stage);

    // Parse throughput and ETA from message if present.
    // Expected format fragments: "throughput: 123 MB/s" and "ETA: 5m 30s"
    if (!message.isEmpty()) {
        static const QRegularExpression throughputRe(
            QStringLiteral(R"(throughput:\s*(.+?)(?:\s*[,|]|$))"),
            QRegularExpression::CaseInsensitiveOption);
        static const QRegularExpression etaRe(
            QStringLiteral(R"(ETA:\s*(.+?)(?:\s*[,|]|$))"),
            QRegularExpression::CaseInsensitiveOption);

        auto throughputMatch = throughputRe.match(message);
        if (throughputMatch.hasMatch()) {
            m_throughputLabel->setText(
                tr("Throughput: %1").arg(throughputMatch.captured(1).trimmed()));
        }

        auto etaMatch = etaRe.match(message);
        if (etaMatch.hasMatch()) {
            m_etaLabel->setText(
                tr("ETA: %1").arg(etaMatch.captured(1).trimmed()));
        }
    }
}

void ProgressPanel::onJobLog(const QString &jobId, const QString &level,
                              const QString &message)
{
    if (jobId != m_currentJobId) {
        return;
    }

    // Build prefix with color via HTML-like approach in plain text;
    // use appendHtml on the underlying document for colored output.
    QString prefix;
    QString color;

    if (level == QLatin1String("WARN") || level == QLatin1String("WARNING")) {
        prefix = QStringLiteral("[WARN] ");
        color = QStringLiteral("#b8860b");  // dark goldenrod
    } else if (level == QLatin1String("ERROR")) {
        prefix = QStringLiteral("[ERROR] ");
        color = QStringLiteral("#cc0000");  // red
    } else {
        prefix = QStringLiteral("[INFO] ");
    }

    if (color.isEmpty()) {
        m_logView->appendPlainText(prefix + message);
    } else {
        // Use HTML to apply color while keeping monospace
        QString html = QStringLiteral("<span style=\"color:%1\">%2%3</span>")
            .arg(color, prefix.toHtmlEscaped(), message.toHtmlEscaped());
        m_logView->appendHtml(html);
    }

    // Auto-scroll to bottom
    QScrollBar *vbar = m_logView->verticalScrollBar();
    vbar->setValue(vbar->maximum());
}

void ProgressPanel::onJobFinished(const QString &jobId, bool success,
                                   const QString &summary)
{
    if (jobId != m_currentJobId) {
        return;
    }

    m_cancelButton->setEnabled(false);

    if (success) {
        m_progressBar->setValue(100);
        m_operationLabel->setText(tr("Complete"));
        m_stageLabel->setText(summary);
    } else {
        // Error styling: red text on the progress bar
        m_progressBar->setStyleSheet(
            QStringLiteral("QProgressBar::chunk { background-color: #cc0000; }"));
        m_operationLabel->setText(tr("Failed"));
        m_stageLabel->setText(summary);
    }

    // Auto-hide after 5 seconds unless a new job starts
    QTimer::singleShot(5000, this, [this, jobId]() {
        // Only hide if no new job has replaced this one
        if (m_currentJobId == jobId) {
            setIdle();
        }
    });
}

void ProgressPanel::cancelJob()
{
    if (!m_currentJobId.isEmpty()) {
        m_client->jobCancel(m_currentJobId);
        m_cancelButton->setEnabled(false);
    }
}

void ProgressPanel::toggleLog()
{
    bool visible = m_logToggle->isChecked();
    m_logView->setVisible(visible);

    m_logToggle->setIcon(QIcon::fromTheme(
        visible ? QStringLiteral("arrow-down") : QStringLiteral("arrow-up")));
}

void ProgressPanel::setIdle()
{
    m_currentJobId.clear();
    m_progressBar->setValue(0);
    m_progressBar->setStyleSheet(QString());  // reset any error styling
    m_operationLabel->clear();
    m_stageLabel->clear();
    m_throughputLabel->clear();
    m_etaLabel->clear();
    m_cancelButton->setEnabled(false);

    hide();
}
