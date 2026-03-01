#pragma once

#include <QTreeWidget>

enum class SidebarSection {
    BrowseSnapshots,
    BrowseSearch,
    BackupRunNow,
    BackupHistory,
    Config,
    HealthDrives,
    HealthGrowth,
    HealthStatus,
};

class Sidebar : public QTreeWidget
{
    Q_OBJECT

public:
    explicit Sidebar(QWidget *parent = nullptr);

    void setCurrentSection(SidebarSection section);

Q_SIGNALS:
    void sectionChanged(SidebarSection section);

private Q_SLOTS:
    void onItemClicked(QTreeWidgetItem *item, int column);

private:
    void buildTree();
    QTreeWidgetItem *addSection(QTreeWidgetItem *parent, const QString &label,
                                SidebarSection section, const QString &icon);
};
