//! Turning deployment settings into a TDS connection configuration.
//!
//! The source service built an ODBC connection string from `MDS_DB_SERVER`, which accepts
//! `host`, `host\instance` and `host,port`. TDS needs the same three parts as separate
//! fields, so [`ServerAddress`] parses them once, with tests, instead of leaving each caller
//! to split on a backslash.
//!
//! Authentication is Windows integrated unless `MDS_DB_TRUSTED=0`, in which case SQL
//! credentials are required from `MDS_DB_USER`/`MDS_DB_PASSWORD`. The password is never
//! rendered by `Debug` or any error message.

use std::fmt;

use crate::config::Settings;

use super::error::StoreError;

/// The default SQL Server port. Named instances resolve theirs through SQL Browser.
pub const DEFAULT_PORT: u16 = 1433;

/// How long a pooled connection may be borrowed before the request is told to retry.
pub const POOL_MAX_SIZE: u32 = 8;

/// A SQL Server address in the three forms the ODBC `SERVER=` keyword accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    pub host: String,
    pub port: Option<u16>,
    pub instance: Option<String>,
}

impl ServerAddress {
    /// Parse `host`, `host\instance`, `host,port` or `host\instance,port`.
    ///
    /// A leading `tcp:` (which ADO-style strings allow) is accepted and ignored, because an
    /// operator copying a connection string should not be told the server is malformed.
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        let trimmed = value.trim();
        let without_protocol = trimmed
            .strip_prefix("tcp:")
            .or_else(|| trimmed.strip_prefix("TCP:"))
            .unwrap_or(trimmed)
            .trim();
        if without_protocol.is_empty() {
            return Err(StoreError::invalid("MDS_DB_SERVER is empty"));
        }

        let (host_part, instance_part) = match without_protocol.split_once('\\') {
            Some((host, rest)) => (host.trim(), Some(rest.trim())),
            None => (without_protocol, None),
        };
        let (host, port_text) = match host_part.split_once(',') {
            Some((host, port)) => (host.trim(), Some(port.trim())),
            None => (host_part.trim(), None),
        };
        if host.is_empty() {
            return Err(StoreError::invalid(format!(
                "MDS_DB_SERVER {value:?} names no host"
            )));
        }

        let (instance, port_from_instance) = match instance_part {
            Some(rest) if !rest.is_empty() => match rest.split_once(',') {
                Some((instance, port)) => (Some(instance.trim()), Some(port.trim())),
                None => (Some(rest), None),
            },
            _ => (None, None),
        };
        let port = parse_port(port_text.or(port_from_instance), value)?;

        Ok(Self {
            host: host.to_string(),
            port,
            instance: instance.filter(|name| !name.is_empty()).map(str::to_string),
        })
    }
}

/// A port is a positive `u16`, or the address is refused before a connection is attempted.
fn parse_port(text: Option<&str>, original: &str) -> Result<Option<u16>, StoreError> {
    match text {
        None | Some("") => Ok(None),
        Some(text) => text.parse::<u16>().map(Some).map_err(|_| {
            StoreError::invalid(format!(
                "MDS_DB_SERVER {original:?} has an invalid port {text:?}"
            ))
        }),
    }
}

/// Everything a connection needs. `Debug` deliberately redacts the password.
#[derive(Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    pub server: String,
    pub database: String,
    pub trusted: bool,
    pub trust_server_certificate: bool,
    pub user: String,
    pub password: String,
    pub timeout_seconds: u64,
}

impl DatabaseConfig {
    /// The deployment's settings, with the ODBC-only driver name deliberately dropped.
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            server: settings.db_server.clone(),
            database: settings.db_database.clone(),
            trusted: settings.db_trusted,
            trust_server_certificate: settings.db_trust_server_certificate,
            user: settings.db_user.clone(),
            password: settings.db_password.clone(),
            timeout_seconds: settings.db_timeout_seconds.max(1) as u64,
        }
    }

    /// The tiberius connection configuration.
    ///
    /// `MDS_DB_TRUST_CERT=0` leaves certificate validation on, matching
    /// `TrustServerCertificate=no`. The source service defaulted it to trust, and so does
    /// this one.
    pub fn tiberius_config(&self) -> Result<tiberius::Config, StoreError> {
        let address = ServerAddress::parse(&self.server)?;
        let mut config = tiberius::Config::new();
        config.host(&address.host);
        if let Some(port) = address.port {
            config.port(port);
        }
        if let Some(instance) = &address.instance {
            config.instance_name(instance);
        }
        config.database(&self.database);
        config.application_name("McTrader_Market_Data_Service");
        config.handshake_timeout(Some(std::time::Duration::from_secs(self.timeout_seconds)));
        // The source service left pyodbc's query timeout at its default (unbounded) and
        // relied on LOCK_TIMEOUT for the read-vs-writer case. Keep that contract rather
        // than inventing a command timeout a long daily MERGE might cross.
        config.command_timeout(None);

        if self.trusted {
            config.authentication(tiberius::AuthMethod::Integrated);
        } else {
            if self.user.trim().is_empty() || self.password.is_empty() {
                return Err(StoreError::invalid(
                    "MDS_DB_TRUSTED=0 requires MDS_DB_USER and MDS_DB_PASSWORD",
                ));
            }
            config.authentication(tiberius::AuthMethod::sql_server(&self.user, &self.password));
        }
        if self.trust_server_certificate {
            config.trust_cert();
        }
        Ok(config)
    }

    /// The pool's checkout timeout. A caller waiting longer than the login timeout is told
    /// to retry, exactly like a lock wait.
    pub fn pool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_seconds)
    }
}

