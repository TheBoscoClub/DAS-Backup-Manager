#include "sidebar.h"

#include <QIcon>
#include <QTreeWidgetItem>
#include <QTreeWidgetItemIterator>

namespace {
constexpr int SectionRole = Qt::UserRole + 1;
}

Sidebar::Sidebar(QWidget *parent)
    : QTreeWidget(parent)
{
    setHeaderHidden(true);
    setRootIsDecorated(true);
    setIndentation(16);
    setIconSize(QSize(16, 16));
    setMinimumWidth(180);
    setMaximumWidth(280);

    buildTree();

    connect(this, &QTreeWidget::itemClicked,
            this, &Sidebar::onItemClicked);

    expandAll();
}

void Sidebar::buildTree()
{
    // Browse section
    auto *browse = new QTreeWidgetItem(this);
    browse->setText(0, tr("Browse"));
    browse->setIcon(0, QIcon::fromTheme(QStringLiteral("folder-open")));
    browse->setFlags(browse->flags() & ~Qt::ItemIsSelectable);

    addSection(browse, tr("Snapshots"), SidebarSection::BrowseSnapshots,
               QStringLiteral("drive-harddisk"));
    addSection(browse, tr("Search"), SidebarSection::BrowseSearch,
               QStringLiteral("edit-find"));

    // Backup section
    auto *backup = new QTreeWidgetItem(this);
    backup->setText(0, tr("Backup"));
    backup->setIcon(0, QIcon::fromTheme(QStringLiteral("document-save-all")));
    backup->setFlags(backup->flags() & ~Qt::ItemIsSelectable);

    addSection(backup, tr("Run Now"), SidebarSection::BackupRunNow,
               QStringLiteral("media-playback-start"));
    addSection(backup, tr("History"), SidebarSection::BackupHistory,
               QStringLiteral("view-history"));

    // Config section (leaf — no children)
    auto *config = new QTreeWidgetItem(this);
    config->setText(0, tr("Config"));
    config->setIcon(0, QIcon::fromTheme(QStringLiteral("configure")));
    config->setData(0, SectionRole, static_cast<int>(SidebarSection::Config));

    // Health section
    auto *health = new QTreeWidgetItem(this);
    health->setText(0, tr("Health"));
    health->setIcon(0, QIcon::fromTheme(QStringLiteral("dialog-information")));
    health->setFlags(health->flags() & ~Qt::ItemIsSelectable);

    addSection(health, tr("Drives"), SidebarSection::HealthDrives,
               QStringLiteral("drive-harddisk"));
    addSection(health, tr("Growth"), SidebarSection::HealthGrowth,
               QStringLiteral("office-chart-line"));
    addSection(health, tr("Status"), SidebarSection::HealthStatus,
               QStringLiteral("security-high"));
}

QTreeWidgetItem *Sidebar::addSection(QTreeWidgetItem *parent,
                                      const QString &label,
                                      SidebarSection section,
                                      const QString &icon)
{
    auto *item = new QTreeWidgetItem(parent);
    item->setText(0, label);
    item->setIcon(0, QIcon::fromTheme(icon));
    item->setData(0, SectionRole, static_cast<int>(section));
    return item;
}

void Sidebar::setCurrentSection(SidebarSection section)
{
    // Walk all items to find the one matching this section
    QTreeWidgetItemIterator it(this);
    while (*it) {
        const QVariant data = (*it)->data(0, SectionRole);
        if (data.isValid() && static_cast<SidebarSection>(data.toInt()) == section) {
            setCurrentItem(*it);
            Q_EMIT sectionChanged(section);
            return;
        }
        ++it;
    }
}

void Sidebar::onItemClicked(QTreeWidgetItem *item, int /*column*/)
{
    const QVariant data = item->data(0, SectionRole);
    if (data.isValid()) {
        Q_EMIT sectionChanged(static_cast<SidebarSection>(data.toInt()));
    }
}
