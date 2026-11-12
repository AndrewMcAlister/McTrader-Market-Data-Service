//! Intraday timeframes and the session-anchored aggregation rule.
//!
//! The vendor's native intraday tape is 5-minute bars. Every larger timeframe here is derived
//! from that one series, so a response never mixes a native 1h bar with a 5m one.
//!
//! # Why the buckets are anchored to the session, not to the series
//!
//! The current McTrader code chunks the returned series from its first bar, and the series
//! begins at `now - window_days` — an arbitrary mid-session instant. The same 4-hour candle
//! therefore changes shape as the clock advances, and can straddle the overnight gap. Here the
//! buckets reset at each session's first bar, so a 4-hour bar is stable across requests and
//! never mixes two sessions.
//!
//! The session date is supplied by the caller ([`aggregate`] takes a closure): a US session
//! spans two UTC dates, so the grouping has to use the exchange's own zone, and the pure
//! function has no business loading a timezone table.

use chrono::{NaiveDate, NaiveDateTime};

/// The timeframes this route serves. `1Day` and `1Week` are not intraday and stay with
/// `POST /bars`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Timeframe {
    FiveMin,
    FifteenMin,
    ThirtyMin,
    OneHour,
    FourHour,
}

/// The timeframes in ascending order, for refusals that list them.
pub const TIMEFRAMES: [Timeframe; 5] = [
    Timeframe::FiveMin,
    Timeframe::FifteenMin,
    Timeframe::ThirtyMin,
    Timeframe::OneHour,
    Timeframe::FourHour,
];

impl Timeframe {
    /// The canonical spelling for every accepted name, case-insensitively.
    ///
    /// `5Min` is not a member of McTrader's `BarTimeframe` enum — 5m is the vendor interval the
    /// larger frames are aggregated from — but it is served here because it is the base series.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "5min" | "5m" => Some(Self::FiveMin),
            "15min" | "15m" => Some(Self::FifteenMin),
            "30min" | "30m" => Some(Self::ThirtyMin),
            "1hour" | "1h" => Some(Self::OneHour),
            "4hour" | "4h" => Some(Self::FourHour),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveMin => "5Min",
            Self::FifteenMin => "15Min",
            Self::ThirtyMin => "30Min",
            Self::OneHour => "1Hour",
            Self::FourHour => "4Hour",
        }
    }

    /// The vendor's interval this route fetches. Every timeframe derives from the same tape.
    pub fn vendor_interval(self) -> &'static str {
        "5m"
    }

    /// How many native 5m bars make one bar at this timeframe.
    pub fn bars_per_bucket(self) -> usize {
        match self {
            Self::FiveMin => 1,
            Self::FifteenMin => 3,
            Self::ThirtyMin => 6,
            Self::OneHour => 12,
            Self::FourHour => 48,
        }
    }

    /// The vendor window, in calendar days, that a request needs for `limit` buckets.
    ///
    /// Scaled exactly as McTrader's `get_historical_bars` scales it today, because a narrower
    /// window would silently return fewer buckets than asked for. 4h asks for one and a half
    /// bars a day (a US session is two 4h buckets), 1h for two a day, the rest one a day.
    /// Capped at the vendor's 600-day ceiling for the 5m interval.
    pub fn window_days(self, limit: i64) -> i64 {
        let limit = limit.max(1);
        let days = match self {
            Self::FourHour => (limit as f64 * 1.5).max(30.0) as i64,
            Self::OneHour => limit.saturating_mul(2).max(14),
            _ => limit.max(7),
        };
        days.clamp(1, VENDOR_MAX_WINDOW_DAYS)
    }

    /// How many native 5m rows a read must cover so `limit` buckets can be derived.
    ///
    /// `bars_per_bucket` per bucket plus two buckets of slack: a session's last bucket may be
    /// short, and a window that begins mid-session misaligns only that oldest session, whose
    /// buckets are discarded when the answer is trimmed to `limit`.
    pub fn native_rows_needed(self, limit: i64) -> i64 {
        let per_bucket = i64::try_from(self.bars_per_bucket()).unwrap_or(i64::MAX);
        limit.max(1).saturating_mul(per_bucket).saturating_add(per_bucket.saturating_mul(2))
    }
}

