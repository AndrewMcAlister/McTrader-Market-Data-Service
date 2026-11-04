//! Store failures, classified the way the API needs to render them.
//!
//! Two kinds matter to a caller:
//!
//! * [`StoreError::Busy`] — SQL Server error 1222 (lock request timeout). The API turns this
//!   into `503 database-busy` with `retryable: true`, because the daily load holds the write
//!   lock only briefly and the right answer is "ask again", not "the service is broken".
//! * everything else — a database failure or a row that violates what the schema promises.
//!
//! The lock-timeout mapping is the same rule the source service applied to pyodbc's error
//! text: error 1222, or a message saying `Lock request time out`. A pool checkout timeout is
//! *also* reported as busy: from the caller's side it means the database could not be reached
//! quickly, and retrying is equally right.

use thiserror::Error;

use crate::error::StoreBusy;

/// The detail the source service returns for a lock wait. Kept identical because callers and
/// runbooks already key on it.
pub const STORE_BUSY_DETAIL: &str = "the database is locked by a writer; retry shortly \
     (the daily load holds the lock only briefly)";

/// SQL Server's error code for a lock request timeout.
pub const LOCK_TIMEOUT_CODE: u32 = 1222;

/// SQL Server's duplicate-key codes: unique index (2601) and unique constraint (2627).
pub const DUPLICATE_KEY_CODES: [u32; 2] = [2601, 2627];

/// Anything that stopped a store operation from answering.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoreError {
    /// A read or write that could not get its lock in time. Retryable.
    #[error("{0}")]
    Busy(#[from] StoreBusy),
    /// The database refused, the connection dropped, or a row contradicted the schema.
    #[error("database error: {0}")]
    Database(String),
    /// A writer supplied data the schema requires but did not provide, such as a single
    /// price row with no venue. This is a programming error, never a caller's request shape.
    #[error("invalid store row: {0}")]
    Invalid(String),
}

impl StoreError {
    pub fn database(detail: impl Into<String>) -> Self {
        Self::Database(detail.into())
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Invalid(detail.into())
    }

    /// Whether the caller should be told to retry.
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Busy(_))
    }

    /// The text the API renders as `detail` for a 503.
    pub fn busy_detail(&self) -> Option<&str> {
        match self {
            Self::Busy(busy) => Some(&busy.detail),
            _ => None,
        }
    }
}

impl From<tiberius::error::Error> for StoreError {
    fn from(error: tiberius::error::Error) -> Self {
        classify_tiberius(&error)
    }
}

impl From<bb8::RunError<tiberius::error::Error>> for StoreError {
    fn from(error: bb8::RunError<tiberius::error::Error>) -> Self {
        match error {
            bb8::RunError::User(error) => error.into(),
            bb8::RunError::TimedOut => StoreError::Busy(StoreBusy::new(
                "no database connection became available before the checkout timeout",
            )),
        }
    }
}

/// Is this server error the lock wait the API reports as retryable?
pub(crate) fn is_lock_timeout(code: u32, message: &str) -> bool {
    code == LOCK_TIMEOUT_CODE || looks_like_lock_timeout(message)
}

/// The message half of the lock-timeout rule, for drivers that report text but not a code.
pub(crate) fn looks_like_lock_timeout(message: &str) -> bool {
    message.contains("Lock request time out")
}

/// Is this server error a duplicate key, i.e. the INSERT lost a race the UPDATE can settle?
pub(crate) fn is_duplicate_key(code: u32) -> bool {
    DUPLICATE_KEY_CODES.contains(&code)
}

/// Whether a driver error is a duplicate key, for callers that must retry rather than fail.
pub(crate) fn is_duplicate_key_error(error: &tiberius::error::Error) -> bool {
    matches!(error, tiberius::error::Error::Server(token) if is_duplicate_key(token.code()))
}

/// Turn a driver error into the two shapes the service distinguishes.
pub(crate) fn classify_tiberius(error: &tiberius::error::Error) -> StoreError {
    if let tiberius::error::Error::Server(token) = error {
        if is_lock_timeout(token.code(), token.message()) {
            return StoreError::Busy(StoreBusy::new(STORE_BUSY_DETAIL));
        }
        return StoreError::Database(token.to_string());
    }
    StoreError::Database(error.to_string())
}

/// The live connection the store borrows from its pool.
pub type SqlConnection = tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_1222_is_the_retryable_condition() {
        assert!(is_lock_timeout(1222, "anything"));
        assert!(!is_lock_timeout(1205, "deadlock victim"));
    }

    #[test]
    fn the_pyodbc_message_spelling_also_matches() {
        assert!(looks_like_lock_timeout(
            "Lock request time out period exceeded."
        ));
        assert!(!looks_like_lock_timeout("Lock request timed out."));
    }

    #[test]
    fn duplicate_key_codes_are_both_recognised() {
        assert!(is_duplicate_key(2601));
        assert!(is_duplicate_key(2627));
        assert!(!is_duplicate_key(1222));
    }

    #[test]
    fn a_busy_error_carries_the_exact_source_detail() {
        let error = StoreError::Busy(StoreBusy::new(STORE_BUSY_DETAIL));
        assert!(error.is_busy());
        assert_eq!(error.busy_detail(), Some(STORE_BUSY_DETAIL));
    }

    #[test]
    fn a_database_error_is_not_retryable() {
        let error = StoreError::database("connection reset");
        assert!(!error.is_busy());
        assert_eq!(error.busy_detail(), None);
        assert_eq!(error.to_string(), "database error: connection reset");
    }

    #[test]
    fn a_checkout_timeout_is_reported_as_busy() {
        let error: StoreError = bb8::RunError::TimedOut.into();
        assert!(error.is_busy());
    }

    #[test]
    fn an_invalid_row_names_what_was_missing() {
        let error = StoreError::invalid("a price row has no venue");
        assert_eq!(
            error.to_string(),
            "invalid store row: a price row has no venue"
        );
    }
}
