//! Is today's data in yet, for a country?
//!
//! The question a caller actually asks before it trades: "has this venue's data been
//! collected since the session opened?" — not "is the database reachable" and not "is there a
//! row". A row stamped before the open is yesterday's, however recently it was written.
//!
//! ```text
//! available = latest observed price for the venue >= the venue's most recent open
//! ```
//!
//! Consequences worth stating, because each is a case that would otherwise look like a bug:
//!
//! * Before the open, today's open is in the FUTURE, so yesterday's data is not "after the
//!   session open" and the answer is false. That is what a screener wants at 09:00 New York.
//! * On a Saturday the clock resolves the venue to Monday's session, so the answer is false
//!   all weekend — there is no "today" to have data for.
//! * A price fetched at 02:00 is not counted: it is stamped before the open, so it cannot
//!   make the answer true.

use std::fmt;

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};

use crate::error::{py_repr, MarketDataError};

/// Country code -> the venue code in `SymbolPriceCache` and the exchange code in the clock
/// reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Country {
    pub key: &'static str,
    pub venue: &'static str,
    pub exchange: &'static str,
}

pub const COUNTRIES: &[Country] = &[
    Country {
        key: "US",
        venue: "US",
        exchange: "NYSE",
    },
    Country {
        key: "AU",
        venue: "AU",
        exchange: "ASX",
    },
];

/// The answer, with the evidence that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub available: bool,
    pub country: String,
    pub venue: String,
    pub exchange: String,
    pub latest_utc: Option<DateTime<Utc>>,
    pub session_open_utc: Option<DateTime<Utc>>,
    pub session_close_utc: Option<DateTime<Utc>>,
    pub session_is_open: bool,
}

/// The bounded window an exchange's session covers.
///
/// Built by the market clock; availability only asks whether an observation landed after the
/// open. Keeping the type here means the rule and the data it reads live in one module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeSession {
    pub exchange: String,
    pub session_date: NaiveDate,
    pub open_utc: Option<DateTime<Utc>>,
    pub close_utc: Option<DateTime<Utc>>,
    pub is_open: bool,
}

/// An unsupported country code, carrying the text Python's `KeyError` would render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountryError {
    pub message: String,
}

impl fmt::Display for CountryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CountryError {}

/// The country a code names; `us`, `US`, ` au ` all work, anything else is an error.
pub fn country_key(country_code: &str) -> Result<&'static Country, CountryError> {
    let key = country_code.trim().to_uppercase();
    COUNTRIES
        .iter()
        .find(|country| country.key == key)
        .ok_or_else(|| CountryError {
            // Python's `str(KeyError(message))` is the repr of the message, quotes included.
            message: py_repr(&format!(
                "country code must be one of ['AU', 'US'], got {}",
                py_repr(country_code)
            )),
        })
}

/// A stored stamp as UTC.
///
/// The columns are `datetime2` and the service writes naive UTC into them, so a naive value
/// here is UTC that lost its label — NOT local time. Calling `astimezone` on it would read it
/// as the host's zone and shift every answer by the UTC offset: measured on this machine,
/// Sydney's +11 turned a 19:31Z observation into 08:31Z and would have made a correct "true"
/// look like "false" for eleven hours.
pub fn naive_as_utc(value: NaiveDateTime) -> DateTime<Utc> {
    Utc.from_utc_datetime(&value)
}

/// Decide the answer. Pure: the clock and the latest stamp come in as arguments.
pub fn check(
    country_code: &str,
    latest_utc: Option<NaiveDateTime>,
    session: &ExchangeSession,
) -> Result<Availability, CountryError> {
    let country = country_key(country_code)?;
    let latest = latest_utc.map(naive_as_utc);
    let available = match (latest, session.open_utc) {
        (Some(latest), Some(open)) => latest >= open,
        _ => false,
    };
    Ok(Availability {
        available,
        country: country.key.to_string(),
        venue: country.venue.to_string(),
        exchange: country.exchange.to_string(),
        latest_utc: latest,
        session_open_utc: session.open_utc,
        session_close_utc: session.close_utc,
        session_is_open: session.is_open,
    })
}