/// The vendor's 5m ceiling, in days, for one intraday request.
pub const VENDOR_MAX_WINDOW_DAYS: i64 = 600;

impl std::fmt::Display for Timeframe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One intraday bar at whatever granularity: the native 5m row, or a derived bucket.
///
/// `ts_utc` is the bar's **opening** instant in naive UTC — the store's `TsUtc` and the vendor
/// epoch both mean that, and a bucket keeps its first bar's.
#[derive(Debug, Clone, PartialEq)]
pub struct IntradayRow {
    pub ts_utc: NaiveDateTime,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

/// Aggregate native 5m rows into `timeframe` bars, session by session.
///
/// Rows are grouped by the session date `session_date_of` returns (the caller converts UTC to
/// the exchange's zone), then each session's rows are chunked consecutively in timestamp order.
/// A short final bucket is kept, not dropped. `d` — the returned row's timestamp — is the first
/// bar's.
pub fn aggregate<F>(rows: &[IntradayRow], timeframe: Timeframe, session_date_of: F) -> Vec<IntradayRow>
where
    F: Fn(NaiveDateTime) -> NaiveDate,
{
    let mut ordered: Vec<&IntradayRow> = rows.iter().collect();
    ordered.sort_by_key(|row| row.ts_utc);

    let per_bucket = timeframe.bars_per_bucket();
    let mut out: Vec<IntradayRow> = Vec::with_capacity(ordered.len() / per_bucket.max(1) + 1);
    let mut session: Option<NaiveDate> = None;
    let mut bucket: Vec<&IntradayRow> = Vec::with_capacity(per_bucket);

    for row in ordered {
        let date = session_date_of(row.ts_utc);
        if session != Some(date) {
            flush(&mut out, &mut bucket);
            session = Some(date);
        }
        bucket.push(row);
        if bucket.len() == per_bucket {
            flush(&mut out, &mut bucket);
        }
    }
    flush(&mut out, &mut bucket);
    out
}

/// Turn the rows collected for one bucket into one bar, and start the next bucket.
fn flush(out: &mut Vec<IntradayRow>, bucket: &mut Vec<&IntradayRow>) {
    let Some(first) = bucket.first().copied() else {
        return;
    };
    let last = bucket.last().copied().unwrap_or(first);
    out.push(IntradayRow {
        ts_utc: first.ts_utc,
        open: first.open,
        high: bucket
            .iter()
            .map(|row| row.high)
            .fold(f64::NEG_INFINITY, f64::max),
        low: bucket
            .iter()
            .map(|row| row.low)
            .fold(f64::INFINITY, f64::min),
        close: last.close,
        volume: bucket.iter().map(|row| row.volume).sum(),
    });
    bucket.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    fn row(day: u32, index: u32, price: f64) -> IntradayRow {
        // Bars at 5m intervals from 14:30 UTC, the 09:30 ET open in November.
        let minute_of_day = 14 * 60 + 30 + index * 5;
        IntradayRow {
            ts_utc: ts(day, minute_of_day / 60, minute_of_day % 60),
            open: price,
            high: price + 1.0,
            low: price - 1.0,
            close: price + 0.5,
            volume: 100,
        }
    }

    fn series(day: u32, count: u32) -> Vec<IntradayRow> {
        (0..count)
            .map(|index| row(day, index, 100.0 + f64::from(index)))
            .collect()
    }

    /// The UTC date, standing in for an exchange zone with no offset.
    fn utc_day(at: NaiveDateTime) -> NaiveDate {
        at.date()
    }

    #[test]
    fn the_canonical_names_and_aliases_resolve() {
        for (name, expected) in [
            ("5Min", Timeframe::FiveMin),
            ("5m", Timeframe::FiveMin),
            ("15MIN", Timeframe::FifteenMin),
            ("15m", Timeframe::FifteenMin),
            ("30Min", Timeframe::ThirtyMin),
            ("30m", Timeframe::ThirtyMin),
            ("1Hour", Timeframe::OneHour),
            ("1h", Timeframe::OneHour),
            ("4Hour", Timeframe::FourHour),
            ("4h", Timeframe::FourHour),
        ] {
            assert_eq!(Timeframe::parse(name), Some(expected), "{name}");
        }
        assert_eq!(Timeframe::parse("1Day"), None);
        assert_eq!(Timeframe::parse("1d"), None);
        assert_eq!(Timeframe::parse(""), None);
        for timeframe in TIMEFRAMES {
            assert_eq!(Timeframe::parse(timeframe.as_str()), Some(timeframe));
        }
    }

    #[test]
    fn the_bucket_sizes_are_the_stated_table() {
        assert_eq!(Timeframe::FiveMin.bars_per_bucket(), 1);
        assert_eq!(Timeframe::FifteenMin.bars_per_bucket(), 3);
        assert_eq!(Timeframe::ThirtyMin.bars_per_bucket(), 6);
        assert_eq!(Timeframe::OneHour.bars_per_bucket(), 12);
        assert_eq!(Timeframe::FourHour.bars_per_bucket(), 48);
        assert_eq!(Timeframe::FourHour.vendor_interval(), "5m");
    }

    #[test]
    fn the_window_scales_per_timeframe_and_is_capped() {
        assert_eq!(Timeframe::FiveMin.window_days(48), 48);
        assert_eq!(Timeframe::FiveMin.window_days(3), 7);
        assert_eq!(Timeframe::OneHour.window_days(48), 96);
        assert_eq!(Timeframe::OneHour.window_days(3), 14);
        assert_eq!(Timeframe::FourHour.window_days(48), 72);
        assert_eq!(Timeframe::FourHour.window_days(3), 30);
        assert_eq!(Timeframe::FourHour.window_days(10_000), VENDOR_MAX_WINDOW_DAYS);
        assert_eq!(Timeframe::FiveMin.window_days(0), 7);
    }

    #[test]
    fn the_native_read_covers_the_buckets_asked_for() {
        assert_eq!(Timeframe::FiveMin.native_rows_needed(48), 50);
        assert_eq!(Timeframe::FourHour.native_rows_needed(1), 144);
        // Five rules at once: never fewer bars than the buckets need.
        for timeframe in TIMEFRAMES {
            let per_bucket = i64::try_from(timeframe.bars_per_bucket()).unwrap();
            assert!(timeframe.native_rows_needed(45) >= 45 * per_bucket);
        }
    }

    #[test]
    fn five_minutes_keeps_every_native_bar() {
        let rows = series(10, 5);
        let bars = aggregate(&rows, Timeframe::FiveMin, utc_day);
        assert_eq!(bars, rows);
    }

    #[test]
    fn fifteen_minutes_takes_three_native_bars_per_bucket() {
        let rows = series(10, 7);
        let bars = aggregate(&rows, Timeframe::FifteenMin, utc_day);
        assert_eq!(bars.len(), 3);
        // The bucket keeps the first bar's opening instant.
        assert_eq!(bars[0].ts_utc, rows[0].ts_utc);
        assert_eq!(bars[1].ts_utc, rows[3].ts_utc);
        assert_eq!(bars[0].open, rows[0].open);
        assert_eq!(bars[0].close, rows[2].close);
        assert_eq!(bars[0].high, rows.iter().take(3).map(|r| r.high).fold(f64::MIN, f64::max));
        assert_eq!(bars[0].low, rows.iter().take(3).map(|r| r.low).fold(f64::MAX, f64::min));
        assert_eq!(bars[0].volume, 300);
    }

    #[test]
    fn a_short_final_bucket_is_kept() {
        let rows = series(10, 7);
        let bars = aggregate(&rows, Timeframe::FifteenMin, utc_day);
        assert_eq!(bars.len(), 3);
        // 7 = 3 + 3 + 1: the last bucket is one bar, not dropped.
        assert_eq!(bars[2].volume, 100);
        assert_eq!(bars[2].open, rows[6].open);
        assert_eq!(bars[2].close, rows[6].close);
    }

    #[test]
    fn four_hours_never_straddles_a_session_date() {
        // Two sessions of 60 bars each: 1.25 buckets apiece, so each session opens a bucket of
        // its own. A chunker that ignored the date would pair the first session's tail with the
        // second session's head.
        let mut rows = series(10, 60);
        rows.extend(series(11, 60));
        let bars = aggregate(&rows, Timeframe::FourHour, utc_day);

        assert_eq!(bars.len(), 4);
        assert_eq!(bars[0].ts_utc.date(), NaiveDate::from_ymd_opt(2026, 11, 10).unwrap());
        assert_eq!(bars[1].ts_utc.date(), NaiveDate::from_ymd_opt(2026, 11, 10).unwrap());
        assert_eq!(bars[2].ts_utc.date(), NaiveDate::from_ymd_opt(2026, 11, 11).unwrap());
        // The first bucket of the second session starts at that session's first bar.
        assert_eq!(bars[2].ts_utc, rows[60].ts_utc);
        // The last bucket holds the 12 bars left over from the second session, alone.
        assert_eq!(bars[3].volume, 12 * 100);
        for bar in &bars {
            assert!(bar.ts_utc.date() == NaiveDate::from_ymd_opt(2026, 11, 10).unwrap()
                || bar.ts_utc.date() == NaiveDate::from_ymd_opt(2026, 11, 11).unwrap());
        }
    }

    #[test]
    fn a_half_day_session_resets_the_buckets_of_the_next_one() {
        // A 13:00 close (half day) gives 42 five-minute bars, then a full 78-bar session.
        let mut rows = series(10, 42);
        rows.extend(series(11, 78));
        let bars = aggregate(&rows, Timeframe::FourHour, utc_day);
        // 42 -> one short bucket; 78 -> one full 48 and one short 30.
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].volume, 42 * 100);
        assert_eq!(bars[1].volume, 48 * 100);
        assert_eq!(bars[2].volume, 30 * 100);
        assert_eq!(bars[1].ts_utc, rows[42].ts_utc);
    }

    #[test]
    fn a_three_day_series_with_unequal_days_groups_by_day() {
        let mut rows = series(10, 6);
        rows.extend(series(11, 3));
        rows.extend(series(12, 9));
        let bars = aggregate(&rows, Timeframe::ThirtyMin, utc_day);
        // 6 -> 1 bucket, 3 -> 1, 9 -> 1 full + 1 short.
        assert_eq!(bars.len(), 4);
        assert_eq!(bars[0].volume, 600);
        assert_eq!(bars[1].volume, 300);
        assert_eq!(bars[2].volume, 600);
        assert_eq!(bars[3].volume, 300);
        let days: Vec<NaiveDate> = bars.iter().map(|bar| bar.ts_utc.date()).collect();
        assert_eq!(
            days,
            vec![
                NaiveDate::from_ymd_opt(2026, 11, 10).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 11).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 12).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 12).unwrap(),
            ]
        );
    }

    #[test]
    fn a_dst_transition_moves_the_session_rows_not_the_rule() {
        // 2026-03-08 is the US spring change. The caller's zone closure dates the rows; the
        // aggregation itself must not care that one session is one hour shorter.
        let mut rows = series(7, 78);
        rows.extend(series(8, 66));
        let eastern = chrono_tz::America::New_York;
        let bars = aggregate(&rows, Timeframe::FourHour, |at| {
            eastern.from_utc_datetime(&at).date_naive()
        });
        // 78 -> 48 + 30; 66 -> 48 + 18.
        assert_eq!(bars.len(), 4);
        assert_eq!(bars[1].volume, 30 * 100);
        assert_eq!(bars[3].volume, 18 * 100);
    }

    #[test]
    fn rows_out_of_order_are_sorted_before_chunking() {
        let mut rows = series(10, 4);
        rows.reverse();
        let bars = aggregate(&rows, Timeframe::FifteenMin, utc_day);
        assert_eq!(bars.len(), 2);
        assert!(bars[0].ts_utc < bars[1].ts_utc);
        // d is still the first bar's timestamp, so the open is the earliest bar's open.
        assert_eq!(bars[0].open, rows[3].open);
    }

    #[test]
    fn an_empty_series_aggregates_to_nothing() {
        assert!(aggregate(&[], Timeframe::FourHour, utc_day).is_empty());
    }
}
