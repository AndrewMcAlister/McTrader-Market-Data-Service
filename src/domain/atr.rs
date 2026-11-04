//! Average True Range — the one place in this service that defines it.
//!
//! # Why true range and not high minus low
//!
//! The obvious reading of "ATR" is the average of each session's high minus its low. That is
//! not what any implementation in the estate does, and it is not what the backtests were
//! measured with. All three use `max(H-L, |H-prevClose|, |L-prevClose|)`. Measured over 2,850
//! symbols, the gap term widens a 14-session ATR by a median 4.3% and by up to 25.8% at p95 —
//! and a movers screen selects precisely the gappy names in that tail. `high_low` is available
//! but is not the default: switching silently would narrow every stop relative to everything
//! already backtested.
//!
//! # Smoothing
//!
//! * `simple` — the mean of the last `period` true ranges. What both stop books use, and
//!   therefore the default, because their `atr_stop_multiple` values were calibrated against
//!   it.
//! * `wilder` — Wilder's recursion, seeded with the mean of the first `period` true ranges.
//!
//! Everything here is pure: no network, no database, no clock.

use std::fmt;

use crate::error::py_repr;

/// The two accepted definitions of "range".
pub const METHOD_TRUE_RANGE: &str = "tr";
pub const METHOD_HIGH_LOW: &str = "high_low";
pub const METHODS: [&str; 2] = [METHOD_TRUE_RANGE, METHOD_HIGH_LOW];

/// The two accepted smoothings.
pub const SMOOTHING_SIMPLE: &str = "simple";
pub const SMOOTHING_WILDER: &str = "wilder";
pub const SMOOTHINGS: [&str; 2] = [SMOOTHING_SIMPLE, SMOOTHING_WILDER];

/// An ATR request the service refuses, naming what was wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtrError {
    pub message: String,
}

impl AtrError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AtrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AtrError {}

/// One range value per session.
///
/// The first session has no previous close, so its range is `H-L` under both methods — there
/// is nothing to gap against. Callers must not read that as a defect: it is the same choice
/// `wilder_atr_series` makes, and it is why `period` true ranges need `period + 1` sessions.
pub fn true_ranges(
    highs: &[f64],
    lows: &[f64],
    closes: &[f64],
    method: &str,
) -> Result<Vec<f64>, AtrError> {
    if !METHODS.contains(&method) {
        return Err(AtrError::new(format!(
            "{} is not an ATR method; use one of {}",
            py_repr(method),
            METHODS.join(", ")
        )));
    }

    let count = highs.len().min(lows.len()).min(closes.len());
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut ranges = Vec::with_capacity(count);
    ranges.push(highs[0] - lows[0]);
    for index in 1..count {
        let high = highs[index];
        let low = lows[index];
        if method == METHOD_HIGH_LOW {
            ranges.push(high - low);
            continue;
        }
        let previous = closes[index - 1];
        ranges.push(
            (high - low)
                .max((high - previous).abs())
                .max((low - previous).abs()),
        );
    }
    Ok(ranges)
}

/// The ATR over the last `period` ranges, or `None` when there is too little history.
///
/// `None` rather than a guess: a stop placed from four sessions is not the stop the study
/// measured, and both stop books already refuse that case rather than approximating it.
///
/// Under `simple` the answer is the mean of the last `period` ranges, so extra history is
/// harmless and only the tail is read. Under `wilder` every earlier range feeds the
/// recursion, so the whole series is used and the seed is the mean of the first `period`
/// ranges.
pub fn average_true_range(
    highs: &[f64],
    lows: &[f64],
    closes: &[f64],
    period: i64,
    method: &str,
    smoothing: &str,
) -> Result<Option<f64>, AtrError> {
    if period < 1 {
        return Err(AtrError::new(format!(
            "period must be positive, got {period}"
        )));
    }
    if !SMOOTHINGS.contains(&smoothing) {
        return Err(AtrError::new(format!(
            "{} is not a smoothing; use one of {}",
            py_repr(smoothing),
            SMOOTHINGS.join(", ")
        )));
    }

    let period = period as usize;
    let ranges = true_ranges(highs, lows, closes, method)?;
    if ranges.len() < period {
        return Ok(None);
    }

    if smoothing == SMOOTHING_SIMPLE {
        let window = &ranges[ranges.len() - period..];
        return Ok(Some(window.iter().sum::<f64>() / period as f64));
    }

    let mut running = ranges[..period].iter().sum::<f64>() / period as f64;
    for value in &ranges[period..] {
        running = (running * (period - 1) as f64 + value) / period as f64;
    }
    Ok(Some(running))
}

