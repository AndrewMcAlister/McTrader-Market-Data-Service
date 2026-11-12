//! Configuration, read once at startup and immutable thereafter.
//!
//! Everything a deployment is likely to change is an environment variable, because the two
//! things most likely to change are the ones that must not require a code edit: the database
//! and the EODHD key. The defaults are the safe ones the estate already uses: bind to
//! localhost, a short price TTL, and gap-inclusive true range for ATR.
//!
//! The key resolution order is deliberate: an explicit argument, then `EODHD_API_KEY`, then
//! the Windows keyring, then no key. The keyring is last so a one-off run can be told the key
//! without touching stored credentials, and so a test can never accidentally spend real
//! quota by inheriting one. Only the *source* is recorded; the key itself is never logged.

use std::fmt;

/// The OS keyring entry the EODHD key lives in. Shared with McTrader deliberately: two keys
/// would mean two quotas, two bills and no single place to measure usage.
pub const KEYRING_SERVICE: &str = "market_data:eodhd:global";
pub const KEYRING_USERNAME: &str = "eodhd:api_key";

pub const EODHD_BASE_URL: &str = "https://eodhd.com/api";

/// One price is reused for this long. Five minutes is what the estate already uses, and it
/// is short enough that a price is never more than one cycle stale at a 15-minute cadence.
pub const DEFAULT_PRICE_TTL_SECONDS: f64 = 300.0;

/// Above this many symbols in one request, the whole-market call is cheaper. The measured
/// tariff is 1 unit per symbol against 100 flat, so 100 is the exact break-even.
pub const DEFAULT_BULK_BREAKEVEN: i64 = 100;

/// How many symbols the scheduler prices one by one for an exchange with no whole-market
/// real-time feed — the ASX today. One vendor unit each.
pub const DEFAULT_SCHEDULE_PRICE_UNIVERSE: i64 = 100;

/// ATR needs 14 completed sessions plus one earlier close to give the first bar a previous
/// close. Fifteen is the floor; a request for fewer cannot produce ATR(14).
pub const DEFAULT_MIN_SESSIONS: i64 = 15;

/// How deep a vendor refresh asks for and keeps. `/eod/` bills one unit whatever the width
/// of the window, so the history past what a caller asked for is already paid for.
pub const DEFAULT_HISTORY_SESSIONS: i64 = 560;

/// How long the owned 5-minute tape answers an intraday request before the vendor is asked
/// again. Five minutes: an intraday bar is a moment, and the same figure as a price's TTL.
pub const DEFAULT_INTRADAY_TTL_SECONDS: f64 = 300.0;

/// How many days of 5-minute bars the owned table keeps.
///
/// Thirty days is ~936,000 rows at 200 symbols, which is the headroom rule's answer for a tape
/// whose only consumer reads the current session. A longer window is a backfill decision, not
/// an endpoint's.
pub const DEFAULT_INTRADAY_RETENTION_DAYS: i64 = 30;

/// The periods the consumer set actually reads.
pub const DEFAULT_ATR_PERIODS: [i64; 5] = [14, 20, 22, 26, 50];

pub const DEFAULT_ATR_PERIOD: i64 = 14;

/// Where the EODHD key was found. It is a fact an operator needs — a service running as
/// `NT AUTHORITY\SYSTEM` cannot see an interactive user's keyring — and never the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Explicit,
    Environment,
    Keyring,
    None,
}

impl KeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Environment => "environment",
            Self::Keyring => "keyring",
            Self::None => "none",
        }
    }
}

impl fmt::Display for KeySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A read of the machine's credential store.
///
/// Config depends on this trait rather than on a keyring implementation so the key
/// resolution order can be tested without touching a real Windows vault.
pub trait KeyringSource {
    /// The stored key, or `None` when no entry exists or the vault is unavailable.
    fn stored_key(&self) -> Option<String>;
}

/// A keyring that never has an entry, for deployments that use the environment only.
pub struct NoKeyring;

