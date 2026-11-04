//! The market clock, and the process clock the policy layers read.
//!
//! # Two clocks, one answer
//!
//! The service answers "is this exchange open?" the same way McTrader does, because two books
//! disagreeing about whether a market is open is the failure this port exists to prevent:
//!
//! * **Clock A — the exchange clock.** `dbo.Exchanges` gives each exchange's local open and
//!   close, DST resolved in the exchange's own zone. A session is open when
//!   `open_utc <= now < close_utc`.
//! * **Clock B — the broker's synthetic 24-hour clock.** 00:00–23:59 UTC, with Saturday and
//!   Sunday skipped when the rule says the class does not trade weekends.
//!
//! Which clock applies is the broker–asset-class rule: `exchange_hours` selects A,
//! `synthetic_24` selects B, and anything else is always open. A rule that names no clock
//! does not pretend to a session.
//!
//! # The reference
//!
//! The exchange table is read from `Market_Data.dbo.Exchanges` when it is reachable;
//! `data/clock.json` holds the brokers, asset classes and rules, and is also the fallback
//! exchange table when the database is down. An explicit path is a fixture and is never
//! overridden by whatever the database holds today — the same rule the source service used.
//!
//! # Time zones
//!
//! Windows abbreviations ("EST", "AEST") and country codes ("US", "AU") are mapped to IANA
//! names *before* lookup. `chrono_tz::Tz::from_str("EST")` would otherwise pin the exchange to
//! a fixed −05:00 all year, which is an hour wrong for the eight months New York is on
//! daylight time.
//!
//! # The process clock
//!
//! Session selection, cache age and the bars refresh memo used to call `datetime.now()`
//! directly, which made every one of them untestable without monkeypatching a module global.
//! They take a [`Clock`] instead, so a test can pin the instant a request is answered at and
//! assert the session and age that follow from it. The vendor adapters still read the real
//! clock where they translate "today" for a provider; that is the provider's own observation
//! time, not policy.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Days, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc,
};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::domain::availability::ExchangeSession;
use crate::store::models::ExchangeRow;

/// The source of "now" for every policy decision.
pub trait Clock: Send + Sync {
    fn now_utc(&self) -> DateTime<Utc>;
}

/// The process clock, in UTC.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// The instant as the naive UTC value SQL Server stores and the service compares against.
pub fn now_naive(clock: &dyn Clock) -> NaiveDateTime {
    clock.now_utc().naive_utc()
}

