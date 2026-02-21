#include "restoreaction.h"

#include <KIO/CopyJob>
#include <KJobWidgets>

#include <QUrl>
#include <QWidget>

RestoreAction::RestoreAction(QObject *parent)
    : QObject(parent)
{
}

void RestoreAction::restore(const QString &sourcePath, const QString &destinationDir)
{
    QUrl sourceUrl = QUrl::fromLocalFile(sourcePath);
    QUrl destUrl = QUrl::fromLocalFile(destinationDir);

    KIO::CopyJob *job = KIO::copy(sourceUrl, destUrl, KIO::DefaultFlags);

    auto *parentWidget = qobject_cast<QWidget *>(parent());
    if (parentWidget) {
        KJobWidgets::setWindow(job, parentWidget);
    }

    connect(job, &KJob::result, this, [this](KJob *j) {
        if (j->error()) {
            Q_EMIT finished(false, j->errorString());
        } else {
            Q_EMIT finished(true, QString());
        }
    });

    job->start();
}
