#pragma once

#include <QWidget>
#include <QVector>
#include "snapshotmodel.h"

class SnapshotTimeline : public QWidget
{
    Q_OBJECT

public:
    explicit SnapshotTimeline(SnapshotModel *model, QWidget *parent = nullptr);

    void setModel(SnapshotModel *model);

Q_SIGNALS:
    void snapshotSelected(qint64 snapshotId);

protected:
    void paintEvent(QPaintEvent *event) override;
    void mousePressEvent(QMouseEvent *event) override;
    QSize sizeHint() const override;
    QSize minimumSizeHint() const override;

private:
    struct HitRect {
        QRect rect;
        qint64 snapshotId = -1;
        bool isDateGroup = false;
    };

    SnapshotModel *m_model = nullptr;
    qint64 m_selectedId = -1;
    QVector<HitRect> m_hitRects;

    void recalculate();

    static constexpr int TimelineX = 20;
    static constexpr int NodeRadius = 5;
    static constexpr int DatePillHeight = 28;
    static constexpr int SnapRowHeight = 24;
    static constexpr int DateGap = 16;
    static constexpr int LeftPadding = 12;
    static constexpr int TopPadding = 12;
};