/// A datetime in the source service's `isoformat()` shape.
///
/// Python emits `2026-10-28T14:03:00` with no fractional part when the microseconds are zero,
/// and six digits when they are not. The contract has callers parsing this string, so the
/// shape is reproduced rather than left to `Debug`.
pub fn iso_naive(value: NaiveDateTime) -> String {
    if value.and_utc().timestamp_subsec_nanos() == 0 {
        value.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        value.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

/// A UTC instant in Python's aware `isoformat()` shape, including the `+00:00` offset.
pub fn iso_utc(value: DateTime<Utc>) -> String {
    format!("{}+00:00", iso_naive(value.naive_utc()))
}

/// Seconds from `earlier` to `later`, with Python's `timedelta.total_seconds()` precision.
///
/// A negative answer means the stored timestamp is in the future; callers compare it against
/// a TTL, where a negative age is legitimately "fresh".
pub fn seconds_between(later: NaiveDateTime, earlier: NaiveDateTime) -> f64 {
    let delta = later.signed_duration_since(earlier);
    delta
        .num_microseconds()
        .map(|micros| micros as f64 / 1_000_000.0)
        .unwrap_or_else(|| delta.num_seconds() as f64)
}

// ---------------------------------------------------------------------------
// time zones
// ---------------------------------------------------------------------------

/// Abbreviations that appear in the reference data -> the IANA zone that has DST.
const TZ_ABBREV_TO_IANA: &[(&str, &str)] = &[
    ("AEST", "Australia/Sydney"),
    ("AEDT", "Australia/Sydney"),
    ("ACST", "Australia/Adelaide"),
    ("ACDT", "Australia/Adelaide"),
    ("AWST", "Australia/Perth"),
    ("EST", "America/New_York"),
    ("EDT", "America/New_York"),
    ("CST", "America/Chicago"),
    ("CDT", "America/Chicago"),
    ("MST", "America/Denver"),
    ("MDT", "America/Denver"),
    ("PST", "America/Los_Angeles"),
    ("PDT", "America/Los_Angeles"),
    ("GMT", "UTC"),
    ("BST", "Europe/London"),
    ("CET", "Europe/Paris"),
    ("CEST", "Europe/Paris"),
    ("EET", "Europe/Helsinki"),
    ("EEST", "Europe/Helsinki"),
    ("JST", "Asia/Tokyo"),
    ("HKT", "Asia/Hong_Kong"),
    ("SGT", "Asia/Singapore"),
    ("NZST", "Pacific/Auckland"),
    ("NZDT", "Pacific/Auckland"),
];

/// Country codes and aliases that are not IANA names either.
const COUNTRY_ALIAS_TO_IANA: &[(&str, &str)] = &[
    ("AU", "Australia/Sydney"),
    ("US", "America/New_York"),
    ("UK", "Europe/London"),
    ("GB", "Europe/London"),
    ("NZ", "Pacific/Auckland"),
    ("JP", "Asia/Tokyo"),
    ("HK", "Asia/Hong_Kong"),
    ("SG", "Asia/Singapore"),
    ("DE", "Europe/Berlin"),
    ("FR", "Europe/Paris"),
];

/// A timezone label -> a zone that knows about daylight saving.
///
/// Abbreviations and country codes are mapped first. A blank label is UTC; an unmapped label
/// is tried as an IANA name; anything unresolvable degrades to UTC rather than failing the
/// service. (The source service logs that fallback; here the caller may log it once.)
pub fn resolve_time_zone(label: Option<&str>) -> Tz {
    let text = label.unwrap_or("").trim();
    if text.is_empty() {
        return Tz::UTC;
    }
    let upper = text.to_uppercase();
    for table in [TZ_ABBREV_TO_IANA, COUNTRY_ALIAS_TO_IANA] {
        if let Some((_, iana)) = table
            .iter()
            .find(|(abbreviation, _)| *abbreviation == upper)
        {
            if let Ok(zone) = Tz::from_str(iana) {
                return zone;
            }
        }
    }
    Tz::from_str(text).unwrap_or(Tz::UTC)
}

/// The label the clock answers with, distinguishing an unresolved zone from UTC.
fn zone_label(label: Option<&str>) -> Option<String> {
    label.map(str::to_string)
}

// ---------------------------------------------------------------------------
// the resolved session
// ---------------------------------------------------------------------------

/// The answer for one clock, at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSession {
    pub is_open: bool,
    pub open_utc: Option<DateTime<Utc>>,
    pub close_utc: Option<DateTime<Utc>>,
    pub next_open_utc: Option<DateTime<Utc>>,
    pub next_close_utc: Option<DateTime<Utc>>,
    pub current_time_utc: DateTime<Utc>,
    pub clock_source: &'static str,
    pub exchange_code: Option<String>,
    pub exchange_timezone: Option<String>,
    pub broker_id: Option<i32>,
    pub asset_class_id: Option<i32>,
}

impl ResolvedSession {
    pub fn new(is_open: bool, ref_utc: DateTime<Utc>, clock_source: &'static str) -> Self {
        Self {
            is_open,
            open_utc: None,
            close_utc: None,
            next_open_utc: None,
            next_close_utc: None,
            current_time_utc: ref_utc,
            clock_source,
            exchange_code: None,
            exchange_timezone: None,
            broker_id: None,
            asset_class_id: None,
        }
    }

    /// The payload `as_dict` produced: the REST shape, field for field.
    pub fn as_json(&self) -> Value {
        let iso = |value: Option<DateTime<Utc>>| value.map(iso_utc);
        json!({
            "is_open": self.is_open,
            "open_utc": iso(self.open_utc),
            "close_utc": iso(self.close_utc),
            "next_open_utc": iso(self.next_open_utc),
            "next_close_utc": iso(self.next_close_utc),
            "current_time_utc": iso_utc(self.current_time_utc),
            "clock_source": self.clock_source,
            "exchange_code": self.exchange_code,
            "exchange_timezone": self.exchange_timezone,
            "broker_id": self.broker_id,
            "asset_class_id": self.asset_class_id,
        })
    }
}

// ---------------------------------------------------------------------------
// the reference
// ---------------------------------------------------------------------------

/// One broker row from `data/clock.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct BrokerRow {
    pub id: i32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub active: Option<bool>,
}

/// One asset-class row from `data/clock.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct AssetClassRow {
    pub id: i32,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct BrokerExchangeRow {
    broker_id: i32,
    exchange_id: i32,
    #[serde(default = "default_true")]
    active: bool,
}

fn default_true() -> bool {
    true
}

/// One broker–asset-class rule.
#[derive(Debug, Clone, Deserialize)]
pub struct BrokerAssetClassRule {
    pub broker_id: i32,
    pub asset_class_id: i32,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub exchange_hours: bool,
    #[serde(default)]
    pub synthetic_24: bool,
    #[serde(default)]
    pub weekend: bool,
}

#[derive(Debug, Deserialize)]
struct RawExchange {
    id: i32,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    opens: Option<String>,
    #[serde(default)]
    closes: Option<String>,
}

