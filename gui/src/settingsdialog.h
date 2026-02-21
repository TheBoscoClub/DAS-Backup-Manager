#pragma once

#include <KConfigDialog>

class QLineEdit;
class QCheckBox;

class SettingsDialog : public KConfigDialog
{
    Q_OBJECT

public:
    explicit SettingsDialog(QWidget *parent, const QString &name, KCoreConfigSkeleton *config);

private:
    QLineEdit *m_dbPathEdit = nullptr;
    QLineEdit *m_watchPathEdit = nullptr;
    QCheckBox *m_autoWatchCheck = nullptr;
    QLineEdit *m_restoreDestEdit = nullptr;
};