impl fmt::Debug for DatabaseConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseConfig")
            .field("server", &self.server)
            .field("database", &self.database)
            .field("trusted", &self.trusted)
            .field("trust_server_certificate", &self.trust_server_certificate)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("timeout_seconds", &self.timeout_seconds)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DatabaseConfig {
        DatabaseConfig {
            server: "localhost".into(),
            database: "Market_Data".into(),
            trusted: true,
            trust_server_certificate: true,
            user: String::new(),
            password: String::new(),
            timeout_seconds: 30,
        }
    }

    #[test]
    fn a_bare_host_uses_the_default_port() {
        let address = ServerAddress::parse("localhost").unwrap();
        assert_eq!(address.host, "localhost");
        assert_eq!(address.port, None);
        assert_eq!(address.instance, None);
    }

    #[test]
    fn a_named_instance_is_kept_for_sql_browser() {
        let address = ServerAddress::parse(r"sqlhost\SQLEXPRESS").unwrap();
        assert_eq!(address.host, "sqlhost");
        assert_eq!(address.instance.as_deref(), Some("SQLEXPRESS"));
        assert_eq!(address.port, None);
    }

    #[test]
    fn an_explicit_port_is_parsed() {
        let address = ServerAddress::parse("sqlhost,1444").unwrap();
        assert_eq!(address.host, "sqlhost");
        assert_eq!(address.port, Some(1444));
    }

    #[test]
    fn an_instance_and_a_port_can_coexist() {
        let address = ServerAddress::parse(r"sqlhost\SQLEXPRESS,1444").unwrap();
        assert_eq!(address.host, "sqlhost");
        assert_eq!(address.instance.as_deref(), Some("SQLEXPRESS"));
        assert_eq!(address.port, Some(1444));
    }

    #[test]
    fn whitespace_and_the_tcp_prefix_are_tolerated() {
        let address = ServerAddress::parse("  tcp:sqlhost, 1444 ").unwrap();
        assert_eq!(address.host, "sqlhost");
        assert_eq!(address.port, Some(1444));
    }

    #[test]
    fn an_empty_server_is_refused() {
        assert!(matches!(
            ServerAddress::parse("   ").unwrap_err(),
            StoreError::Invalid(_)
        ));
    }

    #[test]
    fn an_invalid_port_is_refused_before_connecting() {
        assert!(matches!(
            ServerAddress::parse("sqlhost,not-a-port").unwrap_err(),
            StoreError::Invalid(_)
        ));
    }

    #[test]
    fn trusted_auth_is_the_default_configuration() {
        let tiberius = config().tiberius_config().unwrap();
        assert_eq!(tiberius.get_addr(), "localhost:1433");
    }

    #[test]
    fn a_named_instance_addresses_sql_browser() {
        let mut config = config();
        config.server = r"sqlhost\SQLEXPRESS".into();
        let tiberius = config.tiberius_config().unwrap();
        assert_eq!(tiberius.get_addr(), "sqlhost:1434");
    }

    #[test]
    fn sql_auth_requires_both_credentials() {
        let mut config = config();
        config.trusted = false;
        assert!(matches!(
            config.tiberius_config().unwrap_err(),
            StoreError::Invalid(_)
        ));
    }

    #[test]
    fn the_password_never_appears_in_debug() {
        let mut config = config();
        config.trusted = false;
        config.user = "reader".into();
        config.password = "super-secret".into();
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn settings_are_copied_across_without_the_odbc_driver_name() {
        let settings = Settings::from_lookup(
            &|name| match name {
                "MDS_DB_SERVER" => Some("sqlhost".to_string()),
                "MDS_DATABASE" => Some("MD".to_string()),
                _ => None,
            },
            None,
            &crate::config::NoKeyring,
        );
        let config = DatabaseConfig::from_settings(&settings);
        assert_eq!(config.server, "sqlhost");
        assert_eq!(config.database, "MD");
        assert!(config.trusted);
    }
}
