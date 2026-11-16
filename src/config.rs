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
    /// Minimum (and initial) poll interval per user, in seconds (`POLL_INTERVAL_SECS`).
    pub poll_interval_secs: u64,
    /// Maximum poll interval per user when they are inactive (`POLL_MAX_INTERVAL_SECS`).
    pub poll_max_interval_secs: u64,
    pub spotify_api_base: String,
    pub spotify_accounts_base: String,
    /// `ALLOWED_SPOTIFY_IDS` (comma-separated): when set, only these Spotify
    /// accounts may log in. `None` = anyone Spotify lets through.
    pub allowed_spotify_ids: Option<Vec<String>>,
    /// `TRUST_X_FORWARDED_FOR`: take the client IP from `X-Forwarded-For`
    /// (only behind a reverse proxy that sets it).
    pub trust_x_forwarded_for: bool,
    /// `AUTH_RATE_LIMIT_PER_MIN`: requests per minute and per client IP
    /// allowed on `/auth/login` and `/auth/callback`.
    pub auth_rate_limit_per_min: u32,
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
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| ConfigError::Invalid("POLL_INTERVAL_SECS", poll.clone()))?;
        let poll_max = or_default("POLL_MAX_INTERVAL_SECS", "600");
        let poll_max_interval_secs = poll_max
            .parse::<u64>()
            .ok()
            .filter(|v| *v >= poll_interval_secs)
            .ok_or_else(|| {
                ConfigError::Invalid(
                    "POLL_MAX_INTERVAL_SECS",
                    format!("{poll_max} (must be >= POLL_INTERVAL_SECS)"),
                )
            })?;
        let allowed_spotify_ids = get("ALLOWED_SPOTIFY_IDS")
            .map(|v| {
                v.split(',')
                    .map(|id| id.trim().to_string())
                    .filter(|id| !id.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|ids| !ids.is_empty());
        let trust = or_default("TRUST_X_FORWARDED_FOR", "false");
        let trust_x_forwarded_for = match trust.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => true,
            "0" | "false" | "no" => false,
            _ => return Err(ConfigError::Invalid("TRUST_X_FORWARDED_FOR", trust)),
        };
        let rate = or_default("AUTH_RATE_LIMIT_PER_MIN", "20");
        let auth_rate_limit_per_min = rate
            .parse::<u32>()
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| ConfigError::Invalid("AUTH_RATE_LIMIT_PER_MIN", rate.clone()))?;
        Ok(Self {
            database_url: or_default("DATABASE_URL", "sqlite://data/spotifast.db"),
            bind_addr: or_default("BIND_ADDR", "0.0.0.0:8080"),
            spotify_client_id: required("SPOTIFY_CLIENT_ID")?,
            spotify_client_secret: required("SPOTIFY_CLIENT_SECRET")?,
            spotify_redirect_uri: required("SPOTIFY_REDIRECT_URI")?,
            poll_interval_secs,
            poll_max_interval_secs,
            spotify_api_base: or_default("SPOTIFY_API_BASE", "https://api.spotify.com/v1"),
            spotify_accounts_base: or_default(
                "SPOTIFY_ACCOUNTS_BASE",
                "https://accounts.spotify.com",
            ),
            allowed_spotify_ids,
            trust_x_forwarded_for,
            auth_rate_limit_per_min,
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
            poll_max_interval_secs: 600,
            spotify_api_base: "http://127.0.0.1:1/v1".into(),
            spotify_accounts_base: "http://127.0.0.1:1".into(),
            allowed_spotify_ids: None,
            trust_x_forwarded_for: false,
            auth_rate_limit_per_min: 20,
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
        assert_eq!(c.poll_max_interval_secs, 600);
        assert_eq!(c.allowed_spotify_ids, None);
        assert!(!c.trust_x_forwarded_for);
        assert_eq!(c.auth_rate_limit_per_min, 20);
    }

    #[test]
    fn optional_vars_parse() {
        let c = Config::from_lookup(lookup(HashMap::from([
            ("SPOTIFY_CLIENT_ID", "a"),
            ("SPOTIFY_CLIENT_SECRET", "b"),
            ("SPOTIFY_REDIRECT_URI", "c"),
            ("POLL_INTERVAL_SECS", "60"),
            ("POLL_MAX_INTERVAL_SECS", "900"),
            ("ALLOWED_SPOTIFY_IDS", " alice, bob ,,"),
            ("TRUST_X_FORWARDED_FOR", "true"),
            ("AUTH_RATE_LIMIT_PER_MIN", "5"),
        ])))
        .unwrap();
        assert_eq!(c.poll_interval_secs, 60);
        assert_eq!(c.poll_max_interval_secs, 900);
        assert_eq!(
            c.allowed_spotify_ids,
            Some(vec!["alice".to_string(), "bob".to_string()])
        );
        assert!(c.trust_x_forwarded_for);
        assert_eq!(c.auth_rate_limit_per_min, 5);
    }

    #[test]
    fn max_interval_below_min_fails() {
        let r = Config::from_lookup(lookup(HashMap::from([
            ("SPOTIFY_CLIENT_ID", "a"),
            ("SPOTIFY_CLIENT_SECRET", "b"),
            ("SPOTIFY_REDIRECT_URI", "c"),
            ("POLL_INTERVAL_SECS", "300"),
            ("POLL_MAX_INTERVAL_SECS", "120"),
        ])));
        assert!(matches!(
            r,
            Err(ConfigError::Invalid("POLL_MAX_INTERVAL_SECS", _))
        ));
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
