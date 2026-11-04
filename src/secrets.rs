//! The one secret this service owns, in the OS keyring.
//!
//! # Why this exists at all
//!
//! [`crate::config`] already falls back to the keyring, so why an endpoint? Because the keyring
//! is **per user**. A service running as `NT AUTHORITY\SYSTEM` cannot see an entry stored under
//! an interactive account, so the key has to get into *SYSTEM's* vault. The only process that
//! can write there is the service itself — so it needs a way to be told the key at runtime.
//!
//! That is also why this is better than passing `EODHD_API_KEY` in the service environment: NSSM
//! would store that value in plaintext in the registry, whereas the credential store is
//! encrypted by Windows and only readable by the account that owns it.
//!
//! # The delete-then-set pattern is not incidental
//!
//! [`EodhdKeyStore::write`] deletes any existing entry before setting the new one. Windows
//! Credential Manager treats a write over an existing entry inconsistently and can end up
//! holding *both* the old and the new value under the same target, after which a read may
//! return either. The source service carries the same guard for the same reason.
//!
//! # Never the key
//!
//! The key is only ever returned by [`EodhdKeyStore::read`], to config resolution or to the
//! mask builder. No log line, no response body and no error message contains it.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use keyring_core::Entry;
use serde::Serialize;

use crate::config::{KeyringSource, KEYRING_SERVICE, KEYRING_USERNAME};

/// Why a secret could not be stored. The API maps these to distinct bodies: an unavailable
/// vault is a 503, a blank key is a caller's 400, and a refused write is the vault's 503.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyringFailureKind {
    Unavailable,
    InvalidKey,
    Rejected,
}

impl KeyringFailureKind {
    /// The `/secrets/eodhd` reason string for this failure.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Unavailable | Self::Rejected => "keyring-unavailable",
            Self::InvalidKey => "invalid-key",
        }
    }

    /// The status `/secrets/eodhd` answers with.
    pub fn status(self) -> u16 {
        match self {
            Self::Unavailable | Self::Rejected => 503,
            Self::InvalidKey => 400,
        }
    }
}

/// The stored key could not be written or the vault could not answer usefully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyringFailure {
    pub kind: KeyringFailureKind,
    pub detail: String,
}

