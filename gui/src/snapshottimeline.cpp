#include "snapshottimeline.h"

#include <QPainter>
#include <QPalette>
#include <QMouseEvent>

SnapshotTimeline::SnapshotTimeline(SnapshotModel *model, QWidget *parent)
    : QWidget(parent)
    , m_model(model)
{
    setMouseTracking(true);
    setSizePolicy(QSizePolicy::Preferred, QSizePolicy::Expanding);

    if (m_model) {
        connect(m_model, &QAbstractItemModel::modelReset, this, [this]() {
            recalculate();
            update();
        });
    }
}

void SnapshotTimeline::setModel(SnapshotModel *model)
{
    m_model = model;
    if (m_model) {
        connect(m_model, &QAbstractItemModel::modelReset, this, [this]() {
            recalculate();
            update();
        });
    }
    recalculate();
    update();
}

void SnapshotTimeline::recalculate()
{
    m_hitRects.clear();
    if (!m_model) return;

    int y = TopPadding;
    int numGroups = m_model->rowCount();

    for (int g = 0; g < numGroups; ++g) {
        auto groupIdx = m_model->index(g, 0);

        QRect pillRect(LeftPadding, y, width() - LeftPadding * 2, DatePillHeight);
        m_hitRects.append({.rect = pillRect, .snapshotId = -1, .isDateGroup = true});
        y += DatePillHeight + 4;

        int childCount = m_model->rowCount(groupIdx);
        for (int c = 0; c < childCount; ++c) {
            auto childIdx = m_model->index(c, 0, groupIdx);
            qint64 snapId = m_model->data(childIdx, SnapshotModel::SnapshotIdRole).toLongLong();

            QRect nodeRect(LeftPadding, y, width() - LeftPadding * 2, SnapRowHeight);
            m_hitRects.append({.rect = nodeRect, .snapshotId = snapId, .isDateGroup = false});
            y += SnapRowHeight;
        }

        y += DateGap;
    }

    setMinimumHeight(y);
}

void SnapshotTimeline::paintEvent(QPaintEvent * /*event*/)
{
    QPainter painter(this);
    painter.setRenderHint(QPainter::Antialiasing);

    const auto &pal = palette();
    QColor accentColor = pal.color(QPalette::Highlight);
    QColor textColor = pal.color(QPalette::WindowText);
    QColor surfaceColor = pal.color(QPalette::AlternateBase);
    QColor selectedBg = pal.color(QPalette::Highlight).lighter(180);

    if (!m_model) return;

    int y = TopPadding;
    int lineX = LeftPadding + TimelineX;
    int numGroups = m_model->rowCount();

    for (int g = 0; g < numGroups; ++g) {
        auto groupIdx = m_model->index(g, 0);
        QString dateLabel = m_model->data(groupIdx, Qt::DisplayRole).toString();

        // Date pill
        QRect pillRect(LeftPadding, y, width() - LeftPadding * 2, DatePillHeight);
        painter.setBrush(surfaceColor);
        painter.setPen(Qt::NoPen);
        painter.drawRoundedRect(pillRect, 6, 6);

        // Date pill circle
        painter.setBrush(accentColor);
        painter.drawEllipse(QPoint(lineX, y + DatePillHeight / 2), NodeRadius + 2, NodeRadius + 2);

        // Date text
        painter.setPen(textColor);
        QFont boldFont = font();
        boldFont.setBold(true);
        painter.setFont(boldFont);
        painter.drawText(pillRect.adjusted(TimelineX + NodeRadius + 12, 0, 0, 0),
                         Qt::AlignVCenter, dateLabel);
        painter.setFont(font());
        y += DatePillHeight + 4;

        // Timeline line segment for children
        int childCount = m_model->rowCount(groupIdx);
        if (childCount > 0) {
            int lineTop = y;
            int lineBottom = y + childCount * SnapRowHeight - SnapRowHeight / 2;
            painter.setPen(QPen(accentColor, 2));
            painter.drawLine(lineX, lineTop, lineX, lineBottom);
        }

        // Snapshot nodes
        for (int c = 0; c < childCount; ++c) {
            auto childIdx = m_model->index(c, 0, groupIdx);
            qint64 snapId = m_model->data(childIdx, SnapshotModel::SnapshotIdRole).toLongLong();
            QString label = m_model->data(childIdx, Qt::DisplayRole).toString();

            QRect rowRect(LeftPadding, y, width() - LeftPadding * 2, SnapRowHeight);

            if (snapId == m_selectedId) {
                painter.fillRect(rowRect, selectedBg);
            }

            // Branch connector
            painter.setPen(QPen(accentColor, 1));
            int nodeY = y + SnapRowHeight / 2;
            painter.drawLine(lineX, nodeY, lineX + 10, nodeY);

            // Node circle
            bool selected = (snapId == m_selectedId);
            painter.setBrush(selected ? accentColor : pal.color(QPalette::Window));
            painter.setPen(QPen(accentColor, 2));
            painter.drawEllipse(QPoint(lineX + 14, nodeY), NodeRadius, NodeRadius);

            // Label
            painter.setPen(textColor);
            painter.drawText(QRect(lineX + 24, y, width() - lineX - 30, SnapRowHeight),
                             Qt::AlignVCenter, label);

            y += SnapRowHeight;
        }

        y += DateGap;
    }
}

void SnapshotTimeline::mousePressEvent(QMouseEvent *event)
{
    for (const auto &hit : m_hitRects) {
        if (hit.rect.contains(event->pos()) && !hit.isDateGroup && hit.snapshotId >= 0) {
            m_selectedId = hit.snapshotId;
            update();
            Q_EMIT snapshotSelected(hit.snapshotId);
            return;
        }
    }
    QWidget::mousePressEvent(event);
}

QSize SnapshotTimeline::sizeHint() const
{
    return {220, 400};
}

QSize SnapshotTimeline::minimumSizeHint() const
{
    return {180, 200};
}
