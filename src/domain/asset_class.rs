//! The asset classes the service distinguishes, and their aliases.
//!
//! Two parsers, deliberately. [`AssetClass::parse`] folds anything unrecognised to shares,
//! because that is the overwhelming majority of requests and an internal caller that holds a
//! class only needs it normalised. [`AssetClass::parse_strict`] answers `None` for a typo and is
//! what the HTTP boundary uses, so a caller's `asset_class=indexs` is a 422 that names the
//! vocabulary rather than a silent share — an index priced as a share is a different
//! instrument, not a rounding difference.
//!
//! The aliases are the spellings callers already type: `indicies` (the historical misspelling),
//! `index`, `equities`, `futures`, `bonds`, `fx`, `currency`.

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

/// Every spelling [`AssetClass::parse_strict`] accepts, with the class it means.
///
/// One table serves the parser and the refusal that lists the vocabulary, so a name cannot
/// be accepted by one and missing from the other.
pub const ASSET_CLASS_SPELLINGS: &[(&str, AssetClass)] = &[
    ("shares", AssetClass::Shares),
    ("equities", AssetClass::Shares),
    ("equity", AssetClass::Shares),
    ("crypto", AssetClass::Crypto),
    ("cryptocurrency", AssetClass::Crypto),
    ("indices", AssetClass::Indices),
    ("indicies", AssetClass::Indices),
    ("index", AssetClass::Indices),
    ("commodities", AssetClass::Commodities),
    ("commodity", AssetClass::Commodities),
    ("futures", AssetClass::Commodities),
    ("future", AssetClass::Commodities),
    ("rates", AssetClass::Rates),
    ("rate", AssetClass::Rates),
    ("bonds", AssetClass::Rates),
    ("forex", AssetClass::Forex),
    ("fx", AssetClass::Forex),
    ("currency", AssetClass::Forex),
];

/// The market code an asset with no exchange is cached under. `sessions::market_code` uses the
/// same label, so a bar and the ATR built from it are filed in the same place.
pub const CRYPTO_MARKET: &str = "CRYPTO";

impl AssetClass {
    /// The canonical class for any spelling; unknown and blank values fall back to shares.
    ///
    /// The fallback is for internal callers that already hold a class and only need it
    /// normalised. An HTTP request must use [`AssetClass::parse_strict`]: a caller's typo is a
    /// refusal it can see, not a silent reclassification.
    pub fn parse(value: &str) -> Self {
        Self::parse_strict(value).unwrap_or(Self::Shares)
    }

    /// The canonical class for a recognised spelling, or `None` when it is not one.
    pub fn parse_strict(value: &str) -> Option<Self> {
        let key = value.trim().to_lowercase();
        if key.is_empty() {
            return None;
        }
        ASSET_CLASS_SPELLINGS
            .iter()
            .find(|(spelling, _)| *spelling == key)
            .map(|(_, class)| *class)
    }

    /// The accepted spellings, in the order [`ASSET_CLASS_SPELLINGS`] declares them.
    pub fn accepted_spellings() -> String {
        ASSET_CLASS_SPELLINGS
            .iter()
            .map(|(spelling, _)| *spelling)
            .collect::<Vec<_>>()
            .join(", ")
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

    #[test]
    fn the_strict_parser_refuses_what_the_lenient_one_folds() {
        assert_eq!(AssetClass::parse_strict(" warrants "), None);
        assert_eq!(AssetClass::parse_strict(""), None);
        assert_eq!(AssetClass::parse_strict("   "), None);
        for (spelling, class) in ASSET_CLASS_SPELLINGS {
            assert_eq!(AssetClass::parse_strict(spelling), Some(*class), "{spelling}");
            assert_eq!(AssetClass::parse(spelling), *class, "{spelling}");
        }
    }

    #[test]
    fn the_accepted_list_names_every_spelling_and_nothing_else() {
        let list = AssetClass::accepted_spellings();
        for (spelling, _) in ASSET_CLASS_SPELLINGS {
            assert!(list.contains(spelling), "{spelling} missing from {list}");
        }
        assert_eq!(list.matches(',').count(), ASSET_CLASS_SPELLINGS.len() - 1);
        for class in ASSET_CLASSES {
            assert!(list.contains(class.as_str()), "{} missing", class.as_str());
        }
    }
}
