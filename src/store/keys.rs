//! Key rules, stated once.
//!
//! * A read that says nothing about its venue means US, which is what the source service meant
//!   and what the API defaults to. A *write* that says nothing is not a US row, it is an
//!   unknown one — defaulting it is exactly how an ASX session came to be filed as US in the
//!   first place. The database has no default on these columns either, so a writer that
//!   bypasses this module is refused rather than mis-filed.
//! * A symbol list is trimmed, upper-cased, and stripped of blanks before it reaches SQL.
//!   One rule for prices, bars and indicators, so a caller cannot get one answer for
//!   `" aapl "` on one endpoint and a miss on another.

use super::error::StoreError;

/// The market code for a read: trimmed and upper-cased, defaulting to `US`.
pub fn read_exchange_key(value: Option<&str>) -> String {
    normalise(value).unwrap_or_else(|| "US".to_string())
}

/// The market code for a write, or a refusal naming what the row forgot.
pub fn required_exchange_key(value: Option<&str>) -> Result<String, StoreError> {
    normalise(value).ok_or_else(|| {
        StoreError::invalid(
            "a row must state its venue (Exchange); refusing to file an unknown venue as US",
        )
    })
}

/// The symbols a query should ask for: trimmed, upper-cased, blanks dropped.
pub fn normalise_symbols(symbols: &[String]) -> Vec<String> {
    symbols
        .iter()
        .map(|symbol| symbol.trim().to_uppercase())
        .filter(|symbol| !symbol.is_empty())
        .collect()
}

fn normalise(value: Option<&str>) -> Option<String> {
    let text = value.unwrap_or("").trim().to_uppercase();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_defaults_to_us() {
        assert_eq!(read_exchange_key(None), "US");
        assert_eq!(read_exchange_key(Some("  ")), "US");
    }

    #[test]
    fn a_read_upper_cases_and_trims() {
        assert_eq!(read_exchange_key(Some(" au ")), "AU");
    }

    #[test]
    fn a_write_refuses_a_missing_venue_with_the_source_wording() {
        let error = required_exchange_key(None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid store row: a row must state its venue (Exchange); \
             refusing to file an unknown venue as US"
        );
    }

    #[test]
    fn a_write_keeps_a_stated_venue() {
        assert_eq!(required_exchange_key(Some("au")).unwrap(), "AU");
    }

    #[test]
    fn symbols_are_trimmed_upper_cased_and_blank_dropped() {
        let wanted = normalise_symbols(&[" aapl ".into(), "".into(), "  ".into(), "CBA.AU".into()]);
        assert_eq!(wanted, vec!["AAPL".to_string(), "CBA.AU".to_string()]);
    }
}
