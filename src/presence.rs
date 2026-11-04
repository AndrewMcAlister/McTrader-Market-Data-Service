//! A JSON field that distinguishes "absent" from "present and null".
//!
//! The Python service emits objects whose key set varies: an ATR reading computed from bars
//! carries `"reason": null`, an unavailable one carries a sentence, and a cache hit omits the
//! key entirely. A refresh outcome carries `"expected_session": null` when the calendar could
//! not answer, and omits it altogether for a no-key refusal. `Option<T>` cannot express both,
//! and adding a separate boolean would be a second source of truth.
//!
//! [`Presence`] is that third state. It serializes only when it is not [`Presence::Absent`];
//! callers pair it with `skip_serializing_if = "Presence::is_absent"`.

use serde::{Serialize, Serializer};

/// Three states of an optional field.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Presence<T> {
    /// The field did not exist.
    #[default]
    Absent,
    /// The field existed and was null.
    Null,
    /// The field existed and carried a value.
    Value(T),
}

impl<T> Presence<T> {
    /// Whether the field should be omitted from the rendered object.
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// Whether the field was present at all, including as null.
    pub fn is_present(&self) -> bool {
        !self.is_absent()
    }

    /// The value, when there is one. Null and absent both answer `None`.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }

    /// A present field from a Rust option: `Some` becomes a value, `None` becomes null.
    pub fn from_option(value: Option<T>) -> Self {
        match value {
            Some(value) => Self::Value(value),
            None => Self::Null,
        }
    }
}

impl<T: Serialize> Serialize for Presence<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // Absent fields are skipped by the struct that owns them; if one is ever
            // serialized on its own, null is the least surprising fallback.
            Self::Absent | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use serde_json::json;

    #[derive(Serialize)]
    struct Example {
        #[serde(skip_serializing_if = "Presence::is_absent")]
        reason: Presence<String>,
    }

    #[test]
    fn absent_is_omitted_and_null_is_rendered() {
        let absent = serde_json::to_value(Example {
            reason: Presence::Absent,
        })
        .unwrap();
        assert_eq!(absent, json!({}));

        let null = serde_json::to_value(Example {
            reason: Presence::Null,
        })
        .unwrap();
        assert_eq!(null, json!({"reason": null}));

        let value = serde_json::to_value(Example {
            reason: Presence::Value("too few sessions".into()),
        })
        .unwrap();
        assert_eq!(value, json!({"reason": "too few sessions"}));
    }

    #[test]
    fn options_round_trip_through_presence() {
        assert_eq!(Presence::from_option(Some(1)), Presence::Value(1));
        assert_eq!(Presence::<i32>::from_option(None), Presence::Null);
        assert_eq!(Presence::Value(1).value(), Some(&1));
        assert_eq!(Presence::<i32>::Null.value(), None);
        assert!(Presence::<i32>::Absent.is_absent());
        assert!(!Presence::<i32>::Null.is_absent());
    }
}
