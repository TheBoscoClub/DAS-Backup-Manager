#include <QApplication>
#include <QCommandLineParser>

#include <KAboutData>
#include <KCrash>
#include <KLocalizedString>

#include "mainwindow.h"

using namespace Qt::Literals::StringLiterals;

int main(int argc, char *argv[])
{
    QApplication app(argc, argv);

    KLocalizedString::setApplicationDomain("btrdasd-gui");

    KAboutData aboutData(
        u"btrdasd-gui"_s,
        i18n("ButteredDASD"),
        u"0.1.0"_s,
        i18n("Search, browse, and restore files from BTRFS backup snapshots"),
        KAboutLicense::GPL_V3,
        i18n("(c) 2026 TheBoscoClub"),
        QString(),
        u"https://github.com/TheBoscoClub/DAS-Backup-Manager"_s);

    aboutData.addAuthor(
        i18n("Bosco"),
        i18n("Developer"),
        u"bosco@theboscoclub.com"_s);

    KAboutData::setApplicationData(aboutData);

    KCrash::initialize();

    QCommandLineParser parser;
    aboutData.setupCommandLine(&parser);
    parser.process(app);
    aboutData.processCommandLine(&parser);

    auto *window = new MainWindow();
    window->show();

    return app.exec();
}
