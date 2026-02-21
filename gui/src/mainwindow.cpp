#include "mainwindow.h"

#include <QLabel>
#include <QStatusBar>

MainWindow::MainWindow(QWidget *parent)
    : KXmlGuiWindow(parent)
{
    auto *placeholder = new QLabel(QStringLiteral("ButteredDASD"), this);
    placeholder->setAlignment(Qt::AlignCenter);
    setCentralWidget(placeholder);

    statusBar()->showMessage(QStringLiteral("Ready"));

    setupGUI(Default, QStringLiteral("btrdasd-gui.rc"));
}

MainWindow::~MainWindow() = default;