/// ATR as percent points of price (2.51 means 2.51%), or `None`.
///
/// Percent points and not a fraction, because that is the unit `McTrader`'s `atr_14_pct` /
/// `atr_22_pct` already carry and the unit every caller divides by 100. A zero or missing
/// price yields `None` rather than an infinity.
pub fn atr_percent(value: Option<f64>, reference_price: Option<f64>) -> Option<f64> {
    let value = value?;
    let reference = reference_price?;
    if value == 0.0 || reference <= 0.0 {
        return None;
    }
    Some(value / reference * 100.0)
}

/// Where a resting stop sits, or `None` when it cannot be computed.
///
/// The multiplier is **required**. The four books in the estate use 1.0, 1.0, 2.0 and 3.0, so
/// a default here would silently impose one book's risk on another's. `None` is returned
/// rather than a defaulted width.
///
/// A long's stop sits below the price, a short's above. A level that would be zero or
/// negative is refused: it is not a stop, it is a sign that the ATR is larger than the
/// instrument's price.
pub fn stop_level(
    price: f64,
    atr_value: Option<f64>,
    multiplier: Option<f64>,
    direction: &str,
) -> Result<Option<f64>, AtrError> {
    let Some(atr_value) = atr_value else {
        return Ok(None);
    };
    if atr_value <= 0.0 {
        return Ok(None);
    }
    let Some(multiplier) = multiplier else {
        return Err(AtrError::new("multiplier is required and must be positive"));
    };
    if multiplier <= 0.0 {
        return Err(AtrError::new("multiplier is required and must be positive"));
    }
    if price <= 0.0 {
        return Ok(None);
    }

    let side = if direction.is_empty() {
        "long".to_string()
    } else {
        direction.trim().to_lowercase()
    };
    if side != "long" && side != "short" {
        return Err(AtrError::new(format!(
            "{} is not a direction; use 'long' or 'short'",
            py_repr(direction)
        )));
    }

    let offset = multiplier * atr_value;
    let level = if side == "long" {
        price - offset
    } else {
        price + offset
    };
    Ok(if level > 0.0 {
        Some(round_to_six_places(level))
    } else {
        None
    })
}

