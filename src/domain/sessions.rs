//! Which session has finished, for the exchange that was asked about.
//!
//! Every bar read is filtered so the session still in progress is never served: a partial
//! range would shrink a stop exactly when the morning is quiet. The filter used to compare the
//! session date against the **UTC** date, which is right for the one exchange it was written for
//! and wrong for every exchange that closes earlier in UTC terms.
//!
//! The ASX is the case that bites. Its session closes at 16:00 Sydney — 05:00 or 06:00 UTC —
//! and the UTC date then stays on that same day for a further 18 hours, so a session that had
//! been closed all afternoon was refused as "in progress" until local midnight.
//!
//! So the rule is stated once, per exchange: a session is complete once the clock at that exchange
//! has passed its close plus a settlement margin. Exchanges with no known close keep the old
//! answer — the *previous* UTC date — which can only ever be conservative.

use chrono::{DateTime, Days, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;

use crate::domain::asset_class::CRYPTO_MARKET;
use crate::domain::exchanges::normalise_exchange_code;

/// Exchange -> (IANA zone, regular close), measured against the exchange's own clock so a DST
/// change moves the session rather than the rule.
const EXCHANGE_CLOSE: &[(&str, &str, (u32, u32))] = &[
    ("US", "America/New_York", (16, 0)),
    ("AU", "Australia/Sydney", (16, 0)),
    ("L", "Europe/London", (16, 30)),
    ("TO", "America/Toronto", (16, 0)),
    ("HK", "Asia/Hong_Kong", (16, 0)),
    ("F", "Europe/Berlin", (17, 30)),
    ("PA", "Europe/Paris", (17, 30)),
];

/// How long after the close the day's bar is trusted.
///
/// Three hours is not invented: it is the wait the UTC-date rule already imposed on the US,
/// where midnight UTC falls three hours after the 16:00 ET close in summer and four in
/// winter. Writing it down keeps the US answer where it was while letting every exchange that
/// closes earlier in UTC terms move to its own.
pub const SETTLEMENT_HOURS: i64 = 3;

/// Assets that have no exchange and no close: their bars are UTC days, so the in-progress day is
/// the current UTC date.
const CRYPTO_CLASS: &str = "crypto";

/// The market an exchange names for session purposes, or `""` when unknown.
///
/// An unrecognised exchange is not an error here: it yields `""` and the caller falls back to
/// the UTC-date rule. Refusing the request is the exchange normaliser's job, on the path that is
/// about to price something.
pub fn market_code(exchange: Option<&str>, asset_class: Option<&str>) -> String {
    let kind = asset_class.unwrap_or("").trim().to_lowercase();
    if kind == CRYPTO_CLASS {
        return CRYPTO_MARKET.to_string();
    }
    normalise_exchange_code(exchange).unwrap_or_default()
}

/// The close rule for a market, or `None` when it has no known close.
fn exchange_close(market: &str) -> Option<(Tz, NaiveTime)> {
    let (_, zone_name, (hour, minute)) =
        EXCHANGE_CLOSE.iter().find(|(code, _, _)| *code == market)?;
    let zone: Tz = zone_name.parse().ok()?;
    let close = NaiveTime::from_hms_opt(*hour, *minute, 0)?;
    Some((zone, close))
}

/// The newest session date that has finished and settled at `market`.
///
/// A session dated *D* counts once the exchange's clock has reached close + [`SETTLEMENT_HOURS`]
/// on *D*. Before that, the newest completed session is the previous local date — a calendar
/// day, so a holiday simply has no rows and costs nothing.
///
/// A market with no known close answers with the *previous* UTC date, never today's: the
/// caller compares `session <= answer`, and today's UTC date is still trading somewhere.
pub fn latest_completed_session(market: Option<&str>, now: DateTime<Utc>) -> NaiveDate {
    let key = market.unwrap_or("").trim().to_uppercase();
    let Some((zone, close)) = exchange_close(&key) else {
        return now.date_naive() - Days::new(1);
    };

    let local = now.with_timezone(&zone);
    let local_date = local.date_naive();
    let settled = local_date
        .and_time(close)
        .and_local_timezone(zone)
        .single()
        .map(|close_at| close_at + chrono::Duration::hours(SETTLEMENT_HOURS));

    match settled {
        Some(settled_at) if local >= settled_at => local_date,
        // The close time is not in a DST gap for any exchange in the table, but if a future
        // zone database ever makes it one, answering conservatively is the safe failure.
        _ => local_date - Days::new(1),
    }
}

/// The exchange close table, for diagnostics and tests.
pub fn exchange_closes() -> impl Iterator<Item = (&'static str, &'static str, (u32, u32))> {
    EXCHANGE_CLOSE
        .iter()
        .map(|(code, zone, close)| (*code, *zone, *close))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        utc_at(year, month, day, hour, 0)
    }

    fn utc_at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn an_asx_session_closed_in_the_afternoon_is_complete_that_evening() {
        // 20:00 Sydney on the 29th is 09:00 UTC, still the 29th in UTC.
        assert_eq!(
            latest_completed_session(Some("AU"), utc(2026, 10, 29, 9)),
            NaiveDate::from_ymd_opt(2026, 10, 29).unwrap()
        );
    }

    #[test]
    fn an_asx_session_is_not_complete_the_moment_it_closes() {
        // 17:30 Sydney is 06:30 UTC: past the close, inside the settlement margin.
        assert_eq!(
            latest_completed_session(Some("AU"), utc(2026, 10, 29, 6)),
            NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
        );
    }

    #[test]
    fn the_settlement_boundary_is_exact_to_the_minute() {
        let close = chrono::NaiveDate::from_ymd_opt(2026, 10, 29)
            .unwrap()
            .and_hms_opt(16, 0, 0)
            .unwrap();
        let sydney: Tz = "Australia/Sydney".parse().unwrap();
        let close_at = sydney.from_local_datetime(&close).single().unwrap();
        let margin = chrono::Duration::hours(SETTLEMENT_HOURS);
        let just_before = (close_at + margin - chrono::Duration::minutes(1)).with_timezone(&Utc);
        let settled = (close_at + margin).with_timezone(&Utc);

        assert_eq!(
            latest_completed_session(Some("AU"), just_before),
            NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
        );
        assert_eq!(
            latest_completed_session(Some("AU"), settled),
            NaiveDate::from_ymd_opt(2026, 10, 29).unwrap()
        );
    }

    #[test]
    fn the_us_session_is_not_complete_at_the_open() {
        assert_eq!(
            latest_completed_session(Some("US"), utc(2026, 10, 29, 9)),
            NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
        );
    }

    #[test]
    fn the_us_session_is_complete_by_the_old_utc_midnight_boundary() {
        assert_eq!(
            latest_completed_session(Some("US"), utc(2026, 10, 30, 0)),
            NaiveDate::from_ymd_opt(2026, 10, 29).unwrap()
        );
    }

    #[test]
    fn a_dst_shift_moves_the_session_not_the_rule() {
        // New York on EST: 22:00 UTC is 17:00 local, still settling.
        assert_eq!(
            latest_completed_session(Some("US"), utc(2026, 1, 15, 22)),
            NaiveDate::from_ymd_opt(2026, 1, 14).unwrap()
        );
        // 00:00 UTC on the 16th is 19:00 local -- close + 3h.
        assert_eq!(
            latest_completed_session(Some("US"), utc(2026, 1, 16, 0)),
            NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()
        );
    }

    #[test]
    fn an_unknown_exchange_answers_with_the_previous_utc_date() {
        assert_eq!(
            latest_completed_session(Some("ZZ"), utc(2026, 10, 29, 9)),
            NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
        );
    }

    #[test]
    fn crypto_never_returns_the_day_in_progress() {
        assert_eq!(
            latest_completed_session(Some("CRYPTO"), utc_at(2026, 10, 29, 23, 59)),
            NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
        );
    }

    #[test]
    fn a_missing_market_is_treated_as_unknown() {
        assert_eq!(
            latest_completed_session(None, utc(2026, 10, 29, 9)),
            NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
        );
    }

    #[test]
    fn the_market_code_resolves_a_exchange_name() {
        assert_eq!(market_code(Some("ASX"), None), "AU");
        assert_eq!(market_code(Some("NASDAQ"), None), "US");
        assert_eq!(market_code(Some("us"), None), "US");
    }

    #[test]
    fn crypto_is_its_own_market_whatever_exchange_was_passed() {
        assert_eq!(market_code(Some("US"), Some("crypto")), "CRYPTO");
        assert_eq!(market_code(None, Some("CRYPTO")), "CRYPTO");
    }

    #[test]
    fn an_unknown_exchange_yields_no_market_rather_than_raising() {
        assert_eq!(market_code(Some("NOWHERE"), None), "");
        // A blank exchange is the house default, exactly as `normalise_exchange(None)`.
        assert_eq!(market_code(None, None), "US");
    }
}
