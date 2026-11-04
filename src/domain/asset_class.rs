//! The asset classes the service distinguishes, and their aliases.
//!
//! Anything unrecognised is treated as shares, because that is the overwhelming majority of
//! requests and a wrong default here would show up as "no price" rather than as a wrong
//! price. The aliases are the spellings callers already type: `indicies` (the historical
//! misspelling), `index`, `equities`, `futures`, `bonds`, `fx`, `currency`.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetClass {
    Shares,
    Crypto,
    Indices,
    Commodities,
    Rates,
    Forex,
}

/// The classes in the order the source service declares them.
pub const ASSET_CLASSES: [AssetClass; 6] = [
    AssetClass::Shares,
    AssetClass::Crypto,
    AssetClass::Indices,
    AssetClass::Commodities,
    AssetClass::Rates,
    AssetClass::Forex,
];

/// The market code an asset with no exchange is cached under. `sessions::market_code` uses the
/// same label, so a bar and the ATR built from it are filed in the same place.
pub const CRYPTO_MARKET: &str = "CRYPTO";

impl AssetClass {
    /// The canonical class for any spelling; unknown and blank values fall back to shares.
    pub fn parse(value: &str) -> Self {
        let key = value.trim().to_lowercase();
        match key.as_str() {
            "" => Self::Shares,
            "indicies" | "indices" | "index" => Self::Indices,
            "equities" | "equity" | "shares" => Self::Shares,
            "commodity" | "commodities" | "futures" | "future" => Self::Commodities,
            "rate" | "rates" | "bonds" => Self::Rates,
            "fx" | "forex" | "currency" => Self::Forex,
            "cryptocurrency" | "crypto" => Self::Crypto,
            _ => Self::Shares,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shares => "shares",
            Self::Crypto => "crypto",
            Self::Indices => "indices",
            Self::Commodities => "commodities",
            Self::Rates => "rates",
            Self::Forex => "forex",
        }
    }

    pub fn is_crypto(self) -> bool {
        self == Self::Crypto
    }
}

impl fmt::Display for AssetClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AssetClass {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_canonical_classes_round_trip() {
        for class in ASSET_CLASSES {
            assert_eq!(AssetClass::parse(class.as_str()), class);
        }
    }

    #[test]
    fn aliases_resolve_to_the_canonical_class() {
        assert_eq!(AssetClass::parse("indicies"), AssetClass::Indices);
        assert_eq!(AssetClass::parse("INDEX"), AssetClass::Indices);
        assert_eq!(AssetClass::parse("equities"), AssetClass::Shares);
        assert_eq!(AssetClass::parse("futures"), AssetClass::Commodities);
        assert_eq!(AssetClass::parse("bonds"), AssetClass::Rates);
        assert_eq!(AssetClass::parse("fx"), AssetClass::Forex);
        assert_eq!(AssetClass::parse("Cryptocurrency"), AssetClass::Crypto);
        assert_eq!(AssetClass::parse(" crypto "), AssetClass::Crypto);
    }

    #[test]
    fn unknown_and_blank_fall_back_to_shares() {
        assert_eq!(AssetClass::parse("warrants"), AssetClass::Shares);
        assert_eq!(AssetClass::parse(""), AssetClass::Shares);
    }
}
