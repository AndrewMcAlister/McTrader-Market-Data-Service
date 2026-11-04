//! Turning a caller's reference time into the instant the clock resolves against.
//!
//! Two vocabularies, deliberately different, exactly as the source service had them:
//!
//! * `/market/clock` and `/market/session` read a bare `HH:MM` as **UTC today**, because a
//!   feed's schedule is written in UTC and that is what an operator debugging it has in hand.
//! * `/trading_sessions` reads `HH:MM` in the **operator's** timezone (or the broker's), because
//!   it is a drop-in for a McTrader endpoint whose callers mean "14:30 Sydney".
//!
//! Conflating the two would shift every session answer by the host's offset; keeping them apart
//! is why this module exists rather than one shared parser.

use chrono::{DateTime, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::error::py_repr;

/// The instant a clock query names; `None` means "now".
pub fn clock_reference(
    time: Option<&str>,
    at: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, String> {
    if let Some(at) = at.filter(|value| !value.is_empty()) {
        return parse_instant(at).map(Some);
    }
    if let Some(time) = time.filter(|value| !value.is_empty()) {
        let (hour, minute) = parse_clock_time(time)?;
        let date = now.date_naive();
        return Ok(Some(
            Utc.from_utc_datetime(
                &date
                    .and_hms_opt(hour, minute, 0)
                    .ok_or_else(|| format!("time out of range: {}", py_repr(time)))?,
            ),
        ));
    }
    Ok(None)
}

/// A full ISO-8601 instant. `Z` is accepted exactly as Python's `fromisoformat` accepts it
/// after the source service replaced it with `+00:00`; a stamp with no offset is UTC.
fn parse_instant(text: &str) -> Result<DateTime<Utc>, String> {
    let stamp = text.trim().replace('Z', "+00:00");
    if let Ok(parsed) = DateTime::parse_from_rfc3339(&stamp) {
        return Ok(parsed.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(&stamp, format) {
            return Ok(Utc.from_utc_datetime(&parsed));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(&stamp, "%Y-%m-%d") {
        return Ok(Utc.from_utc_datetime(&date.and_time(NaiveTime::MIN)));
    }
    Err(format!("at must be ISO-8601, got {}", py_repr(text)))
}

/// `HH:MM` as UTC today, with the source service's two error sentences.
fn parse_clock_time(text: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() != 2 {
        return Err(format!("time must be HH:MM, got {}", py_repr(text)));
    }
    let hour = parts[0]
        .parse::<i64>()
        .map_err(|_| format!("time must be HH:MM, got {}", py_repr(text)))?;
    let minute = parts[1]
        .parse::<i64>()
        .map_err(|_| format!("time must be HH:MM, got {}", py_repr(text)))?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return Err(format!("time out of range: {}", py_repr(text)));
    }
    Ok((hour as u32, minute as u32))
}

/// `HH:MM` read in an operator or broker zone. The failure sentences are the ones the
/// `/trading_sessions` consumers already match on.
pub fn operator_time(text: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    let parsed = if parts.len() == 2 {
        parts[0]
            .parse::<i64>()
            .ok()
            .zip(parts[1].parse::<i64>().ok())
    } else {
        None
    };
    let Some((hour, minute)) = parsed else {
        return Err("Invalid time format. Use HH:MM (e.g. \"09:30\" or \"14:00\").".to_string());
    };
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return Err(format!(
            "Invalid time: {text}. Hour must be 0-23, minute 0-59."
        ));
    }
    Ok((hour as u32, minute as u32))
}

/// `hour:minute` on the zone's current local date, as UTC.
///
/// The same DST rule as the clock's own arithmetic: the earlier instant when an hour repeats,
/// and the UTC interpretation for a wall time a spring-forward skipped.
pub fn local_instant_in_zone(
    now: DateTime<Utc>,
    zone: Tz,
    hour: u32,
    minute: u32,
) -> DateTime<Utc> {
    let date = now.with_timezone(&zone).date_naive();
    let Some(naive) = date.and_hms_opt(hour, minute, 0) else {
        return now;
    };
    let local = match zone.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(earlier, _) => earlier,
        LocalResult::None => zone.from_utc_datetime(&naive),
    };
    local.with_timezone(&Utc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn a_full_instant_wins_over_a_bare_time() {
        let now = utc(2026, 11, 2, 12, 0);
        let parsed = clock_reference(Some("09:00"), Some("2026-11-01T14:30:00Z"), now)
            .unwrap()
            .unwrap();
        assert_eq!(parsed, utc(2026, 11, 1, 14, 30));
    }

    #[test]
    fn a_bare_time_is_read_in_utc_today() {
        let now = utc(2026, 11, 2, 12, 0);
        let parsed = clock_reference(Some("14:30"), None, now).unwrap().unwrap();
        assert_eq!(parsed, utc(2026, 11, 2, 14, 30));
    }

    #[test]
    fn an_offset_stamp_keeps_its_instant() {
        let now = utc(2026, 11, 2, 12, 0);
        let parsed = clock_reference(None, Some("2026-11-02T09:30:00+11:00"), now)
            .unwrap()
            .unwrap();
        assert_eq!(parsed, utc(2026, 11, 1, 22, 30));
    }

    #[test]
    fn the_refusal_sentences_match_the_source() {
        let now = utc(2026, 11, 2, 12, 0);
        assert_eq!(
            clock_reference(None, Some("yesterday"), now).unwrap_err(),
            "at must be ISO-8601, got 'yesterday'"
        );
        assert_eq!(
            clock_reference(Some("nine"), None, now).unwrap_err(),
            "time must be HH:MM, got 'nine'"
        );
        assert_eq!(
            clock_reference(Some("25:00"), None, now).unwrap_err(),
            "time out of range: '25:00'"
        );
        assert_eq!(
            operator_time("9").unwrap_err(),
            "Invalid time format. Use HH:MM (e.g. \"09:30\" or \"14:00\")."
        );
        assert_eq!(
            operator_time("09:75").unwrap_err(),
            "Invalid time: 09:75. Hour must be 0-23, minute 0-59."
        );
    }

    #[test]
    fn an_operator_time_is_read_in_the_named_zone() {
        // 14:30 Sydney in November is 03:30Z.
        let now = utc(2026, 11, 2, 0, 0);
        let zone: Tz = "Australia/Sydney".parse().unwrap();
        assert_eq!(
            local_instant_in_zone(now, zone, 14, 30),
            utc(2026, 11, 2, 3, 30)
        );
    }
}
