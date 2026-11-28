#include "growthmath.h"

#include <QSet>

#include <algorithm>
#include <array>
#include <cmath>

namespace GrowthMath {

Eta etaAt(const QVector<Reading> &series, int index)
{
    Eta eta;
    if (index < 0 || index >= series.size())
        return eta;
    const Reading &row = series[index];

    // The readings dated within the 14 days up to and including the row's
    // date, by date, not by position: later rows never count.
    double sumX = 0, sumY = 0, sumXX = 0, sumXY = 0;
    int n = 0;
    QSet<qint64> days;
    for (const Reading &r : series) {
        const qint64 back = r.date.daysTo(row.date);
        if (back < 0 || back >= kEtaWindowDays)
            continue;
        const double x = static_cast<double>(-back);
        const double y = static_cast<double>(r.used);
        sumX += x;
        sumY += y;
        sumXX += x * x;
        sumXY += x * y;
        ++n;
        days.insert(r.date.toJulianDay());
        if (!eta.windowStart.isValid() || r.date < eta.windowStart)
            eta.windowStart = r.date;
        if (!eta.windowEnd.isValid() || r.date > eta.windowEnd)
            eta.windowEnd = r.date;
    }
    eta.readings = n;

    if (days.size() < 2) {
        eta.kind = EtaKind::NotEnoughHistory;
        return eta;
    }
    if (!row.total) {
        eta.kind = EtaKind::CapacityUnknown;
        return eta;
    }
    // Two distinct days make the denominator positive.
    const double denom = n * sumXX - sumX * sumX;
    eta.bytesPerDay = (n * sumXY - sumX * sumY) / denom;
    if (row.used >= *row.total) {
        eta.kind = EtaKind::Full;
        return eta;
    }
    if (eta.bytesPerDay < 0) {
        eta.kind = EtaKind::Shrinking;
        return eta;
    }
    if (!(eta.bytesPerDay > 0)) { // zero, or no trend at all
        eta.kind = EtaKind::Flat;
        return eta;
    }
    const double daysLeft = static_cast<double>(*row.total - row.used) / eta.bytesPerDay;
    if (daysLeft > 365.0 * 10) {
        eta.kind = EtaKind::BeyondTenYears;
        return eta;
    }
    // Full by the end of the day the line reaches the capacity; a hair of
    // rounding error never moves it a day later.
    eta.kind = EtaKind::Date;
    eta.date = row.date.addDays(static_cast<qint64>(std::ceil(daysLeft - 1e-6)));
    return eta;
}

std::optional<qint64> freeOf(const Reading &reading)
{
    if (!reading.total)
        return std::nullopt;
    return std::max<qint64>(0, *reading.total - reading.used);
}

bool isRetired(const QString &label, const QStringList &currentLabels)
{
    return !currentLabels.contains(label);
}

QColor seriesColor(int index)
{
    // Eight hues told apart in light and dark themes (Tableau 10, less grey
    // and the two nearest the retired grey).
    static constexpr std::array<QRgb, 8> palette = {
        0x1f77b4, // blue
        0xff7f0e, // orange
        0x2ca02c, // green
        0xd62728, // red
        0x9467bd, // purple
        0x8c564b, // brown
        0xe377c2, // pink
        0x17becf, // cyan
    };
    const auto i = static_cast<std::size_t>(index < 0 ? 0 : index) % palette.size();
    return QColor::fromRgb(palette[i]);
}

QColor retiredColor(int index)
{
    return (index % 2 == 0) ? QColor(0x80, 0x80, 0x80) : QColor(0xa8, 0xa8, 0xa8);
}

Range presetRange(Preset preset, std::optional<qint64> earliestMs, std::optional<qint64> latestMs,
                  qint64 todayMs)
{
    const qint64 end = latestMs ? std::max(*latestMs, todayMs) : todayMs;
    auto days = [end](int n) { return Range{end - qint64{n} * kDayMs, end}; };
    switch (preset) {
    case Preset::Days30:
        return days(30);
    case Preset::Days90:
        return days(90);
    case Preset::Year:
        return days(365);
    case Preset::All:
        break;
    }
    if (!earliestMs || !latestMs)
        return days(90);
    Range r{*earliestMs, *latestMs};
    if (r.span() < kDayMs)
        r = Range{r.startMs - kDayMs, r.endMs + kDayMs};
    return r;
}

Range zoomRange(const Range &range, qint64 anchorMs, double factor, const Range &limit)
{
    if (range.span() <= 0 || !(factor > 0))
        return range;
    const double span = static_cast<double>(range.span());
    const double maxSpan = static_cast<double>(std::max(limit.span(), kMinSpanMs));
    const double newSpan = std::clamp(span * factor, static_cast<double>(kMinSpanMs), maxSpan);
    // The anchor keeps its place: the same fraction of the way across.
    const double frac = std::clamp(static_cast<double>(anchorMs - range.startMs) / span, 0.0, 1.0);
    const auto start = static_cast<qint64>(std::llround(static_cast<double>(anchorMs) - frac * newSpan));
    Range r{start, start + static_cast<qint64>(std::llround(newSpan))};
    return panRange(r, 0, limit);
}

Range panRange(const Range &range, qint64 deltaMs, const Range &limit)
{
    Range r{range.startMs + deltaMs, range.endMs + deltaMs};
    if (r.span() >= limit.span())
        return Range{limit.startMs, limit.startMs + r.span()};
    if (r.startMs < limit.startMs)
        r = Range{limit.startMs, limit.startMs + range.span()};
    if (r.endMs > limit.endMs)
        r = Range{limit.endMs - range.span(), limit.endMs};
    return r;
}

static double niceNumber(double x, bool round)
{
    const double exp = std::floor(std::log10(x));
    const double f = x / std::pow(10.0, exp);
    double nf;
    if (round)
        nf = f < 1.5 ? 1 : f < 3 ? 2 : f < 7 ? 5 : 10;
    else
        nf = f <= 1 ? 1 : f <= 2 ? 2 : f <= 5 ? 5 : 10;
    return nf * std::pow(10.0, exp);
}

Axis niceAxis(double lo, double hi, int maxTicks)
{
    if (!(hi > lo)) {
        // One value (or none): a band around it, from 0 when it is small.
        const double pad = std::max(std::abs(hi) * 0.1, 1.0);
        lo = std::max(0.0, lo - pad);
        hi = hi + pad;
    }
    maxTicks = std::max(maxTicks, 2);
    const double range = niceNumber(hi - lo, false);
    const double step = niceNumber(range / (maxTicks - 1), true);
    Axis a;
    a.lo = std::floor(lo / step) * step;
    a.hi = std::ceil(hi / step) * step;
    a.ticks = static_cast<int>(std::llround((a.hi - a.lo) / step)) + 1;
    return a;
}

QString dateFormatFor(qint64 spanMs)
{
    // A gap between two ticks under a day puts two labels on one day.
    if (spanMs < static_cast<qint64>(kDateTicks - 1) * kDayMs)
        return QStringLiteral("MM-dd HH:mm");
    return spanMs <= 120 * kDayMs ? QStringLiteral("MM-dd") : QStringLiteral("yyyy-MM");
}

} // namespace GrowthMath
