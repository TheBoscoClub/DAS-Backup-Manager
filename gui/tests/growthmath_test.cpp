// GrowthMath (gui/src/growthmath.h): the Growth tab's arithmetic, tested with no
// widget and no display (bd DAS-Backup-Manager-3gzs.15).

#include <QSet>
#include <QTest>

#include "../src/growthmath.h"

using namespace GrowthMath;

namespace {

constexpr qint64 gib = qint64{1} << 30;
const QDate day0(2026, 11, 1);

// A reading a day from day0, growing `perDay` GiB from `start` GiB, capacity
// `cap` GiB (none when 0).
QVector<Reading> daily(int count, qint64 start, qint64 perDay, qint64 cap)
{
    QVector<Reading> s;
    for (int i = 0; i < count; ++i) {
        Reading r{day0.addDays(i), (start + perDay * i) * gib, {}};
        if (cap > 0)
            r.total = cap * gib;
        s.append(r);
    }
    return s;
}

qint64 ms(const QDate &d)
{
    return QDateTime(d, QTime(0, 0)).toMSecsSinceEpoch();
}

} // namespace

class GrowthMathTest : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void everyRowIsProjectedFromItsOwnWindow()
    {
        const auto s = daily(30, 100, 10, 1000);
        // Row 5: used 150 of 1000, 10 a day: 85 days on.
        const Eta e5 = etaAt(s, 5);
        QCOMPARE(e5.kind, EtaKind::Date);
        QCOMPARE(e5.date, day0.addDays(5 + 85));
        QCOMPARE(e5.readings, 6);
        QCOMPARE(e5.windowStart, day0);
        // Row 29: the window is the 14 days up to it, days 16..29.
        const Eta e29 = etaAt(s, 29);
        QCOMPARE(e29.readings, 14);
        QCOMPARE(e29.windowStart, day0.addDays(16));
        QCOMPARE(e29.windowEnd, day0.addDays(29));
        QCOMPARE(e29.date, day0.addDays(29 + 61));
        QVERIFY(std::abs(e29.bytesPerDay - 10.0 * gib) < 1.0);
    }

    void theWindowIsByDateNotByCount()
    {
        // A month's gap: the row after it has one day in its window.
        QVector<Reading> s = daily(5, 100, 10, 1000);
        s.append(Reading{day0.addDays(40), 500 * gib, 1000 * gib});
        QCOMPARE(etaAt(s, 5).kind, EtaKind::NotEnoughHistory);
        QCOMPARE(etaAt(s, 5).readings, 1);
        // Two readings on one day are still one day.
        QVector<Reading> same{{day0, 100 * gib, 1000 * gib}, {day0, 110 * gib, 1000 * gib}};
        QCOMPARE(etaAt(same, 1).kind, EtaKind::NotEnoughHistory);
    }

    void aRowWithoutAProjectionSaysWhy()
    {
        QCOMPARE(etaAt(daily(1, 100, 10, 1000), 0).kind, EtaKind::NotEnoughHistory);
        QCOMPARE(etaAt(daily(10, 100, 10, 0), 9).kind, EtaKind::CapacityUnknown);
        QCOMPARE(etaAt(daily(10, 100, 0, 1000), 9).kind, EtaKind::Flat);
        // Shrinking is said as such, with its rate (operator 2026-11-27: the
        // targets lost 39 GiB a day while the row read "not growing").
        const Eta shrinking = etaAt(daily(10, 500, -10, 1000), 9);
        QCOMPARE(shrinking.kind, EtaKind::Shrinking);
        QVERIFY(shrinking.bytesPerDay < 0);
        QCOMPARE(etaAt(daily(10, 1000, 10, 1000), 9).kind, EtaKind::Full);
        // 1 GiB a day with 9000 GiB left: past ten years.
        QCOMPARE(etaAt(daily(10, 100, 1, 10000), 9).kind, EtaKind::BeyondTenYears);
        QCOMPARE(etaAt({}, 0).kind, EtaKind::NotEnoughHistory);
    }

    void freeIsCapacityLessUsedOrNone()
    {
        QCOMPARE(freeOf(Reading{day0, 300, 1000}), std::optional<qint64>(700));
        QCOMPARE(freeOf(Reading{day0, 1200, 1000}), std::optional<qint64>(0));
        QCOMPARE(freeOf(Reading{day0, 300, {}}), std::nullopt);
    }

    void aSeriesThatIsNoCurrentTargetIsRetired()
    {
        const QStringList current{QStringLiteral("primary-22tb"), QStringLiteral("system-recovery-A-2tb")};
        QVERIFY(!isRetired(QStringLiteral("primary-22tb"), current));
        QVERIFY(isRetired(QStringLiteral("/mnt/backup-system"), current));
        QVERIFY(isRetired(QStringLiteral("/mnt/backup-system-mirror"), current));
    }

    void eightSeriesGetEightColours()
    {
        QSet<QRgb> seen;
        for (int i = 0; i < 8; ++i)
            seen.insert(seriesColor(i).rgb());
        QCOMPARE(seen.size(), 8);
        for (int i = 0; i < 8; ++i)
            QVERIFY(!seen.contains(retiredColor(i).rgb()));
    }

    void presetsEndToday()
    {
        const qint64 today = ms(QDate(2026, 11, 27));
        const qint64 first = ms(QDate(2026, 2, 20));
        const Range r90 = presetRange(Preset::Days90, first, today, today);
        QCOMPARE(r90.endMs, today);
        QCOMPARE(r90.span(), 90 * kDayMs);
        QCOMPARE(presetRange(Preset::Days30, first, today, today).span(), 30 * kDayMs);
        QCOMPARE(presetRange(Preset::Year, first, today, today).span(), 365 * kDayMs);
        QCOMPARE(presetRange(Preset::All, first, today, today), (Range{first, today}));
        // Old data only: the day counts still end today.
        const qint64 may = ms(QDate(2026, 5, 17));
        QCOMPARE(presetRange(Preset::Days90, first, may, today).endMs, today);
        // No data at all: 90 days ending today.
        QCOMPARE(presetRange(Preset::All, std::nullopt, std::nullopt, today),
                 (Range{today - 90 * kDayMs, today}));
        // One day of data: widened either side.
        QCOMPARE(presetRange(Preset::All, today, today, today).span(), 2 * kDayMs);
    }

    void theWheelZoomsAboutThePointer()
    {
        const Range limit{0, 100 * kDayMs};
        const Range r{0, 100 * kDayMs};
        const Range in = zoomRange(r, 25 * kDayMs, 0.5, limit);
        QCOMPARE(in.span(), 50 * kDayMs);
        // The anchor stays a quarter of the way across.
        QCOMPARE(in.startMs + in.span() / 4, 25 * kDayMs);
        // Out again: never wider than the limit, and inside it.
        QCOMPARE(zoomRange(in, 25 * kDayMs, 4.0, limit), limit);
        // Never under two days.
        QCOMPARE(zoomRange(r, 50 * kDayMs, 0.0001, limit).span(), kMinSpanMs);
    }

    void dragPansWithinTheData()
    {
        const Range limit{0, 100 * kDayMs};
        const Range r{40 * kDayMs, 50 * kDayMs};
        QCOMPARE(panRange(r, 5 * kDayMs, limit), (Range{45 * kDayMs, 55 * kDayMs}));
        QCOMPARE(panRange(r, -100 * kDayMs, limit), (Range{0, 10 * kDayMs}));
        QCOMPARE(panRange(r, 100 * kDayMs, limit), (Range{90 * kDayMs, 100 * kDayMs}));
    }

    void theValueAxisHasRoundTicks()
    {
        // The odd ticks of the finding (10678, 8008...) become round ones.
        const Axis a = niceAxis(8008, 10678);
        QVERIFY(a.lo <= 8008 && a.hi >= 10678);
        const double step = (a.hi - a.lo) / (a.ticks - 1);
        QCOMPARE(std::fmod(a.lo, step), 0.0);
        QVERIFY(step == 500 || step == 1000);
        // One value: a band around it.
        const Axis one = niceAxis(5, 5);
        QVERIFY(one.lo <= 5 && one.hi > 5);
    }

    void dateLabelsFollowTheSpan()
    {
        // Zoomed in until two ticks fall on one day, the labels carry the
        // time, so no two read the same (operator 2026-11-27: 10-12, 10-12, 10-13).
        QCOMPARE(dateFormatFor(2 * kDayMs), QStringLiteral("MM-dd HH:mm"));
        QCOMPARE(dateFormatFor((kDateTicks - 1) * kDayMs), QStringLiteral("MM-dd"));
        QCOMPARE(dateFormatFor(30 * kDayMs), QStringLiteral("MM-dd"));
        QCOMPARE(dateFormatFor(90 * kDayMs), QStringLiteral("MM-dd"));
        QCOMPARE(dateFormatFor(365 * kDayMs), QStringLiteral("yyyy-MM"));
    }
};

QTEST_GUILESS_MAIN(GrowthMathTest)
#include "growthmath_test.moc"
