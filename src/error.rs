//! The error shapes the external contract speaks.
//!
//! Three things travel through the service:
//!
//! * [`MarketDataError`] — a caller-facing refusal with a reason, a detail and an HTTP status.
//!   Every endpoint renders it as `{"error": reason, "detail": detail}`.
//! * [`StoreBusy`] — a database read that could not get a lock in time. It is a named,
//!   retryable condition rather than a hang; the API renders it as 503 `database-busy`.
//! * [`ProviderFailure`] — a vendor refused or a provider could not answer. It carries the
//!   same reason/detail pair so the router and the API can branch on it.
//!
//! The types are deliberately plain data. Formatting lives on `Display`, because a log line
//! and an HTTP body want the same sentence.

use std::fmt;

/// A request the service refuses, naming what was wrong.
///
/// `status` is the HTTP status the API will use. `reason` is stable and machine-readable;
/// `detail` is written for the person who has to fix the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketDataError {
    pub reason: String,
    pub detail: String,
    pub status: u16,
}

impl MarketDataError {
    pub fn new(reason: impl Into<String>, detail: impl Into<String>, status: u16) -> Self {
        Self {
            reason: reason.into(),
            detail: detail.into(),
            status,
        }
    }

    /// A caller's mistake: something about the request was wrong, not the service.
    pub fn bad_request(reason: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(reason, detail, 400)
    }

    /// The service could not answer for a reason the caller cannot fix.
    pub fn internal(reason: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(reason, detail, 500)
    }
}

impl fmt::Display for MarketDataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() {
            formatter.write_str(&self.reason)
        } else {
            write!(formatter, "{}: {}", self.reason, self.detail)
        }
    }
}

impl std::error::Error for MarketDataError {}

/// A database read that could not get a lock in time.
///
/// Raised instead of letting a pooled connection wait out the server's default timeout: a
/// read waiting two minutes for a writer is indistinguishable, from the caller's side, from a
/// service that has died. The API turns this into a 503 that says "retry".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreBusy {
    pub detail: String,
}

impl StoreBusy {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for StoreBusy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "database busy: {}", self.detail)
    }
}

impl std::error::Error for StoreBusy {}

/// A provider could not answer, with a reason the router can branch on.
///
/// This mirrors the source service's `ProviderError`: `reason` is what the fallback decision
/// reads, `status` is the vendor's HTTP status when there was one, and `detail` is evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub reason: String,
    pub detail: String,
    pub status: Option<u16>,
}

impl ProviderFailure {
    pub fn new(reason: impl Into<String>, detail: impl Into<String>, status: Option<u16>) -> Self {
        Self {
            reason: reason.into(),
            detail: detail.into(),
            status,
        }
    }
}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() {
            formatter.write_str(&self.reason)
        } else {
            write!(formatter, "{}: {}", self.reason, self.detail)
        }
    }
}

impl std::error::Error for ProviderFailure {}

/// A Python-readable string literal, for details that quote a caller's input.
///
/// The source service builds these details with `!r`, and its readers and tests match the
/// single quotes. Rust's `Debug` uses double quotes, so the shape is reproduced explicitly.
pub fn py_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_the_python_exception_text() {
        let error = MarketDataError::bad_request("unknown-venue", "NOWHERE is not a market");
        assert_eq!(error.to_string(), "unknown-venue: NOWHERE is not a market");
        let bare = MarketDataError::bad_request("no-symbols", "");
        assert_eq!(bare.to_string(), "no-symbols");
    }

    #[test]
    fn repr_uses_single_quotes_like_python() {
        assert_eq!(py_repr("CBA.AU"), "'CBA.AU'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("both'and\""), "'both\\'and\"'");
    }
}