impl KeyringSource for NoKeyring {
    fn stored_key(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    // -- vendor ------------------------------------------------------------
    pub eodhd_base_url: String,
    pub eodhd_api_key: String,
    pub eodhd_key_source: KeySource,
    pub eodhd_timeout_seconds: f64,
    /// The whole-market response is ~2.3 MB; it gets a longer leash than a price.
    pub eodhd_bulk_timeout_seconds: f64,

    // -- database ----------------------------------------------------------
    pub db_server: String,
    pub db_database: String,
    /// Accepted for compatibility with the Python service, but unused under TDS: the Rust
    /// client speaks the protocol directly rather than through an ODBC driver.
    pub db_driver: String,
    pub db_trusted: bool,
    pub db_trust_server_certificate: bool,
    pub db_timeout_seconds: i64,
    /// SQL authentication credentials. Only consulted when `db_trusted` is false; the
    /// source service used Windows trusted auth exclusively, so these default to empty.
    /// They are never logged.
    pub db_user: String,
    pub db_password: String,

    // -- behaviour ---------------------------------------------------------
    pub price_ttl_seconds: f64,
    pub bulk_breakeven: i64,
    pub schedule_price_universe: i64,
    pub min_sessions: i64,
    pub history_sessions: i64,
    /// How long the owned 5-minute tape answers before `/bars/intraday` asks the vendor again.
    pub intraday_ttl_seconds: f64,
    /// How many days of 5-minute bars the owned table keeps.
    pub intraday_retention_days: i64,
    pub atr_periods: Vec<i64>,
    pub atr_period: i64,
    /// Gap-inclusive true range. `high_low` is available and is NOT the default, because
    /// every implementation in the estate uses true range.
    pub atr_method: String,
    /// `simple` is the mean of the last N true ranges, which is what both stop books use.
    pub atr_smoothing: String,

    // -- server ------------------------------------------------------------
    pub host: String,
    pub port: i64,
    pub service_name: String,

    // -- market clock ------------------------------------------------------
    /// The timezone a bare HH:MM reference time is read in when a caller does not say.
    pub clock_user_timezone: String,

    // -- the refresh schedule ----------------------------------------------
    pub schedule_offset_minutes: i64,
    pub schedule_retry_minutes: i64,
    pub schedule_enabled: bool,
}

impl Settings {
    /// Settings from the process environment, resolving the key last.
    pub fn from_env(keyring: &dyn KeyringSource) -> Self {
        let lookup = |name: &str| std::env::var(name).ok();
        Self::from_lookup(&lookup, None, keyring)
    }

    /// Settings from the process environment with an explicitly supplied key.
    pub fn from_env_with(explicit_key: Option<&str>, keyring: &dyn KeyringSource) -> Self {
        let lookup = |name: &str| std::env::var(name).ok();
        Self::from_lookup(&lookup, explicit_key, keyring)
    }

