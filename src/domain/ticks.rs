//! Exchange price steps — the resolution a market actually quotes in.
//!
//! # Why this exists
//!
//! An ATR is a distance in the instrument's price units, and a stop is a multiple of it.
//! Neither is meaningful unless it is compared against the smallest distance the market can
//! express. STP (ASX, $0.135) answered `ATR = 0.005357` — mathematically right and, at a
//! $0.005 step, **1.07 ticks**. A 2.5x stop on it was 2.7 ticks wide against a 1-tick spread:
//! the stop was inside the noise floor, and no choice of ATR period could have fixed that.
//!
//! Tick size is a property of the *venue and the price band*, not of the asset class. A US
//! listing moves in $0.01 at any price; the ASX moves in $0.001 under $0.10, $0.005 from
//! $0.10 to $2.00, and $0.01 above that. The estate's broker-side table
//! (`broker_adapters/ibkr/calendar.py`, `tick_size: 0.01` per asset class) is correct for the
//! US and for an ASX name above $2.00, and wrong below it — which is the entire band where the
//! problem occurs. This module is the one authority for the model; callers that need a step
//! read it from the service's ATR answer rather than carrying their own table.
//!
//! Everything here is pure: no network, no database, no clock.

/// The step a US listing quotes in, and an ASX listing above $2.00.
pub const STEP_DOLLAR: f64 = 0.01;
/// The ASX step from $0.10 to $2.00.
pub const STEP_HALF_CENT: f64 = 0.005;
/// The ASX step below $0.10.
pub const STEP_MILL: f64 = 0.001;

/// The ASX price-band boundaries: under `BAND_LOW` the step is a tenth of a cent, under
/// `BAND_HIGH` it is half a cent, above it a whole cent.
pub const ASX_BAND_LOW: f64 = 0.10;
pub const ASX_BAND_HIGH: f64 = 2.00;

/// The smallest price increment `exchange` quotes `price` in.
///
/// `None` for a price that cannot be quoted at all (zero, negative or not finite) — the same
/// refusal `atr_percent` makes, and for the same reason: a ratio against it would be a number
/// with no meaning.
///
/// An unrecognised venue answers the dollar step rather than `None`. That is the conservative
/// default for this service's universe (the US tape is $0.01 at every price), and it keeps an
/// unknown exchange from silently disabling the tick diagnostic downstream. A venue whose
/// smallest step is *smaller* than a cent would have its tick count understated, never
/// overstated — the error points at "fewer ticks than there are", so a caller's floor still
/// refuses rather than admits.
pub fn price_step(exchange: &str, price: f64) -> Option<f64> {
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    let venue = exchange.trim().to_ascii_uppercase();
    let step = match venue.as_str() {
        // ASX quotes in bands. The boundaries are inclusive at the bottom of the higher band:
        // a stock at exactly $0.10 moves in half-cents, one at exactly $2.00 in cents.
        "ASX" | "ASX24" => {
            if price < ASX_BAND_LOW {
                STEP_MILL
            } else if price < ASX_BAND_HIGH {
                STEP_HALF_CENT
            } else {
                STEP_DOLLAR
            }
        }
        _ => STEP_DOLLAR,
    };
    Some(step)
}

/// `value` measured in ticks of `step`.
///
/// `None` when `step` is not a step (`None`, zero or negative) or `value` is not a distance.
pub fn as_ticks(value: f64, step: Option<f64>) -> Option<f64> {
    let step = step?;
    if !value.is_finite() || !step.is_finite() || step <= 0.0 {
        return None;
    }
    Some(value / step)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asx_bands_are_half_a_cent_at_the_stock_that_prompted_this() {
        // STP traded at $0.135, where one tick was 3.70% of the price.
        assert_eq!(price_step("ASX", 0.135), Some(STEP_HALF_CENT));
        assert_eq!(price_step("ASX", 1.99), Some(STEP_HALF_CENT));
    }

    #[test]
    fn the_band_edges_belong_to_the_higher_band() {
        assert_eq!(price_step("ASX", 0.099), Some(STEP_MILL));
        assert_eq!(price_step("ASX", 0.10), Some(STEP_HALF_CENT));
        assert_eq!(price_step("ASX", 2.00), Some(STEP_DOLLAR));
    }

    #[test]
    fn a_us_listing_moves_in_cents_at_every_price() {
        assert_eq!(price_step("NYSE", 0.135), Some(STEP_DOLLAR));
        assert_eq!(price_step("NASDAQ", 50.0), Some(STEP_DOLLAR));
    }

    #[test]
    fn the_venue_is_matched_without_regard_to_case_or_padding() {
        assert_eq!(price_step(" asx ", 0.135), Some(STEP_HALF_CENT));
    }

    #[test]
    fn an_unquoteable_price_has_no_step() {
        assert_eq!(price_step("ASX", 0.0), None);
        assert_eq!(price_step("ASX", -1.0), None);
        assert_eq!(price_step("ASX", f64::NAN), None);
    }

    #[test]
    fn ticks_are_the_ratio_and_nothing_else() {
        // The service's report: 0.005357 over a 0.005 step is 1.07 ticks.
        let step = price_step("ASX", 0.135);
        let ticks = as_ticks(0.005357, step).unwrap();
        assert!((ticks - 1.0714).abs() < 0.0001, "got {ticks}");
    }

    #[test]
    fn a_step_that_is_not_a_step_yields_no_tick_count() {
        assert_eq!(as_ticks(0.005, None), None);
        assert_eq!(as_ticks(0.005, Some(0.0)), None);
        assert_eq!(as_ticks(0.005, Some(-0.01)), None);
        assert_eq!(as_ticks(f64::INFINITY, Some(0.01)), None);
    }
}
