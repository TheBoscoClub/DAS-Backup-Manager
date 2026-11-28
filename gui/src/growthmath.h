#pragma once
// The Growth tab's arithmetic, apart from any widget so it is tested without a
// display (bd DAS-Backup-Manager-3gzs.15): each row's full-by projection and
// free space, which series are retired, the series colours, and the date and
// value ranges the chart shows.

#include <QColor>
#include <QDate>
#include <QString>
#include <QStringList>
#include <QVector>

#include <optional>

namespace GrowthMath {

// One growth.log reading of one target, as the helper sends it: the day, the
// bytes in use, and the capacity when known (never a 0 standing for unknown).
struct Reading {
    QDate date;
    qint64 used = 0;
    std::optional<qint64> total;
};

// Why a row has, or has no, full-by date.
enum class EtaKind {
    Date,             // `date` is the projected day the target fills
    BeyondTenYears,   // growing, but more than ten years from full
    NotEnoughHistory, // fewer than two days of readings in the window
    CapacityUnknown,  // the row has no capacity
    Flat,             // the window's trend is flat
    Shrinking,        // the window's trend is falling: `bytesPerDay` < 0
    Full,             // used is already at or past the capacity
};

struct Eta {
    EtaKind kind = EtaKind::NotEnoughHistory;
    QDate date;                // set for EtaKind::Date
    double bytesPerDay = 0;    // the trend, when one was fitted
    int readings = 0;          // readings in the window
    QDate windowStart;         // the window's first and last reading
    QDate windowEnd;
};

// The days a row's projection looks back over, its own day included.
inline constexpr int kEtaWindowDays = 14;

// The full-by projection of `series[index]`: a least-squares line through the
// series' readings dated within the 14 days up to and including that row's
// date. `series` is in date order.
[[nodiscard]] Eta etaAt(const QVector<Reading> &series, int index);

// The row's free bytes, capacity less used (never below 0); none without a
// capacity.
[[nodiscard]] std::optional<qint64> freeOf(const Reading &reading);

// A series whose label is no current target's is retired.
[[nodiscard]] bool isRetired(const QString &label, const QStringList &currentLabels);

// The colour of the index-th current series: eight distinct colours before
// any repeats. Retired series are grey (two shades, alternating).
[[nodiscard]] QColor seriesColor(int index);
[[nodiscard]] QColor retiredColor(int index);

// A span of the date axis, milliseconds since the epoch.
struct Range {
    qint64 startMs = 0;
    qint64 endMs = 0;
    [[nodiscard]] qint64 span() const { return endMs - startMs; }
    bool operator==(const Range &) const = default;
};

enum class Preset { Days30, Days90, Year, All };

inline constexpr qint64 kDayMs = qint64{86400} * 1000;
// The narrowest the wheel zooms to: two days.
inline constexpr qint64 kMinSpanMs = 2 * kDayMs;

// The range a preset shows. A day count ends at `todayMs` (or the newest
// reading, if later); All runs from the earliest to the latest reading shown.
// No readings: the day count ending today. A single day is widened by a day
// either side.
[[nodiscard]] Range presetRange(Preset preset, std::optional<qint64> earliestMs,
                                std::optional<qint64> latestMs, qint64 todayMs);

// `range` zoomed by `factor` (below 1 zooms in) about `anchorMs`, which stays
// where it is on screen; never narrower than kMinSpanMs, never wider than
// `limit` (the whole of the data, or the preset when wider).
[[nodiscard]] Range zoomRange(const Range &range, qint64 anchorMs, double factor, const Range &limit);

// `range` moved by `deltaMs`, kept inside `limit` when it fits there.
[[nodiscard]] Range panRange(const Range &range, qint64 deltaMs, const Range &limit);

// A value axis with round ticks covering [lo, hi].
struct Axis {
    double lo = 0;
    double hi = 1;
    int ticks = 2;
};
[[nodiscard]] Axis niceAxis(double lo, double hi, int maxTicks = 6);

// Ticks on the date axis: the chart sets this many, and dateFormatFor knows
// two labels fall on one day once a gap between them is under a day.
inline constexpr int kDateTicks = 7;

// The date-axis label format for a span: day, month and time while two ticks
// could fall on one day, day and month up to ~four months, year and month beyond.
[[nodiscard]] QString dateFormatFor(qint64 spanMs);

} // namespace GrowthMath