    /// Settings from an arbitrary variable lookup.
    ///
    /// The lookup abstraction exists so the exact parsing rules — trim, blank means default,
    /// invalid numbers fall back, only `0`/`false`/`False` are false — are testable without
    /// mutating the process environment, which is global and racy under parallel tests.
    pub fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
        explicit_key: Option<&str>,
        keyring: &dyn KeyringSource,
    ) -> Self {
        let periods = parse_atr_periods(&env_or(lookup, "MDS_ATR_PERIODS", ""));
        let env_key = lookup("EODHD_API_KEY");
        let (api_key, key_source) = resolve_api_key(explicit_key, env_key.as_deref(), keyring);

        Self {
            eodhd_base_url: env_or(lookup, "MDS_EODHD_BASE_URL", EODHD_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            eodhd_api_key: api_key,
            eodhd_key_source: key_source,
            eodhd_timeout_seconds: env_float(lookup, "MDS_EODHD_TIMEOUT", 30.0),
            eodhd_bulk_timeout_seconds: env_float(lookup, "MDS_EODHD_BULK_TIMEOUT", 60.0),
            db_server: env_or(lookup, "MDS_DB_SERVER", "localhost"),
            db_database: env_or(lookup, "MDS_DATABASE", "Market_Data"),
            db_driver: env_or(lookup, "MDS_DB_DRIVER", "ODBC Driver 17 for SQL Server"),
            db_trusted: env_bool(lookup, "MDS_DB_TRUSTED", true),
            db_trust_server_certificate: env_bool(lookup, "MDS_DB_TRUST_CERT", true),
            db_timeout_seconds: env_int(lookup, "MDS_DB_TIMEOUT", 30),
            db_user: env_or(lookup, "MDS_DB_USER", ""),
            db_password: env_or(lookup, "MDS_DB_PASSWORD", ""),
            price_ttl_seconds: env_float(
                lookup,
                "MDS_PRICE_TTL_SECONDS",
                DEFAULT_PRICE_TTL_SECONDS,
            ),
            bulk_breakeven: env_int(lookup, "MDS_BULK_BREAKEVEN", DEFAULT_BULK_BREAKEVEN),
            schedule_price_universe: env_int(
                lookup,
                "MDS_SCHEDULE_PRICE_UNIVERSE",
                DEFAULT_SCHEDULE_PRICE_UNIVERSE,
            ),
            min_sessions: env_int(lookup, "MDS_MIN_SESSIONS", DEFAULT_MIN_SESSIONS),
            history_sessions: env_int(lookup, "MDS_HISTORY_SESSIONS", DEFAULT_HISTORY_SESSIONS),
            intraday_ttl_seconds: env_float(
                lookup,
                "MDS_INTRADAY_TTL_SECONDS",
                DEFAULT_INTRADAY_TTL_SECONDS,
            ),
            intraday_retention_days: env_int(
                lookup,
                "MDS_INTRADAY_RETENTION_DAYS",
                DEFAULT_INTRADAY_RETENTION_DAYS,
            ),
            atr_periods: if periods.is_empty() {
                DEFAULT_ATR_PERIODS.to_vec()
            } else {
                periods
            },
            atr_period: env_int(lookup, "MDS_ATR_PERIOD", DEFAULT_ATR_PERIOD),
            atr_method: env_or(lookup, "MDS_ATR_METHOD", "tr").to_lowercase(),
            atr_smoothing: env_or(lookup, "MDS_ATR_SMOOTHING", "simple").to_lowercase(),
            host: env_or(lookup, "MDS_HOST", "127.0.0.1"),
            port: env_int(lookup, "MDS_PORT", 8003),
            service_name: "McTrader_Market_Data_Service".to_string(),
            clock_user_timezone: env_or(lookup, "MDS_CLOCK_USER_TIMEZONE", "Australia/Sydney"),
            schedule_offset_minutes: env_int(lookup, "MDS_SCHEDULE_OFFSET_MINUTES", 45),
            schedule_retry_minutes: env_int(lookup, "MDS_SCHEDULE_RETRY_MINUTES", 15),
            schedule_enabled: env_bool(lookup, "MDS_SCHEDULE_ENABLED", true),
        }
    }

    /// The database this process is pointed at, for logs and `/stats`.
    pub fn database_label(&self) -> String {
        format!("{}/{}", self.db_server, self.db_database)
    }
}

/// One environment value: trimmed, with blank treated as absent, else the default.
fn env_or(lookup: &dyn Fn(&str) -> Option<String>, name: &str, default: &str) -> String {
    match lookup(name) {
        Some(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => default.to_string(),
    }
}

/// A float environment value; an unparseable one falls back to the default.
fn env_float(lookup: &dyn Fn(&str) -> Option<String>, name: &str, default: f64) -> f64 {
    let raw = env_or(lookup, name, &default.to_string());
    raw.parse::<f64>().unwrap_or(default)
}

/// An integer environment value; an unparseable one falls back to the default.
fn env_int(lookup: &dyn Fn(&str) -> Option<String>, name: &str, default: i64) -> i64 {
    let raw = env_or(lookup, name, &default.to_string());
    raw.parse::<i64>().unwrap_or(default)
}

/// The source service's boolean rule: only `0`, `false` and `False` are false.
fn env_bool(lookup: &dyn Fn(&str) -> Option<String>, name: &str, default: bool) -> bool {
    let raw = env_or(lookup, name, if default { "1" } else { "0" });
    !matches!(raw.as_str(), "0" | "false" | "False")
}

/// `MDS_ATR_PERIODS`: semicolon or comma separated, digit-only chunks, otherwise default.
fn parse_atr_periods(raw: &str) -> Vec<i64> {
    raw.replace(';', ",")
        .split(',')
        .filter_map(|chunk| {
            let chunk = chunk.trim();
            if !chunk.is_empty() && chunk.chars().all(|character| character.is_ascii_digit()) {
                chunk.parse::<i64>().ok()
            } else {
                None
            }
        })
        .collect()
}

/// The key and where it came from — never the key itself in a log.
pub fn resolve_api_key(
    explicit: Option<&str>,
    environment: Option<&str>,
    keyring: &dyn KeyringSource,
) -> (String, KeySource) {
    if let Some(key) = explicit.map(str::trim).filter(|key| !key.is_empty()) {
        return (key.to_string(), KeySource::Explicit);
    }
    if let Some(key) = environment.map(str::trim).filter(|key| !key.is_empty()) {
        return (key.to_string(), KeySource::Environment);
    }
    if let Some(key) = keyring
        .stored_key()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    {
        return (key, KeySource::Keyring);
    }
    (String::new(), KeySource::None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapKeyring(Option<&'static str>);

    impl KeyringSource for MapKeyring {
        fn stored_key(&self) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    fn settings_with(pairs: &[(&str, &str)]) -> Settings {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let lookup = move |name: &str| map.get(name).cloned();
        Settings::from_lookup(&lookup, None, &NoKeyring)
    }

    #[test]
    fn defaults_match_the_source_service() {
        let settings = settings_with(&[]);
        assert_eq!(settings.eodhd_base_url, "https://eodhd.com/api");
        assert_eq!(settings.eodhd_timeout_seconds, 30.0);
        assert_eq!(settings.eodhd_bulk_timeout_seconds, 60.0);
        assert_eq!(settings.db_server, "localhost");
        assert_eq!(settings.db_database, "Market_Data");
        assert!(settings.db_trusted);
        assert!(settings.db_trust_server_certificate);
        assert_eq!(settings.price_ttl_seconds, 300.0);
        assert_eq!(settings.bulk_breakeven, 100);
        assert_eq!(settings.min_sessions, 15);
        assert_eq!(settings.history_sessions, 560);
        assert_eq!(settings.intraday_ttl_seconds, 300.0);
        assert_eq!(settings.intraday_retention_days, 30);
        assert_eq!(settings.atr_periods, vec![14, 20, 22, 26, 50]);
        assert_eq!(settings.atr_method, "tr");
        assert_eq!(settings.atr_smoothing, "simple");
        assert_eq!(settings.host, "127.0.0.1");
        assert_eq!(settings.port, 8003);
        assert_eq!(settings.clock_user_timezone, "Australia/Sydney");
        assert!(settings.schedule_enabled);
        assert_eq!(settings.eodhd_key_source, KeySource::None);
        assert_eq!(settings.eodhd_api_key, "");
    }

    #[test]
    fn only_the_three_documented_spellings_are_false() {
        for value in ["0", "false", "False"] {
            let settings = settings_with(&[("MDS_DB_TRUSTED", value)]);
            assert!(!settings.db_trusted, "{value} should be false");
        }
        for value in ["1", "true", "TRUE", "no", "off", "yes"] {
            let settings = settings_with(&[("MDS_DB_TRUSTED", value)]);
            assert!(settings.db_trusted, "{value} should be true");
        }
    }

    #[test]
    fn blank_and_whitespace_values_keep_the_default() {
        let settings = settings_with(&[("MDS_DB_SERVER", "   "), ("MDS_PORT", "")]);
        assert_eq!(settings.db_server, "localhost");
        assert_eq!(settings.port, 8003);
    }

    #[test]
    fn invalid_numbers_fall_back_to_the_default() {
        let settings = settings_with(&[
            ("MDS_EODHD_TIMEOUT", "soon"),
            ("MDS_PORT", "eight thousand"),
            ("MDS_PRICE_TTL_SECONDS", "NaN-ish"),
        ]);
        assert_eq!(settings.eodhd_timeout_seconds, 30.0);
        assert_eq!(settings.port, 8003);
        assert_eq!(settings.price_ttl_seconds, 300.0);
    }

    #[test]
    fn numeric_values_are_trimmed_and_parsed() {
        let settings = settings_with(&[
            ("MDS_EODHD_TIMEOUT", " 12.5 "),
            ("MDS_PORT", " 8013 "),
            ("MDS_ATR_PERIOD", "22"),
        ]);
        assert_eq!(settings.eodhd_timeout_seconds, 12.5);
        assert_eq!(settings.port, 8013);
        assert_eq!(settings.atr_period, 22);
    }

    #[test]
    fn atr_periods_accept_both_separators_and_keep_digit_chunks() {
        let settings = settings_with(&[("MDS_ATR_PERIODS", "14; 22,26, x,50,-1,")]);
        assert_eq!(settings.atr_periods, vec![14, 22, 26, 50]);
    }

    #[test]
    fn atr_periods_fall_back_when_nothing_parses() {
        let settings = settings_with(&[("MDS_ATR_PERIODS", "none")]);
        assert_eq!(settings.atr_periods, vec![14, 20, 22, 26, 50]);
    }

    #[test]
    fn atr_method_and_smoothing_are_lower_cased() {
        let settings = settings_with(&[
            ("MDS_ATR_METHOD", "HIGH_LOW"),
            ("MDS_ATR_SMOOTHING", "Wilder"),
        ]);
        assert_eq!(settings.atr_method, "high_low");
        assert_eq!(settings.atr_smoothing, "wilder");
    }

    #[test]
    fn the_intraday_settings_are_read_from_their_own_variables() {
        let settings = settings_with(&[
            ("MDS_INTRADAY_TTL_SECONDS", " 90 "),
            ("MDS_INTRADAY_RETENTION_DAYS", "7"),
        ]);
        assert_eq!(settings.intraday_ttl_seconds, 90.0);
        assert_eq!(settings.intraday_retention_days, 7);
    }

    #[test]
    fn base_url_sheds_every_trailing_slash() {
        let settings = settings_with(&[("MDS_EODHD_BASE_URL", "https://example.test/api///")]);
        assert_eq!(settings.eodhd_base_url, "https://example.test/api");
    }

    #[test]
    fn key_resolution_prefers_explicit_then_environment_then_keyring() {
        let keyring = MapKeyring(Some("keyring-key"));
        let (key, source) = resolve_api_key(Some("explicit-key"), Some("env-key"), &keyring);
        assert_eq!(
            (key.as_str(), source),
            ("explicit-key", KeySource::Explicit)
        );

        let (key, source) = resolve_api_key(None, Some(" env-key "), &keyring);
        assert_eq!((key.as_str(), source), ("env-key", KeySource::Environment));

        let (key, source) = resolve_api_key(Some("  "), None, &keyring);
        assert_eq!((key.as_str(), source), ("keyring-key", KeySource::Keyring));

        let (key, source) = resolve_api_key(None, None, &NoKeyring);
        assert_eq!((key.as_str(), source), ("", KeySource::None));
    }

    #[test]
    fn an_environment_key_is_never_replaced_by_the_keyring() {
        let keyring = MapKeyring(Some("keyring-key"));
        let (key, source) = resolve_api_key(None, Some("env-key"), &keyring);
        assert_eq!(key, "env-key");
        assert_eq!(source, KeySource::Environment);
    }

    #[test]
    fn database_label_names_both_parts() {
        let settings = settings_with(&[("MDS_DB_SERVER", "sqlhost"), ("MDS_DATABASE", "MD")]);
        assert_eq!(settings.database_label(), "sqlhost/MD");
    }
}
