//! Configuration loaded from environment variables.

use thiserror::Error;

/// Runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub spotify_client_id: String,
    pub spotify_client_secret: String,
    pub spotify_redirect_uri: String,
    pub poll_interval_secs: u64,
    pub spotify_api_base: String,
    pub spotify_accounts_base: String,
}

/// Configuration error.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("invalid value for {0}: {1}")]
    Invalid(&'static str, String),
}

impl Config {
    /// Load the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Load the configuration from an arbitrary key lookup (testable).
    pub fn from_lookup<F>(get: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let or_default = |key: &str, default: &str| {
            get(key)
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        let required = |key: &'static str| {
            get(key)
                .filter(|v| !v.is_empty())
                .ok_or(ConfigError::Missing(key))
        };
        let poll = or_default("POLL_INTERVAL_SECS", "120");
        let poll_interval_secs = poll
            .parse::<u64>()
            .map_err(|e| ConfigError::Invalid("POLL_INTERVAL_SECS", e.to_string()))?;
        Ok(Self {
            database_url: or_default("DATABASE_URL", "sqlite://data/spotifast.db"),
            bind_addr: or_default("BIND_ADDR", "0.0.0.0:8080"),
            spotify_client_id: required("SPOTIFY_CLIENT_ID")?,
            spotify_client_secret: required("SPOTIFY_CLIENT_SECRET")?,
            spotify_redirect_uri: required("SPOTIFY_REDIRECT_URI")?,
            poll_interval_secs,
            spotify_api_base: or_default("SPOTIFY_API_BASE", "https://api.spotify.com/v1"),
            spotify_accounts_base: or_default(
                "SPOTIFY_ACCOUNTS_BASE",
                "https://accounts.spotify.com",
            ),
        })
    }

    /// A configuration suitable for tests (no network, dummy credentials).
    pub fn for_tests() -> Self {
        Self {
            database_url: "sqlite::memory:".into(),
            bind_addr: "127.0.0.1:0".into(),
            spotify_client_id: "test-client".into(),
            spotify_client_secret: "test-secret".into(),
            spotify_redirect_uri: "http://127.0.0.1/callback".into(),
            poll_interval_secs: 120,
            spotify_api_base: "http://127.0.0.1:1/v1".into(),
            spotify_accounts_base: "http://127.0.0.1:1".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |k| map.get(k).map(|v| v.to_string())
    }

    #[test]
    fn defaults_apply() {
        let c = Config::from_lookup(lookup(HashMap::from([
            ("SPOTIFY_CLIENT_ID", "a"),
            ("SPOTIFY_CLIENT_SECRET", "b"),
            ("SPOTIFY_REDIRECT_URI", "c"),
        ])))
        .unwrap();
        assert_eq!(c.database_url, "sqlite://data/spotifast.db");
        assert_eq!(c.bind_addr, "0.0.0.0:8080");
        assert_eq!(c.poll_interval_secs, 120);
        assert_eq!(c.spotify_api_base, "https://api.spotify.com/v1");
        assert_eq!(c.spotify_accounts_base, "https://accounts.spotify.com");
    }

    #[test]
    fn missing_spotify_vars_fail() {
        assert!(matches!(
            Config::from_lookup(lookup(HashMap::new())),
            Err(ConfigError::Missing("SPOTIFY_CLIENT_ID"))
        ));
    }

    #[test]
    fn bad_poll_interval_fails() {
        let r = Config::from_lookup(lookup(HashMap::from([
            ("SPOTIFY_CLIENT_ID", "a"),
            ("SPOTIFY_CLIENT_SECRET", "b"),
            ("SPOTIFY_REDIRECT_URI", "c"),
            ("POLL_INTERVAL_SECS", "abc"),
        ])));
        assert!(matches!(
            r,
            Err(ConfigError::Invalid("POLL_INTERVAL_SECS", _))
        ));
    }
}
