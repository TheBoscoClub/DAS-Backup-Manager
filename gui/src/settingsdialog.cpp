#include "settingsdialog.h"

#include <KLocalizedString>

#include <QCheckBox>
#include <QFormLayout>
#include <QLineEdit>
#include <QWidget>

SettingsDialog::SettingsDialog(QWidget *parent, const QString &name, KCoreConfigSkeleton *config)
    : KConfigDialog(parent, name, config)
{
    auto *page = new QWidget(this);
    auto *layout = new QFormLayout(page);

    m_dbPathEdit = new QLineEdit(page);
    m_dbPathEdit->setObjectName(QStringLiteral("kcfg_DatabasePath"));
    layout->addRow(i18n("Database path:"), m_dbPathEdit);

    m_watchPathEdit = new QLineEdit(page);
    m_watchPathEdit->setObjectName(QStringLiteral("kcfg_WatchPath"));
    layout->addRow(i18n("Watch path:"), m_watchPathEdit);

    m_autoWatchCheck = new QCheckBox(i18n("Auto-watch for new snapshots"), page);
    m_autoWatchCheck->setObjectName(QStringLiteral("kcfg_AutoWatch"));
    layout->addRow(QString(), m_autoWatchCheck);

    m_restoreDestEdit = new QLineEdit(page);
    m_restoreDestEdit->setObjectName(QStringLiteral("kcfg_DefaultRestorePath"));
    layout->addRow(i18n("Default restore destination:"), m_restoreDestEdit);

    addPage(page, i18n("General"), QStringLiteral("configure"));
}