impl KeyringFailure {
    fn new(kind: KeyringFailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for KeyringFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for KeyringFailure {}

/// A credential vault, narrowed to the one entry this service owns.
///
/// The trait exists so a test can run without a real Windows vault, exactly as the source
/// suite injected a fake `keyring` module. It is also what keeps the no-test-reads-the-real-key
/// rule true by construction: only [`WindowsCredentialBackend`] touches Credential Manager.
pub trait CredentialBackend: Send + Sync {
    /// Whether a vault is reachable at all.
    fn available(&self) -> bool;

    /// The backend's name, for `/secrets/eodhd` diagnostics.
    fn describe(&self) -> Option<String>;

    /// The stored value, or `None` when there is no entry or the vault refused to answer.
    fn read(&self, service: &str, user: &str) -> Option<String>;

    /// Store (or replace) a value. The caller has already deleted any old entry.
    fn write(&self, service: &str, user: &str, value: &str) -> Result<(), KeyringFailure>;

    /// Remove the entry; `true` when something was there. Absent is not an error.
    fn delete(&self, service: &str, user: &str) -> bool;
}

/// A backend that is never available. Used when the vault cannot be initialised, and by tests
/// that only exercise the caller's response to an unavailable vault.
#[derive(Debug, Clone, Default)]
pub struct UnavailableBackend {
    pub detail: String,
}

impl CredentialBackend for UnavailableBackend {
    fn available(&self) -> bool {
        false
    }

    fn describe(&self) -> Option<String> {
        None
    }

    fn read(&self, _service: &str, _user: &str) -> Option<String> {
        None
    }

    fn write(&self, _service: &str, _user: &str, _value: &str) -> Result<(), KeyringFailure> {
        Err(KeyringFailure::new(
            KeyringFailureKind::Unavailable,
            if self.detail.is_empty() {
                "no keyring backend is available"
            } else {
                self.detail.as_str()
            },
        ))
    }

    fn delete(&self, _service: &str, _user: &str) -> bool {
        false
    }
}

/// Windows Credential Manager, addressed by the same target the Python service wrote.
///
/// Windows normally derives a target of `user.service`; the source service used the bare
/// service string as `TargetName`, so the entry is built with the `target` modifier, which the
/// Windows store uses verbatim.
pub struct WindowsCredentialBackend;

impl WindowsCredentialBackend {
    /// Install the Windows store as the process default, once.
    fn ensure_default_store() -> Result<(), KeyringFailure> {
        static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();
        let result = INSTALLED.get_or_init(|| {
            let store = windows_native_keyring_store::Store::new()
                .map_err(|error| format!("could not open the Windows credential store: {error}"))?;
            keyring_core::set_default_store(store);
            Ok(())
        });
        result
            .clone()
            .map_err(|detail| KeyringFailure::new(KeyringFailureKind::Unavailable, detail))
    }

    fn entry(service: &str, user: &str) -> Result<Entry, KeyringFailure> {
        Self::ensure_default_store()?;
        let modifiers = HashMap::from([("target", service)]);
        Entry::new_with_modifiers(service, user, &modifiers).map_err(|error| {
            KeyringFailure::new(
                KeyringFailureKind::Unavailable,
                format!("could not address the keyring entry: {error}"),
            )
        })
    }
}

impl CredentialBackend for WindowsCredentialBackend {
    fn available(&self) -> bool {
        Self::ensure_default_store().is_ok()
    }

    fn describe(&self) -> Option<String> {
        self.available()
            .then(|| "windows-native-keyring-store".to_string())
    }

    fn read(&self, service: &str, user: &str) -> Option<String> {
        let entry = Self::entry(service, user).ok()?;
        match entry.get_password() {
            Ok(value) => Some(value),
            Err(keyring_core::Error::NoEntry) => None,
            Err(error) => {
                tracing::warn!("keyring read failed: {error}");
                None
            }
        }
    }

    fn write(&self, service: &str, user: &str, value: &str) -> Result<(), KeyringFailure> {
        let entry = Self::entry(service, user)?;
        // Delete first so Windows cannot keep both the old and the new value.
        let _ = entry.delete_credential();
        entry.set_password(value).map_err(|error| {
            KeyringFailure::new(
                KeyringFailureKind::Rejected,
                format!("could not write to the keyring: {error}"),
            )
        })
    }

    fn delete(&self, service: &str, user: &str) -> bool {
        let Ok(entry) = Self::entry(service, user) else {
            return false;
        };
        entry.delete_credential().is_ok()
    }
}

/// The EODHD key in `market_data:eodhd:global` / `eodhd:api_key`, shared with McTrader.
pub struct EodhdKeyStore {
    backend: Arc<dyn CredentialBackend>,
}

impl EodhdKeyStore {
    /// The real Windows vault.
    pub fn windows() -> Self {
        Self {
            backend: Arc::new(WindowsCredentialBackend),
        }
    }

    /// A store over an injected vault, for tests and for deployments with no vault.
    pub fn with_backend(backend: Arc<dyn CredentialBackend>) -> Self {
        Self { backend }
    }

    pub fn available(&self) -> bool {
        self.backend.available()
    }

    /// The stored key, or `None`. Never logged or returned anywhere else.
    pub fn read(&self) -> Option<String> {
        self.backend
            .read(KEYRING_SERVICE, KEYRING_USERNAME)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Store (or replace) the key. Deletes first — see the module docstring.
    pub fn write(&self, value: &str) -> Result<(), KeyringFailure> {
        if !self.backend.available() {
            return Err(KeyringFailure::new(
                KeyringFailureKind::Unavailable,
                "no keyring backend is available, so the key cannot be stored; \
                 set EODHD_API_KEY in the service environment instead",
            ));
        }
        let cleaned = value.trim();
        if cleaned.is_empty() {
            return Err(KeyringFailure::new(
                KeyringFailureKind::InvalidKey,
                "an empty API key is not a key",
            ));
        }
        let _ = self.backend.delete(KEYRING_SERVICE, KEYRING_USERNAME);
        self.backend
            .write(KEYRING_SERVICE, KEYRING_USERNAME, cleaned)?;
        tracing::info!("EODHD key written to the keyring ({KEYRING_SERVICE}/{KEYRING_USERNAME})");
        Ok(())
    }

    /// Remove the key. `true` when something was there.
    pub fn delete(&self) -> bool {
        self.backend.delete(KEYRING_SERVICE, KEYRING_USERNAME)
    }

    /// Whether a key is stored, and a mask — never the key itself.
    pub fn status(&self) -> KeyStoreStatus {
        let key = self.read();
        let masked = key.as_ref().map(|key| mask(key));
        KeyStoreStatus {
            stored: key.is_some(),
            masked,
            length: key.as_ref().map(|key| key.chars().count()).unwrap_or(0),
            keyring_service: KEYRING_SERVICE,
            keyring_username: KEYRING_USERNAME,
            backend: self.backend.describe(),
            keyring_available: self.backend.available(),
        }
    }
}

impl Default for EodhdKeyStore {
    fn default() -> Self {
        Self::windows()
    }
}

/// The configuration chain's view of the vault: the same entry `/secrets/eodhd` writes.
///
/// This is the link that makes `Settings::from_env(&EodhdKeyStore::windows())` resolve
/// `EODHD_API_KEY` -> keyring -> none without config depending on the keyring crate.
impl KeyringSource for EodhdKeyStore {
    fn stored_key(&self) -> Option<String> {
        self.read()
    }
}

/// The last four characters after a field of asterisks: enough to recognise the key, useless
/// to anyone who intercepts it.
pub fn mask(key: &str) -> String {
    let characters: Vec<char> = key.chars().collect();
    let keep = characters.len().saturating_sub(4);
    let mut masked = "*".repeat(keep);
    masked.extend(characters.iter().skip(keep));
    masked
}

/// The `/secrets/eodhd` status body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyStoreStatus {
    pub stored: bool,
    pub masked: Option<String>,
    pub length: usize,
    pub keyring_service: &'static str,
    pub keyring_username: &'static str,
    pub backend: Option<String>,
    pub keyring_available: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryBackend {
        value: Mutex<Option<String>>,
        calls: Mutex<Vec<String>>,
        available: bool,
    }

    impl MemoryBackend {
        fn available() -> Arc<Self> {
            Arc::new(Self {
                available: true,
                ..Self::default()
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CredentialBackend for MemoryBackend {
        fn available(&self) -> bool {
            self.available
        }

        fn describe(&self) -> Option<String> {
            self.available.then(|| "memory".to_string())
        }

        fn read(&self, _service: &str, _user: &str) -> Option<String> {
            self.value.lock().unwrap().clone()
        }

        fn write(&self, _service: &str, _user: &str, value: &str) -> Result<(), KeyringFailure> {
            self.calls.lock().unwrap().push(format!("write:{value}"));
            *self.value.lock().unwrap() = Some(value.to_string());
            Ok(())
        }

        fn delete(&self, _service: &str, _user: &str) -> bool {
            self.calls.lock().unwrap().push("delete".to_string());
            self.value.lock().unwrap().take().is_some()
        }
    }

    #[test]
    fn a_stored_key_reads_back_and_is_masked() {
        let backend = MemoryBackend::available();
        let store = EodhdKeyStore::with_backend(backend.clone());
        store.write("abcdefgh1234").unwrap();

        assert_eq!(store.read().as_deref(), Some("abcdefgh1234"));
        let status = store.status();
        assert!(status.stored);
        assert_eq!(status.masked.as_deref(), Some("********1234"));
        assert_eq!(status.length, 12);
        assert_eq!(status.keyring_service, "market_data:eodhd:global");
        assert_eq!(status.keyring_username, "eodhd:api_key");
        assert_eq!(status.backend.as_deref(), Some("memory"));
        assert!(status.keyring_available);
    }

    #[test]
    fn writing_deletes_the_old_entry_first() {
        let backend = MemoryBackend::available();
        let store = EodhdKeyStore::with_backend(backend.clone());
        store.write("first-key").unwrap();
        store.write("second-key").unwrap();

        assert_eq!(store.read().as_deref(), Some("second-key"));
        assert_eq!(
            backend.calls(),
            vec!["delete", "write:first-key", "delete", "write:second-key"]
        );
    }

    #[test]
    fn a_blank_key_is_refused_without_touching_the_vault() {
        let backend = MemoryBackend::available();
        let store = EodhdKeyStore::with_backend(backend.clone());

        let error = store.write("   ").unwrap_err();
        assert!(error.detail.contains("empty API key"));
        assert!(backend.calls().is_empty());
    }

    #[test]
    fn an_unavailable_vault_refuses_the_write_and_says_what_to_do_instead() {
        let store = EodhdKeyStore::with_backend(Arc::new(UnavailableBackend::default()));

        let error = store.write("a-key").unwrap_err();
        assert!(error.detail.contains("no keyring backend"));
        assert!(error.detail.contains("EODHD_API_KEY"));
        assert!(!store.status().keyring_available);
        assert_eq!(store.status().masked, None);
    }

    #[test]
    fn deleting_reports_whether_something_was_there() {
        let backend = MemoryBackend::available();
        let store = EodhdKeyStore::with_backend(backend);
        store.write("a-key").unwrap();

        assert!(store.delete());
        assert!(!store.delete());
        assert_eq!(store.read(), None);
    }

    #[test]
    fn the_mask_keeps_only_the_last_four_characters() {
        assert_eq!(mask("abcdefgh1234"), "********1234");
        assert_eq!(mask("abcd"), "abcd");
        assert_eq!(mask("ab"), "ab");
        assert_eq!(mask(""), "");
    }

    #[test]
    fn the_key_store_answers_the_configuration_chain() {
        let backend = MemoryBackend::available();
        let store = EodhdKeyStore::with_backend(backend);
        store.write("key-from-the-vault").unwrap();

        let settings = crate::config::Settings::from_lookup(&|_| None, None, &store);

        assert_eq!(settings.eodhd_api_key, "key-from-the-vault");
        assert_eq!(settings.eodhd_key_source, crate::config::KeySource::Keyring);
    }

    #[test]
    fn a_whitespace_only_entry_counts_as_absent() {
        let backend = MemoryBackend::available();
        *backend.value.lock().unwrap() = Some("   ".to_string());
        let store = EodhdKeyStore::with_backend(backend);

        assert_eq!(store.read(), None);
        assert!(!store.status().stored);
    }
}