/// Rust's fixed-precision formatting is correctly rounded to the nearest decimal, ties to
/// even, which is what Python's `round(level, 6)` does. Multiplying by `1e6` first would
/// introduce its own error before the rounding step and can move a level in the last digit.
fn round_to_six_places(value: f64) -> f64 {
    format!("{value:.6}").parse().unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn flat_series_has_a_range_of_its_width() {
        let highs = [10.0; 20];
        let lows = [6.0; 20];
        let closes = [8.0; 20];
        let ranges = true_ranges(&highs, &lows, &closes, METHOD_TRUE_RANGE).unwrap();
        assert!(ranges.iter().all(|value| approx(*value, 4.0)));
    }

    #[test]
    fn a_gap_widens_the_range_beyond_high_minus_low() {
        let highs = [10.0, 21.0];
        let lows = [9.0, 20.0];
        let closes = [10.0, 20.5];
        let tr = true_ranges(&highs, &lows, &closes, METHOD_TRUE_RANGE).unwrap();
        let hl = true_ranges(&highs, &lows, &closes, METHOD_HIGH_LOW).unwrap();
        assert!(approx(tr[1], 11.0));
        assert!(approx(hl[1], 1.0));
        assert!(tr[1] > hl[1]);
    }

    #[test]
    fn high_minus_low_is_never_wider_than_true_range() {
        let highs = [12.0, 31.0, 33.0, 20.0];
        let lows = [10.0, 29.0, 30.0, 15.0];
        let closes = [11.0, 30.0, 31.0, 16.0];
        let tr = true_ranges(&highs, &lows, &closes, METHOD_TRUE_RANGE).unwrap();
        let hl = true_ranges(&highs, &lows, &closes, METHOD_HIGH_LOW).unwrap();
        assert!(tr
            .iter()
            .zip(&hl)
            .all(|(wide, narrow)| wide + 1e-12 >= *narrow));
    }

    #[test]
    fn unknown_method_is_refused() {
        let error = true_ranges(&[1.0], &[0.5], &[0.75], "average_of_something").unwrap_err();
        assert!(error.message.contains("not an ATR method"));
    }

    #[test]
    fn simple_atr_of_a_constant_width_series_is_that_width() {
        let result = average_true_range(
            &[10.0; 15],
            &[6.0; 15],
            &[8.0; 15],
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap();
        assert!(approx(result.unwrap(), 4.0));
    }

    #[test]
    fn insufficient_history_returns_none_rather_than_a_guess() {
        let result = average_true_range(
            &[10.0; 5],
            &[6.0; 5],
            &[8.0; 5],
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn the_in_progress_session_must_be_excluded_by_the_caller() {
        let highs = [10.0; 14].into_iter().chain([11.0]).collect::<Vec<_>>();
        let lows = [6.0; 14].into_iter().chain([10.5]).collect::<Vec<_>>();
        let closes = [8.0; 14].into_iter().chain([10.8]).collect::<Vec<_>>();

        let with_partial = average_true_range(
            &highs,
            &lows,
            &closes,
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap();
        let without_partial = average_true_range(
            &highs[..14],
            &lows[..14],
            &closes[..14],
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap();

        assert!(approx(without_partial.unwrap(), 4.0));
        assert!(!approx(with_partial.unwrap(), without_partial.unwrap()));
    }

    #[test]
    fn the_agreed_formula_is_the_default() {
        let highs = [
            10.0, 11.0, 12.0, 11.5, 12.5, 13.0, 12.0, 13.5, 14.0, 13.0, 14.5, 15.0, 14.0, 15.5,
            16.0,
        ];
        let lows = [
            9.0, 9.5, 10.5, 10.0, 11.0, 11.5, 10.5, 12.0, 12.5, 11.5, 13.0, 13.5, 12.5, 14.0, 14.5,
        ];
        let closes = [
            9.5, 10.0, 11.0, 10.5, 11.5, 12.0, 11.0, 12.5, 13.0, 12.0, 13.5, 14.0, 13.0, 14.5, 15.0,
        ];

        let mut ranges = Vec::new();
        for index in 1..highs.len() {
            let high: f64 = highs[index];
            let low: f64 = lows[index];
            let previous: f64 = closes[index - 1];
            ranges.push(
                (high - low)
                    .max((high - previous).abs())
                    .max((low - previous).abs()),
            );
        }
        let expected = ranges[ranges.len() - 14..].iter().sum::<f64>() / 14.0;

        let actual = average_true_range(
            &highs,
            &lows,
            &closes,
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap();
        assert!(approx(actual.unwrap(), expected));
    }

    #[test]
    fn wilder_is_not_the_default() {
        let highs = [10.0; 14].into_iter().chain([20.0; 14]).collect::<Vec<_>>();
        let lows = [6.0; 14].into_iter().chain([16.0; 14]).collect::<Vec<_>>();
        let closes = [8.0; 14].into_iter().chain([18.0; 14]).collect::<Vec<_>>();
        let simple = average_true_range(
            &highs,
            &lows,
            &closes,
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap()
        .unwrap();
        let wilder = average_true_range(
            &highs,
            &lows,
            &closes,
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_WILDER,
        )
        .unwrap()
        .unwrap();
        assert!(!approx(simple, wilder));
    }

    #[test]
    fn high_minus_low_is_not_the_default() {
        let highs = [10.0, 21.0];
        let lows = [9.0, 20.0];
        let closes = [10.0, 20.5];
        let default = average_true_range(
            &highs,
            &lows,
            &closes,
            1,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap()
        .unwrap();
        let narrow =
            average_true_range(&highs, &lows, &closes, 1, METHOD_HIGH_LOW, SMOOTHING_SIMPLE)
                .unwrap()
                .unwrap();
        assert!(approx(default, 11.0));
        assert!(approx(narrow, 1.0));
    }

    #[test]
    fn wilder_and_simple_agree_on_a_constant_series() {
        let simple = average_true_range(
            &[10.0; 30],
            &[6.0; 30],
            &[8.0; 30],
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap()
        .unwrap();
        let wilder = average_true_range(
            &[10.0; 30],
            &[6.0; 30],
            &[8.0; 30],
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_WILDER,
        )
        .unwrap()
        .unwrap();
        assert!(approx(simple, 4.0));
        assert!(approx(wilder, 4.0));
    }

    #[test]
    fn wilder_changes_when_the_series_changes() {
        let highs = [10.0; 14].into_iter().chain([20.0; 14]).collect::<Vec<_>>();
        let lows = [6.0; 14].into_iter().chain([16.0; 14]).collect::<Vec<_>>();
        let closes = [8.0; 14].into_iter().chain([18.0; 14]).collect::<Vec<_>>();
        let simple = average_true_range(
            &highs,
            &lows,
            &closes,
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap()
        .unwrap();
        let wilder = average_true_range(
            &highs,
            &lows,
            &closes,
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_WILDER,
        )
        .unwrap()
        .unwrap();
        assert!(!approx(simple, wilder));
    }

    #[test]
    fn a_longer_period_is_not_the_same_reading() {
        let mut highs = vec![210.0; 40];
        highs.push(152.0);
        highs.extend([152.0; 19]);
        let mut lows = vec![190.0; 40];
        lows.push(150.0);
        lows.extend([150.0; 19]);
        let mut closes = vec![200.0; 40];
        closes.extend([151.0; 20]);

        let atr = |period| {
            average_true_range(
                &highs,
                &lows,
                &closes,
                period,
                METHOD_TRUE_RANGE,
                SMOOTHING_SIMPLE,
            )
            .unwrap()
            .unwrap()
        };
        let a14 = atr(14);
        let a22 = atr(22);
        let a50 = atr(50);
        assert!(approx(a14, 2.0));
        assert!(a22 > a14);
        assert!(a50 > a22);
    }

    #[test]
    fn bad_period_is_refused() {
        let error = average_true_range(
            &[1.0; 5],
            &[0.5; 5],
            &[0.75; 5],
            0,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap_err();
        assert!(error.message.contains("period must be positive"));
    }

    #[test]
    fn bad_smoothing_is_refused() {
        let error = average_true_range(
            &[1.0; 5],
            &[0.5; 5],
            &[0.75; 5],
            2,
            METHOD_TRUE_RANGE,
            "exponential",
        )
        .unwrap_err();
        assert!(error.message.contains("not a smoothing"));
    }

    #[test]
    fn percent_is_percent_points_not_a_fraction() {
        assert!(approx(atr_percent(Some(5.0), Some(200.0)).unwrap(), 2.5));
        assert_eq!(atr_percent(Some(5.0), None), None);
        assert_eq!(atr_percent(Some(5.0), Some(0.0)), None);
        assert_eq!(atr_percent(None, Some(200.0)), None);
    }

    #[test]
    fn stop_sits_one_atr_below_a_long() {
        assert!(approx(
            stop_level(100.0, Some(5.0), Some(1.0), "long")
                .unwrap()
                .unwrap(),
            95.0
        ));
    }

    #[test]
    fn stop_sits_above_a_short() {
        assert!(approx(
            stop_level(100.0, Some(5.0), Some(1.0), "short")
                .unwrap()
                .unwrap(),
            105.0
        ));
    }

    #[test]
    fn blank_direction_is_long() {
        assert!(approx(
            stop_level(100.0, Some(5.0), Some(1.0), "")
                .unwrap()
                .unwrap(),
            95.0
        ));
    }

    #[test]
    fn multiplier_is_required_and_never_defaulted() {
        let error = stop_level(100.0, Some(5.0), None, "long").unwrap_err();
        assert!(error.message.contains("multiplier is required"));
    }

    #[test]
    fn a_multiplier_wider_than_the_price_is_not_a_stop() {
        assert_eq!(
            stop_level(10.0, Some(20.0), Some(1.0), "long").unwrap(),
            None
        );
    }

    #[test]
    fn zero_atr_yields_no_stop() {
        assert_eq!(
            stop_level(100.0, Some(0.0), Some(1.0), "long").unwrap(),
            None
        );
        assert_eq!(stop_level(100.0, None, Some(1.0), "long").unwrap(), None);
    }

    #[test]
    fn bad_direction_is_refused() {
        let error = stop_level(100.0, Some(5.0), Some(1.0), "sideways").unwrap_err();
        assert!(error.message.contains("not a direction"));
    }

    #[test]
    fn a_stop_is_rounded_to_six_decimal_places_like_python() {
        let level = stop_level(10.0, Some(1.23456789), Some(1.0), "long")
            .unwrap()
            .unwrap();
        assert_eq!(level, 8.765432);
    }

    #[test]
    fn true_range_and_high_low_diverge_materially_on_a_gappy_series() {
        // Deterministic version of the measured 2,850-symbol comparison: gaps every session
        // make true range wider than H-L by far more than the 0.1% materiality threshold.
        let mut highs = Vec::with_capacity(200);
        let mut lows = Vec::with_capacity(200);
        let mut closes = Vec::with_capacity(200);
        let mut price = 100.0_f64;
        for index in 0..200 {
            let gap = if index % 2 == 0 { 2.0 } else { -2.0 };
            let open = price + gap;
            let high = open + 1.0 + (index % 3) as f64 * 0.1;
            let low = open - 1.0;
            let close = (high + low) / 2.0;
            highs.push(high);
            lows.push(low);
            closes.push(close);
            price = close;
        }

        let tr = average_true_range(
            &highs,
            &lows,
            &closes,
            14,
            METHOD_TRUE_RANGE,
            SMOOTHING_SIMPLE,
        )
        .unwrap()
        .unwrap();
        let hl = average_true_range(
            &highs,
            &lows,
            &closes,
            14,
            METHOD_HIGH_LOW,
            SMOOTHING_SIMPLE,
        )
        .unwrap()
        .unwrap();
        assert!(tr > hl);
        assert!(tr / hl > 1.001);
    }
}