/// The API's refusal for an unsupported country. It is not the usual `{"error", "detail"}`
/// shape: the source endpoint answers a bare `{"error": <KeyError text>}`.
pub fn unknown_country_body(error: &CountryError) -> MarketDataError {
    MarketDataError::new(error.message.clone(), "", 400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn session(
        exchange: &str,
        open: DateTime<Utc>,
        close: DateTime<Utc>,
        is_open: bool,
    ) -> ExchangeSession {
        ExchangeSession {
            exchange: exchange.to_string(),
            session_date: open.date_naive(),
            open_utc: Some(open),
            close_utc: Some(close),
            is_open,
        }
    }

    #[test]
    fn the_country_code_is_case_insensitive_and_trimmed() {
        assert_eq!(country_key("us").unwrap().key, "US");
        assert_eq!(country_key(" AU ").unwrap().key, "AU");
        assert_eq!(country_key("uS").unwrap().key, "US");
    }

    #[test]
    fn an_unknown_country_is_a_caller_error() {
        let error = country_key("NZ").unwrap_err();
        assert!(error.message.contains("US"));
    }

    #[test]
    fn us_true_when_a_price_arrived_after_the_open() {
        // Monday 2 November: open 14:30Z, a price observed at 15:10Z is today's.
        let window = session(
            "NYSE",
            utc(2026, 11, 2, 14, 30),
            utc(2026, 11, 2, 21, 0),
            true,
        );
        let result = check("US", Some(utc(2026, 11, 2, 15, 10).naive_utc()), &window).unwrap();
        assert!(result.available);
        assert_eq!(result.session_open_utc, Some(utc(2026, 11, 2, 14, 30)));
        assert!(result.session_is_open);
    }

    #[test]
    fn us_false_before_the_open_even_with_fridays_prices() {
        let window = session(
            "NYSE",
            utc(2026, 11, 2, 14, 30),
            utc(2026, 11, 2, 21, 0),
            false,
        );
        let result = check("US", Some(utc(2026, 10, 30, 19, 31).naive_utc()), &window).unwrap();
        assert!(!result.available);
    }

    #[test]
    fn us_false_on_a_weekend_because_there_is_no_session_yet() {
        // On Saturday the clock resolves to Monday's open, which has not happened.
        let window = session(
            "NYSE",
            utc(2026, 11, 2, 14, 30),
            utc(2026, 11, 2, 21, 0),
            false,
        );
        let result = check("US", Some(utc(2026, 10, 30, 19, 31).naive_utc()), &window).unwrap();
        assert!(!result.available);
        assert!(!result.session_is_open);
    }

    #[test]
    fn an_out_of_hours_price_cannot_make_it_true() {
        let window = session(
            "NYSE",
            utc(2026, 11, 2, 14, 30),
            utc(2026, 11, 2, 21, 0),
            false,
        );
        let result = check("US", Some(utc(2026, 11, 2, 2, 0).naive_utc()), &window).unwrap();
        assert!(!result.available);
    }

    #[test]
    fn au_true_inside_the_sydney_session() {
        // ASX 10:00-16:00 Sydney in November is 23:00-05:00Z; 00:30Z is inside it.
        let window = session("ASX", utc(2026, 11, 1, 23, 0), utc(2026, 11, 2, 5, 0), true);
        let result = check("AU", Some(utc(2026, 11, 2, 0, 30).naive_utc()), &window).unwrap();
        assert!(result.available);
        assert_eq!(result.session_open_utc, Some(utc(2026, 11, 1, 23, 0)));
    }

    #[test]
    fn au_false_when_the_newest_price_predates_the_asx_open() {
        let window = session("ASX", utc(2026, 11, 1, 23, 0), utc(2026, 11, 2, 5, 0), true);
        let result = check("AU", Some(utc(2026, 11, 1, 22, 50).naive_utc()), &window).unwrap();
        assert!(!result.available);
    }

    #[test]
    fn no_stored_price_at_all_is_false_not_an_error() {
        let window = session(
            "NYSE",
            utc(2026, 11, 2, 14, 30),
            utc(2026, 11, 2, 21, 0),
            true,
        );
        let result = check("US", None, &window).unwrap();
        assert!(!result.available);
        assert!(result.latest_utc.is_none());
    }

    #[test]
    fn a_naive_stamp_is_utc_and_is_not_shifted_by_the_host_zone() {
        let window = session(
            "NYSE",
            utc(2026, 11, 2, 14, 30),
            utc(2026, 11, 2, 21, 0),
            true,
        );
        let naive = NaiveDate::from_ymd_opt(2026, 11, 2)
            .unwrap()
            .and_hms_opt(15, 10, 0)
            .unwrap();
        let result = check("US", Some(naive), &window).unwrap();
        assert!(result.available);
        assert_eq!(result.latest_utc, Some(utc(2026, 11, 2, 15, 10)));
    }

    /// The source suite asked the real clock, not a hand-built window. These are the same
    /// cases joined to [`crate::clock::MarketClock`], so a clock regression that moved an
    /// open would fail here as well as in the clock's own tests.
    #[test]
    fn the_clock_and_the_rule_agree_across_the_day() {
        use crate::clock::MarketClock;

        let clock = MarketClock::from_json(include_str!("../../data/clock.json")).unwrap();
        let window = |exchange: &str, now: DateTime<Utc>| {
            let row = clock
                .exchanges()
                .find(|row| row.code.as_deref() == Some(exchange))
                .expect("the exchange is in the reference");
            let resolved = clock.exchange_session(row, now, false);
            ExchangeSession {
                exchange: exchange.to_string(),
                session_date: resolved.open_utc.unwrap().date_naive(),
                open_utc: resolved.open_utc,
                close_utc: resolved.close_utc,
                is_open: resolved.is_open,
            }
        };
        let ask = |country: &str, exchange: &str, latest: DateTime<Utc>, now: DateTime<Utc>| {
            check(country, Some(latest.naive_utc()), &window(exchange, now)).unwrap()
        };

        // Monday 2 November: open 14:30Z, a price at 15:10Z is today's.
        let result = ask(
            "US",
            "NYSE",
            utc(2026, 11, 2, 15, 10),
            utc(2026, 11, 2, 15, 20),
        );
        assert!(result.available);
        assert_eq!(result.session_open_utc, Some(utc(2026, 11, 2, 14, 30)));
        assert!(result.session_is_open);

        // 12:00Z is before that open: Friday's data is not today's.
        assert!(
            !ask(
                "US",
                "NYSE",
                utc(2026, 10, 30, 19, 31),
                utc(2026, 11, 2, 12, 0)
            )
            .available
        );

        // The weekend has no session yet.
        let weekend = ask(
            "US",
            "NYSE",
            utc(2026, 10, 30, 19, 31),
            utc(2026, 10, 31, 12, 0),
        );
        assert!(!weekend.available);
        assert!(!weekend.session_is_open);

        // A quote taken at 02:00 Monday is stamped before the open.
        assert!(
            !ask(
                "US",
                "NYSE",
                utc(2026, 11, 2, 2, 0),
                utc(2026, 11, 2, 12, 0)
            )
            .available
        );

        // ASX 10:00-16:00 Sydney in November is 23:00-05:00Z; 00:30Z is inside it.
        let asx = ask("AU", "ASX", utc(2026, 11, 2, 0, 30), utc(2026, 11, 2, 1, 0));
        assert!(asx.available);
        assert_eq!(asx.session_open_utc, Some(utc(2026, 11, 1, 23, 0)));

        // The AU close rolls past UTC midnight, so the comparison is on instants.
        assert!(
            !ask(
                "AU",
                "ASX",
                utc(2026, 11, 1, 22, 50),
                utc(2026, 11, 2, 1, 0)
            )
            .available
        );
    }
}