impl RawExchange {
    fn into_row(self) -> ExchangeRow {
        ExchangeRow {
            id: self.id,
            code: self.code,
            name: self.name,
            country: self.country,
            currency: self.currency.map(|value| value.trim().to_string()),
            timezone: self.timezone,
            opens: self.opens,
            closes: self.closes,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReferenceFile {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    brokers: Vec<BrokerRow>,
    #[serde(default)]
    exchanges: Vec<RawExchange>,
    #[serde(default)]
    asset_classes: Vec<AssetClassRow>,
    #[serde(default)]
    broker_exchanges: Vec<BrokerExchangeRow>,
    #[serde(default)]
    broker_asset_classes: Vec<BrokerAssetClassRule>,
}

/// Why a clock reference could not be loaded.
#[derive(Debug)]
pub enum ClockLoadError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for ClockLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Json(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ClockLoadError {}

impl From<std::io::Error> for ClockLoadError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ClockLoadError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// A broker lookup failure, carrying the text the API renders as a 404.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownBroker {
    pub message: String,
}

impl fmt::Display for UnknownBroker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UnknownBroker {}

// ---------------------------------------------------------------------------
// the clock
// ---------------------------------------------------------------------------

/// Resolve sessions from the exchange table and the rule reference.
#[derive(Debug, Clone)]
pub struct MarketClock {
    source: String,
    brokers: BTreeMap<i32, BrokerRow>,
    exchanges: BTreeMap<i32, ExchangeRow>,
    asset_classes: BTreeMap<i32, AssetClassRow>,
    broker_exchanges: BTreeMap<i32, Vec<i32>>,
    broker_asset_classes: BTreeMap<(i32, i32), BrokerAssetClassRule>,
}

impl MarketClock {
    /// Load a fixture. The path is explicit, so the database is deliberately not consulted.
    pub fn from_file(path: &Path) -> Result<Self, ClockLoadError> {
        let text = std::fs::read_to_string(path)?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("clock.json")
            .to_string();
        Self::from_text(&text, filename)
    }

    /// Load the service reference, replacing the frozen exchange table when the database
    /// answered with rows. An empty database answer is the fallback case, not an override.
    pub fn load(
        path: &Path,
        exchange_rows: Option<Vec<ExchangeRow>>,
    ) -> Result<Self, ClockLoadError> {
        let text = std::fs::read_to_string(path)?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("clock.json")
            .to_string();
        let reference: ReferenceFile = serde_json::from_str(&text)?;
        let document_source = reference.source.clone();
        let mut clock = Self::from_reference(reference, String::new());
        match exchange_rows.filter(|rows| !rows.is_empty()) {
            Some(rows) => {
                let count = rows.len();
                clock.exchanges = rows.into_iter().map(|row| (row.id, row)).collect();
                clock.source = format!(
                    "Market_Data.dbo.Exchanges ({count} rows) + {filename} (brokers/rules)"
                );
            }
            None => clock.source = document_source.unwrap_or(filename),
        }
        Ok(clock)
    }

    /// Parse a reference document. The source label is the document's own, else "injected".
    pub fn from_json(text: &str) -> Result<Self, ClockLoadError> {
        Self::from_text(text, "injected".to_string())
    }

    fn from_text(text: &str, fallback_source: String) -> Result<Self, ClockLoadError> {
        let reference: ReferenceFile = serde_json::from_str(text)?;
        let source = reference.source.clone().unwrap_or(fallback_source);
        Ok(Self::from_reference(reference, source))
    }

    fn from_reference(reference: ReferenceFile, source: String) -> Self {
        let brokers = reference
            .brokers
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        let exchanges = reference
            .exchanges
            .into_iter()
            .map(|row| {
                let row = row.into_row();
                (row.id, row)
            })
            .collect();
        let asset_classes = reference
            .asset_classes
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        let mut broker_exchanges: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
        for row in reference.broker_exchanges {
            if row.active {
                broker_exchanges
                    .entry(row.broker_id)
                    .or_default()
                    .push(row.exchange_id);
            }
        }
        let broker_asset_classes = reference
            .broker_asset_classes
            .into_iter()
            .map(|row| ((row.broker_id, row.asset_class_id), row))
            .collect();
        Self {
            source,
            brokers,
            exchanges,
            asset_classes,
            broker_exchanges,
            broker_asset_classes,
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn exchanges(&self) -> impl Iterator<Item = &ExchangeRow> {
        self.exchanges.values()
    }

    pub fn brokers(&self) -> impl Iterator<Item = &BrokerRow> {
        self.brokers.values()
    }

    pub fn brokers_len(&self) -> usize {
        self.brokers.len()
    }

    pub fn exchanges_len(&self) -> usize {
        self.exchanges.len()
    }

    pub fn asset_classes_len(&self) -> usize {
        self.asset_classes.len()
    }

    pub fn rules_len(&self) -> usize {
        self.broker_asset_classes.len()
    }

    pub fn broker_exchange_counts(&self) -> BTreeMap<i32, usize> {
        self.broker_exchanges
            .iter()
            .map(|(broker, exchanges)| (*broker, exchanges.len()))
            .collect()
    }

    pub fn broker_exchange_ids(&self, broker_id: i32) -> &[i32] {
        self.broker_exchanges
            .get(&broker_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn broker_rules(&self) -> impl Iterator<Item = &BrokerAssetClassRule> {
        self.broker_asset_classes.values()
    }

    pub fn asset_classes(&self) -> impl Iterator<Item = &AssetClassRow> {
        self.asset_classes.values()
    }

    pub fn exchange(&self, id: i32) -> Option<&ExchangeRow> {
        self.exchanges.get(&id)
    }

    /// `"shares"` -> the asset-class id, case-insensitively.
    pub fn asset_class_id(&self, name: &str) -> Option<i32> {
        let wanted = name.trim().to_lowercase();
        self.asset_classes
            .iter()
            .find(|(_, row)| {
                row.name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase() == wanted)
            })
            .map(|(id, _)| *id)
    }

    pub fn describe(&self) -> Value {
        json!({
            "source": self.source,
            "brokers": self.brokers.len(),
            "exchanges": self.exchanges.len(),
            "asset_classes": self.asset_classes.len(),
            "broker_exchanges": self.broker_exchange_counts(),
            "rules": self.broker_asset_classes.len(),
        })
    }
}

// ---------------------------------------------------------------------------
// session arithmetic
// ---------------------------------------------------------------------------

/// `'HH:MM'` or `'HH:MM:SS'` -> a time of day, or `None` when it is not one.
fn parse_hhmm(text: Option<&str>) -> Option<NaiveTime> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let hour: u32 = parts[0].parse().ok()?;
    let minute: u32 = parts[1].parse().ok()?;
    let second: u32 = parts.get(2).map_or(Some(0), |value| value.parse().ok())?;
    NaiveTime::from_hms_opt(hour, minute, second)
}

fn is_weekend(date: NaiveDate) -> bool {
    date.weekday().num_days_from_monday() >= 5
}

fn skip_weekend(mut date: NaiveDate) -> NaiveDate {
    while is_weekend(date) {
        date = date + Days::new(1);
    }
    date
}

/// A wall-clock time on a date in a zone, choosing the earlier instant when the hour repeats
/// and degrading to the UTC interpretation for a wall time a DST jump skipped. The exchanges in
/// the reference open at 09:30 and 10:00, neither of which is ever skipped.
fn local_at(date: NaiveDate, time: NaiveTime, zone: Tz) -> DateTime<Tz> {
    let naive = date.and_time(time);
    match zone.from_local_datetime(&naive) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(earlier, _) => earlier,
        LocalResult::None => zone.from_utc_datetime(&naive),
    }
}

impl MarketClock {
    /// Clock A: the exchange's own hours, in the exchange's own timezone.
    pub fn exchange_session(
        &self,
        exchange: &ExchangeRow,
        ref_utc: DateTime<Utc>,
        weekend: bool,
    ) -> ResolvedSession {
        let zone = resolve_time_zone(exchange.timezone.as_deref());
        let opens = parse_hhmm(exchange.opens.as_deref());
        let closes = parse_hhmm(exchange.closes.as_deref());
        let label = zone_label(exchange.timezone.as_deref());

        let (Some(opens), Some(closes)) = (opens, closes) else {
            // No stated hours is not an open market; McTrader treats it as always open, and
            // the answer is kept identical so the two books cannot disagree.
            let mut session = ResolvedSession::new(true, ref_utc, "always_open");
            session.exchange_code = exchange.code.clone();
            session.exchange_timezone = label;
            return session;
        };

        let local_now = ref_utc.with_timezone(&zone);

        if !weekend && is_weekend(local_now.date_naive()) {
            // Project the next open onto Monday, keeping the exchange's wall-clock open time.
            let weekday = local_now.weekday().num_days_from_monday();
            let days_until_monday = u64::from(7 - weekday);
            let next_date = local_now.date_naive() + Days::new(days_until_monday);
            let next_open_local = Some(local_at(next_date, opens, zone));
            let mut next_close_local = local_at(next_date, closes, zone);
            if next_close_local <= local_at(next_date, opens, zone) {
                next_close_local = local_at(next_date + Days::new(1), closes, zone);
            }
            let next_open_utc = next_open_local.map(|value| value.with_timezone(&Utc));
            let next_close_utc = next_close_local.with_timezone(&Utc);
            return ResolvedSession {
                is_open: false,
                open_utc: next_open_utc,
                close_utc: next_open_utc,
                next_open_utc,
                next_close_utc: Some(next_close_utc),
                current_time_utc: ref_utc,
                clock_source: "exchange",
                exchange_code: exchange.code.clone(),
                exchange_timezone: label,
                broker_id: None,
                asset_class_id: None,
            };
        }

        let today = local_now.date_naive();
        let today_open_local = local_at(today, opens, zone);
        let mut today_close_local = local_at(today, closes, zone);
        if today_close_local <= today_open_local {
            today_close_local = local_at(today + Days::new(1), closes, zone);
        }
        let today_open_utc = today_open_local.with_timezone(&Utc);
        let today_close_utc = today_close_local.with_timezone(&Utc);
        let is_open = today_open_utc <= ref_utc && ref_utc < today_close_utc;

        let (next_open_utc, next_close_utc) = if ref_utc >= today_close_utc {
            let next_open_local = skip_weekend(today + Days::new(1));
            let mut next_close_local = local_at(next_open_local, closes, zone);
            if next_close_local <= local_at(next_open_local, opens, zone) {
                next_close_local = local_at(next_open_local + Days::new(1), closes, zone);
            }
            (
                local_at(next_open_local, opens, zone).with_timezone(&Utc),
                next_close_local.with_timezone(&Utc),
            )
        } else if ref_utc < today_open_utc {
            (today_open_utc, today_close_utc)
        } else {
            let next_open_local = skip_weekend(today + Days::new(1));
            (
                local_at(next_open_local, opens, zone).with_timezone(&Utc),
                today_close_utc,
            )
        };

        ResolvedSession {
            is_open,
            open_utc: Some(today_open_utc),
            close_utc: Some(today_close_utc),
            next_open_utc: Some(next_open_utc),
            next_close_utc: Some(next_close_utc),
            current_time_utc: ref_utc,
            clock_source: "exchange",
            exchange_code: exchange.code.clone(),
            exchange_timezone: label,
            broker_id: None,
            asset_class_id: None,
        }
    }

    /// The availability rule's window for an exchange, read from this reference.
    ///
    /// This is the only bridge between the reference data and
    /// [`crate::domain::availability`]: the rule is pure and takes an
    /// [`ExchangeSession`], while resolving one from an exchange row is the clock's job.
    /// `None` means the exchange is not in the reference, which the callers report as a
    /// missing-exchange error rather than as "not available".
    pub fn availability_session(
        &self,
        exchange_code: &str,
        now: DateTime<Utc>,
    ) -> Option<ExchangeSession> {
        let row = self
            .exchanges()
            .find(|row| row.code.as_deref() == Some(exchange_code))?;
        let resolved = self.exchange_session(row, now, false);
        Some(ExchangeSession {
            exchange: exchange_code.to_string(),
            session_date: resolved
                .open_utc
                .map(|open| open.date_naive())
                .unwrap_or_else(|| now.date_naive()),
            open_utc: resolved.open_utc,
            close_utc: resolved.close_utc,
            is_open: resolved.is_open,
        })
    }

    /// Clock B: the broker's 00:00–23:59 UTC clock, optionally weekends off.
    pub fn synthetic_session(
        &self,
        ref_utc: DateTime<Utc>,
        weekend: bool,
        broker_id: Option<i32>,
        asset_class_id: Option<i32>,
    ) -> ResolvedSession {
        let midnight =
            |date: NaiveDate| local_at(date, NaiveTime::MIN, Tz::UTC).with_timezone(&Utc);
        // 23:59:59.999999, the source service's last microsecond of the UTC day.
        let day = chrono::Duration::days(1) - chrono::Duration::microseconds(1);
        let today = ref_utc.date_naive();

        let mut session = if !weekend && is_weekend(today) {
            let next_open = midnight(skip_weekend(today));
            let mut session = ResolvedSession::new(false, ref_utc, "synthetic24");
            session.open_utc = Some(next_open);
            session.close_utc = Some(next_open + day);
            session.next_open_utc = Some(next_open);
            session.next_close_utc = Some(next_open + day);
            session
        } else {
            let open = midnight(today);
            let next_date = if weekend {
                today + Days::new(1)
            } else {
                skip_weekend(today + Days::new(1))
            };
            let next_open = midnight(next_date);
            let mut session = ResolvedSession::new(true, ref_utc, "synthetic24");
            session.open_utc = Some(open);
            session.close_utc = Some(open + day);
            session.next_open_utc = Some(next_open);
            session.next_close_utc = Some(next_open + day);
            session
        };
        session.broker_id = broker_id;
        session.asset_class_id = asset_class_id;
        session
    }

    /// One broker–asset-class pair, through the same rule McTrader applies.
    ///
    /// When the exchange clock applies and the broker maps several exchanges, the first active
    /// one is used. They are the same exchange class in practice (Alpaca: NYSE, NASDAQ, ARCA,
    /// AMEX, IEX, all 09:30–16:00 New York; IBKR: ASX and NSX, both 10:00–16:00 Sydney).
    pub fn resolve(
        &self,
        broker_id: i32,
        asset_class_id: i32,
        ref_utc: DateTime<Utc>,
    ) -> ResolvedSession {
        let Some(rule) = self.broker_asset_classes.get(&(broker_id, asset_class_id)) else {
            let mut session = ResolvedSession::new(false, ref_utc, "unknown");
            session.broker_id = Some(broker_id);
            session.asset_class_id = Some(asset_class_id);
            return session;
        };

        if rule.exchange_hours {
            if let Some(exchange_id) = self
                .broker_exchanges
                .get(&broker_id)
                .and_then(|ids| ids.first())
            {
                if let Some(exchange) = self.exchanges.get(exchange_id) {
                    let mut session = self.exchange_session(exchange, ref_utc, rule.weekend);
                    session.broker_id = Some(broker_id);
                    session.asset_class_id = Some(asset_class_id);
                    return session;
                }
            }
            let mut session = ResolvedSession::new(true, ref_utc, "always_open");
            session.broker_id = Some(broker_id);
            session.asset_class_id = Some(asset_class_id);
            return session;
        }
        if rule.synthetic_24 {
            return self.synthetic_session(
                ref_utc,
                rule.weekend,
                Some(broker_id),
                Some(asset_class_id),
            );
        }
        let mut session = ResolvedSession::new(true, ref_utc, "always_open");
        session.broker_id = Some(broker_id);
        session.asset_class_id = Some(asset_class_id);
        session
    }

    /// The payload McTrader's `GET /trading_sessions` returns, so the swap is one URL.
    pub fn broker_clock(
        &self,
        broker_id: i32,
        ref_utc: Option<DateTime<Utc>>,
    ) -> Result<Value, UnknownBroker> {
        let ref_utc = ref_utc.unwrap_or_else(Utc::now);
        let broker = self.brokers.get(&broker_id).ok_or_else(|| UnknownBroker {
            message: format!("broker {broker_id} is not in the clock reference"),
        })?;
        let broker_zone = resolve_time_zone(broker.timezone.as_deref());

        let exchanges: Vec<Value> = self
            .broker_exchanges
            .get(&broker_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter_map(|exchange_id| {
                self.exchanges
                    .get(exchange_id)
                    .map(|row| (*exchange_id, row))
            })
            .map(|(exchange_id, exchange)| {
                let session = self.exchange_session(exchange, ref_utc, false);
                let (opens_utc, closes_utc) = self.exchange_hours_utc(exchange, ref_utc);
                json!({
                    "id": exchange_id,
                    "code": exchange.code,
                    "name": exchange.name,
                    "timezone": exchange.timezone,
                    "opens": exchange.opens,
                    "closes": exchange.closes,
                    "opens_utc": opens_utc,
                    "closes_utc": closes_utc,
                    "is_open": session.is_open,
                    "clock_source": session.clock_source,
                    "next_open_utc": session.next_open_utc.map(iso_utc),
                    "next_close_utc": session.next_close_utc.map(iso_utc),
                })
            })
            .collect();

        Ok(json!({
            "success": true,
            "broker": {
                "id": broker_id,
                "name": broker.name,
                "timezone": broker.timezone,
                "is_active": broker.active,
            },
            "reference_time_utc": iso_utc(ref_utc),
            "reference_time": ref_utc.format("%H:%M").to_string(),
            "reference_time_broker": ref_utc.with_timezone(&broker_zone).format("%H:%M").to_string(),
            "reference_time_broker_tz": broker.timezone,
            "timestamp": iso_utc(Utc::now()),
            "exchanges": exchanges,
            "reference_source": self.source,
        }))
    }

    /// `(opens, closes)` as UTC `HH:MM` for the reference day, with the close rolled when the
    /// exchange's close is after midnight in UTC.
    fn exchange_hours_utc(
        &self,
        exchange: &ExchangeRow,
        ref_utc: DateTime<Utc>,
    ) -> (Option<String>, Option<String>) {
        let (Some(opens), Some(closes)) = (
            parse_hhmm(exchange.opens.as_deref()),
            parse_hhmm(exchange.closes.as_deref()),
        ) else {
            return (None, None);
        };
        let zone = resolve_time_zone(exchange.timezone.as_deref());
        let local = ref_utc.with_timezone(&zone);
        let open_local = local_at(local.date_naive(), opens, zone);
        let mut close_local = local_at(local.date_naive(), closes, zone);
        if close_local <= open_local {
            close_local = local_at(local.date_naive() + Days::new(1), closes, zone);
        }
        (
            Some(open_local.with_timezone(&Utc).format("%H:%M").to_string()),
            Some(close_local.with_timezone(&Utc).format("%H:%M").to_string()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Offset, TimeZone, Timelike};

    struct Fixed(DateTime<Utc>);

    impl Clock for Fixed {
        fn now_utc(&self) -> DateTime<Utc> {
            self.0
        }
    }

    fn instant(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Fixed {
        Fixed(
            Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
                .single()
                .unwrap(),
        )
    }

    #[test]
    fn a_fixed_clock_is_read_as_naive_utc() {
        let clock = instant(2026, 10, 28, 23, 30, 0);
        assert_eq!(
            now_naive(&clock),
            NaiveDateTime::parse_from_str("2026-10-28 23:30:00", "%Y-%m-%d %H:%M:%S").unwrap()
        );
    }

    #[test]
    fn iso_matches_pythons_isoformat_with_and_without_microseconds() {
        let whole =
            NaiveDateTime::parse_from_str("2026-10-28 23:30:00", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(iso_naive(whole), "2026-10-28T23:30:00");
        let fractional =
            NaiveDateTime::parse_from_str("2026-10-28 23:30:00.123456", "%Y-%m-%d %H:%M:%S%.6f")
                .unwrap();
        assert_eq!(iso_naive(fractional), "2026-10-28T23:30:00.123456");
        assert_eq!(
            iso_utc(Utc.with_ymd_and_hms(2026, 10, 30, 14, 0, 0).unwrap()),
            "2026-10-30T14:00:00+00:00"
        );
    }

    #[test]
    fn ages_are_signed_seconds_with_microsecond_precision() {
        let earlier =
            NaiveDateTime::parse_from_str("2026-10-28 23:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let later =
            NaiveDateTime::parse_from_str("2026-10-28 23:00:01.500000", "%Y-%m-%d %H:%M:%S%.6f")
                .unwrap();
        assert!((seconds_between(later, earlier) - 1.5).abs() < 1e-9);
        assert!((seconds_between(earlier, later) + 1.5).abs() < 1e-9);
    }

    #[test]
    fn the_system_clock_answers_in_utc() {
        let before = Utc::now();
        let now = SystemClock.now_utc();
        let after = Utc::now();
        assert!(now >= before && now <= after);
    }

    // -- the dual clock, from the source suite's `test_market_clock.py` --------------

    fn reference() -> MarketClock {
        MarketClock::from_json(include_str!("../data/clock.json")).expect("the reference parses")
    }

    fn exchange(clock: &MarketClock, code: &str) -> ExchangeRow {
        clock
            .exchanges()
            .find(|row| row.code.as_deref() == Some(code))
            .cloned()
            .unwrap_or_else(|| panic!("{code} is not in the reference"))
    }

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn the_us_session_is_open_in_the_middle_of_the_day() {
        let clock = reference();
        let session =
            clock.exchange_session(&exchange(&clock, "NYSE"), utc(2026, 10, 30, 14, 0), false);
        assert!(session.is_open);
        assert_eq!(session.open_utc, Some(utc(2026, 10, 30, 13, 30)));
        assert_eq!(session.close_utc, Some(utc(2026, 10, 30, 20, 0)));
        assert_eq!(session.clock_source, "exchange");
    }

    #[test]
    fn the_us_session_is_closed_before_the_open() {
        let clock = reference();
        let session =
            clock.exchange_session(&exchange(&clock, "NYSE"), utc(2026, 10, 30, 12, 0), false);
        assert!(!session.is_open);
        assert_eq!(session.next_open_utc, Some(utc(2026, 10, 30, 13, 30)));
    }

    #[test]
    fn the_session_shifts_an_hour_when_new_york_changes_clock() {
        // 30 Oct is EDT (13:30 UTC), 2 Nov is EST (14:30 UTC). The exchange does not move.
        let clock = reference();
        let nyse = exchange(&clock, "NYSE");
        let before = clock.exchange_session(&nyse, utc(2026, 10, 30, 14, 0), false);
        let after = clock.exchange_session(&nyse, utc(2026, 11, 2, 15, 0), false);
        assert_eq!(before.open_utc.unwrap().hour(), 13);
        assert_eq!(after.open_utc.unwrap().hour(), 14);
        assert!(after.is_open);
    }

    #[test]
    fn a_projection_across_a_clock_change_is_not_an_hour_late() {
        // Asked on Friday 30 October (EDT), the next US open is Monday 2 November (EST):
        // 14:30 UTC, not 13:30.
        let clock = reference();
        let session =
            clock.exchange_session(&exchange(&clock, "NYSE"), utc(2026, 10, 30, 14, 0), false);
        assert_eq!(session.next_open_utc, Some(utc(2026, 11, 2, 14, 30)));
    }

    #[test]
    fn the_weekend_is_closed_and_the_next_open_is_monday() {
        let clock = reference();
        let session =
            clock.exchange_session(&exchange(&clock, "NYSE"), utc(2026, 10, 31, 12, 0), false);
        assert!(!session.is_open);
        assert_eq!(session.next_open_utc, Some(utc(2026, 11, 2, 14, 30)));
    }

    #[test]
    fn the_asx_close_rolls_into_the_next_utc_day() {
        // 10:00-16:00 Sydney in November (AEDT, +11) is 23:00-05:00 UTC.
        let clock = reference();
        let session =
            clock.exchange_session(&exchange(&clock, "ASX"), utc(2026, 11, 4, 1, 0), false);
        assert!(session.is_open);
        assert_eq!(session.open_utc, Some(utc(2026, 11, 3, 23, 0)));
        assert_eq!(session.close_utc, Some(utc(2026, 11, 4, 5, 0)));
    }

    #[test]
    fn the_synthetic_clock_skips_the_weekend_only_when_told_to() {
        let clock = reference();
        let saturday = utc(2026, 10, 31, 12, 0);
        assert!(!clock.synthetic_session(saturday, false, None, None).is_open);
        assert!(clock.synthetic_session(saturday, true, None, None).is_open);
        assert!(
            clock
                .synthetic_session(utc(2026, 11, 2, 12, 0), false, None, None)
                .is_open
        );
        let closed = clock.synthetic_session(saturday, false, None, None);
        assert_eq!(closed.next_open_utc, Some(utc(2026, 11, 2, 0, 0)));
        let nanosecond = closed.close_utc.unwrap();
        assert_eq!(
            (nanosecond.hour(), nanosecond.minute(), nanosecond.second()),
            (23, 59, 59)
        );
        assert_eq!(nanosecond.timestamp_subsec_micros(), 999_999);
    }

    #[test]
    fn a_broker_asset_class_resolves_through_the_same_rule_mctrader_uses() {
        let clock = reference();
        let shares = clock.asset_class_id("shares").unwrap();
        let crypto = clock.asset_class_id("crypto").unwrap();
        let instant = utc(2026, 10, 30, 14, 0);
        assert_eq!(clock.resolve(1, shares, instant).clock_source, "exchange");
        // Alpaca's crypto row is ExchangeHours=0, Synthetic24=0 -> always open, as in McTrader.
        assert_eq!(
            clock.resolve(1, crypto, instant).clock_source,
            "always_open"
        );
    }

    #[test]
    fn the_broker_payload_lists_the_exchanges_that_broker_answers_for() {
        let clock = reference();
        let payload = clock
            .broker_clock(1, Some(utc(2026, 10, 30, 14, 0)))
            .unwrap();
        assert_eq!(payload["broker"]["name"], "Alpaca");
        let codes: std::collections::BTreeSet<&str> = payload["exchanges"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["code"].as_str())
            .collect();
        let expected: std::collections::BTreeSet<&str> =
            ["NYSE", "NASDAQ", "NYSEARCA", "AMEX", "IEX"]
                .into_iter()
                .collect();
        assert_eq!(codes, expected);
        assert!(payload["exchanges"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["is_open"] == true));
        assert!(payload["reference_source"]
            .as_str()
            .unwrap()
            .starts_with("McTrader_Live"));
    }

    #[test]
    fn abbreviations_resolve_to_a_zone_that_knows_about_daylight_saving() {
        // `Tz::from_str("EST")` would pin -05:00 all year, an hour wrong for eight months.
        let new_york = resolve_time_zone(Some("EST"));
        let july = utc(2026, 7, 15, 12, 0).with_timezone(&new_york);
        let january = utc(2026, 1, 15, 12, 0).with_timezone(&new_york);
        assert_eq!(july.offset().fix().local_minus_utc(), -4 * 3600);
        assert_eq!(january.offset().fix().local_minus_utc(), -5 * 3600);
    }

    #[test]
    fn an_unknown_broker_is_a_named_refusal() {
        let clock = reference();
        let error = clock.broker_clock(99, None).unwrap_err();
        assert_eq!(error.message, "broker 99 is not in the clock reference");
    }

    #[test]
    fn the_reference_source_is_the_documents_own() {
        let clock = reference();
        assert!(clock.source().starts_with("McTrader_Live"));
        assert_eq!(clock.exchanges_len(), 13);
        assert_eq!(clock.brokers_len(), 2);
    }

    #[test]
    fn hhmm_parsing_accepts_two_or_three_parts_and_rejects_nonsense() {
        assert_eq!(
            parse_hhmm(Some(" 09:30 ")),
            NaiveTime::from_hms_opt(9, 30, 0)
        );
        assert_eq!(
            parse_hhmm(Some("16:00:30")),
            NaiveTime::from_hms_opt(16, 0, 30)
        );
        assert_eq!(parse_hhmm(Some("24:00")), None);
        assert_eq!(parse_hhmm(Some("9:30:00:00")), None);
        assert_eq!(parse_hhmm(None), None);
    }
}
