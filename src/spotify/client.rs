//! Thin HTTP client for the Spotify accounts service and Web API.
//!
//! Failures are classified in [`ClientError`]. Its messages never contain a
//! token, an authorization code or a response body: only Spotify's own short
//! error message, truncated to [`MAX_MESSAGE_CHARS`] characters.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Deserialize;
use serde_json::Value;

use crate::AppState;

pub const SCOPES: &str = "user-read-recently-played user-read-private user-read-currently-playing";

/// Page size asked from `recently-played`, Spotify's maximum.
///
/// It is also the whole history Spotify exposes: only the last 50 plays can be
/// read, `after` and `before` only filter them.
pub const RECENTLY_PLAYED_LIMIT: usize = 50;
/// Spotify's error messages are cut to this many characters.
pub const MAX_MESSAGE_CHARS: usize = 200;
/// No Spotify request may hold the poller longer than this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Cap on `Retry-After` (one year), only to keep the time arithmetic in range.
const MAX_RETRY_AFTER_SECS: f64 = 365.0 * 24.0 * 3600.0;

/// Failure talking to Spotify.
#[derive(Debug)]
pub enum ClientError {
    /// HTTP 429, with the `Retry-After` delay when Spotify sent a usable one.
    RateLimited { retry_after: Option<Duration> },
    /// HTTP 401 from the Web API: the access token is expired or invalid.
    Unauthorized,
    /// The accounts service answered `invalid_grant`: the refresh token (or the
    /// authorization code) is revoked, expired or already used.
    InvalidGrant,
    /// Any other non-2xx status, with Spotify's short error message.
    Status {
        what: &'static str,
        status: u16,
        message: String,
    },
    /// The request did not complete (connection, DNS, TLS, timeout).
    Network { what: &'static str, detail: String },
    /// A 2xx answer whose body is not what Spotify documents.
    InvalidResponse { what: &'static str, detail: String },
}

impl ClientError {
    /// Short machine-readable category, used in logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rate_limited",
            Self::Unauthorized => "unauthorized",
            Self::InvalidGrant => "invalid_grant",
            Self::Status { .. } => "http_status",
            Self::Network { .. } => "network",
            Self::InvalidResponse { .. } => "invalid_response",
        }
    }

    /// HTTP status Spotify answered with, when there was an error status.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::RateLimited { .. } => Some(429),
            Self::Unauthorized => Some(401),
            Self::InvalidGrant => Some(400),
            Self::Status { status, .. } => Some(*status),
            Self::Network { .. } | Self::InvalidResponse { .. } => None,
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited {
                retry_after: Some(d),
            } => write!(
                f,
                "rate limited by Spotify (HTTP 429, Retry-After {} s)",
                d.as_secs()
            ),
            Self::RateLimited { retry_after: None } => write!(
                f,
                "rate limited by Spotify (HTTP 429, no usable Retry-After)"
            ),
            Self::Unauthorized => write!(f, "Spotify rejected the access token (HTTP 401)"),
            Self::InvalidGrant => write!(
                f,
                "Spotify rejected the grant (invalid_grant): it is revoked, expired or already used"
            ),
            Self::Status {
                what,
                status,
                message,
            } => write!(f, "Spotify {what} failed with HTTP {status}: {message}"),
            Self::Network { what, detail } => {
                write!(f, "network error during Spotify {what}: {detail}")
            }
            Self::InvalidResponse { what, detail } => {
                write!(f, "unexpected Spotify {what} response: {detail}")
            }
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: i64,
    /// Absent on refresh when Spotify keeps the same refresh token.
    pub refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Profile {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RecentlyPlayed {
    pub items: Vec<PlayItem>,
}

#[derive(Debug, Deserialize)]
pub struct PlayItem {
    pub played_at: String,
    pub track: Option<TrackObject>,
}

#[derive(Debug, Deserialize)]
pub struct TrackObject {
    /// `null` for local files.
    pub id: Option<String>,
    pub name: String,
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub artists: Vec<ArtistObject>,
    pub album: Option<AlbumObject>,
}

#[derive(Debug, Deserialize)]
pub struct ArtistObject {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct AlbumObject {
    pub name: String,
}

/// Body of `GET /me/player/currently-playing` (only what is used).
#[derive(Debug, Deserialize)]
pub struct CurrentlyPlaying {
    pub is_playing: bool,
    pub progress_ms: Option<i64>,
    /// `track`, `episode`, `ad` or `unknown`.
    pub currently_playing_type: Option<String>,
    /// `null` when Spotify cannot say (e.g. a private session).
    pub item: Option<TrackObject>,
}

/// Which Spotify service answered: a 400 or a 401 means something different
/// from each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Service {
    Accounts,
    WebApi,
}

/// `s` without control characters, cut to `max_chars` characters ("…" marks a cut).
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, c) in s.chars().filter(|c| !c.is_control()).enumerate() {
        if count == max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// Spotify's error code and short message, read from an error body.
///
/// The body is `{"error": {"status": 403, "message": "…"}}` (Web API) or
/// `{"error": "invalid_grant", "error_description": "…"}` (accounts service).
/// Nothing else of it is kept.
#[derive(Debug, Default, PartialEq, Eq)]
struct SpotifyError {
    code: Option<String>,
    message: Option<String>,
}

impl SpotifyError {
    fn from_body(body: &[u8]) -> Self {
        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            return Self::default();
        };
        match v.get("error") {
            Some(Value::Object(e)) => Self {
                code: None,
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .map(|m| truncate_chars(m, MAX_MESSAGE_CHARS)),
            },
            Some(Value::String(code)) => {
                let message = match v.get("error_description").and_then(Value::as_str) {
                    Some(d) if !d.is_empty() => format!("{code}: {d}"),
                    _ => code.clone(),
                };
                Self {
                    code: Some(code.clone()),
                    message: Some(truncate_chars(&message, MAX_MESSAGE_CHARS)),
                }
            }
            _ => Self::default(),
        }
    }
}

/// The `Retry-After` delay, when present and positive.
///
/// Spotify sends seconds. Fractions are rounded up so the wait is never
/// shorter than asked.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs: f64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if secs.is_finite() && secs > 0.0 {
        Some(Duration::from_secs_f64(
            secs.ceil().min(MAX_RETRY_AFTER_SECS),
        ))
    } else {
        None
    }
}

/// A transport failure, without the URL.
fn network(what: &'static str, e: reqwest::Error) -> ClientError {
    let e = e.without_url();
    let mut detail = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        detail.push_str(": ");
        detail.push_str(&s.to_string());
        source = std::error::Error::source(s);
    }
    ClientError::Network {
        what,
        detail: truncate_chars(&detail, MAX_MESSAGE_CHARS),
    }
}

/// Where a body failed to parse. serde's own message is left out: it can quote
/// the body, which may hold a token.
fn json_error(e: &serde_json::Error) -> String {
    let category = e.classify();
    let (line, column) = (e.line(), e.column());
    format!("{category:?} error at line {line}, column {column}")
}

async fn send(
    req: reqwest::RequestBuilder,
    what: &'static str,
) -> Result<reqwest::Response, ClientError> {
    req.timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| network(what, e))
}

async fn parse<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    service: Service,
    what: &'static str,
) -> Result<T, ClientError> {
    let status = resp.status();
    if status.is_success() {
        let body = resp.bytes().await.map_err(|e| network(what, e))?;
        return serde_json::from_slice(&body).map_err(|e| ClientError::InvalidResponse {
            what,
            detail: json_error(&e),
        });
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(ClientError::RateLimited {
            retry_after: retry_after(resp.headers()),
        });
    }
    if status == StatusCode::UNAUTHORIZED && service == Service::WebApi {
        return Err(ClientError::Unauthorized);
    }
    let body = resp.bytes().await.unwrap_or_default();
    let error = SpotifyError::from_body(&body);
    if status == StatusCode::BAD_REQUEST
        && service == Service::Accounts
        && error.code.as_deref() == Some("invalid_grant")
    {
        return Err(ClientError::InvalidGrant);
    }
    let message = error.message.unwrap_or_else(|| {
        status
            .canonical_reason()
            .unwrap_or("no error message")
            .to_string()
    });
    Err(ClientError::Status {
        what,
        status: status.as_u16(),
        message,
    })
}

async fn token_request(
    state: &AppState,
    form: &[(&str, &str)],
    what: &'static str,
) -> Result<TokenResponse, ClientError> {
    let url = format!("{}/api/token", state.config.spotify_accounts_base);
    let req = state
        .http
        .post(url)
        .basic_auth(
            &state.config.spotify_client_id,
            Some(&state.config.spotify_client_secret),
        )
        .form(form);
    parse(send(req, what).await?, Service::Accounts, what).await
}

pub async fn exchange_code(state: &AppState, code: &str) -> Result<TokenResponse, ClientError> {
    token_request(
        state,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &state.config.spotify_redirect_uri),
        ],
        "token exchange",
    )
    .await
}

pub async fn refresh_token(
    state: &AppState,
    refresh_token: &str,
) -> Result<TokenResponse, ClientError> {
    token_request(
        state,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
        "token refresh",
    )
    .await
}

pub async fn get_profile(state: &AppState, access_token: &str) -> Result<Profile, ClientError> {
    let req = state
        .http
        .get(format!("{}/me", state.config.spotify_api_base))
        .bearer_auth(access_token);
    parse(send(req, "profile").await?, Service::WebApi, "profile").await
}

/// One page of the user's last plays: with `after_ms`, only those played
/// strictly after it (the `after` cursor is exclusive).
pub async fn get_recently_played(
    state: &AppState,
    access_token: &str,
    after_ms: Option<i64>,
) -> Result<RecentlyPlayed, ClientError> {
    let what = "recently-played";
    let mut req = state
        .http
        .get(format!(
            "{}/me/player/recently-played",
            state.config.spotify_api_base
        ))
        .bearer_auth(access_token)
        .query(&[("limit", RECENTLY_PLAYED_LIMIT.to_string())]);
    if let Some(after) = after_ms {
        req = req.query(&[("after", after.to_string())]);
    }
    parse(send(req, what).await?, Service::WebApi, what).await
}

/// What the user is playing right now: one call, `None` on HTTP 204 (nothing
/// is playing). A 403 comes back as [`ClientError::Status`] (token without the
/// `user-read-currently-playing` scope).
pub async fn get_currently_playing(
    state: &AppState,
    access_token: &str,
) -> Result<Option<CurrentlyPlaying>, ClientError> {
    let what = "currently-playing";
    let req = state
        .http
        .get(format!(
            "{}/me/player/currently-playing",
            state.config.spotify_api_base
        ))
        .bearer_auth(access_token);
    let resp = send(req, what).await?;
    if resp.status() == StatusCode::NO_CONTENT {
        return Ok(None);
    }
    parse(resp, Service::WebApi, what).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;
    use serde_json::json;

    fn retry_after_header(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn retry_after_is_read_in_seconds_when_usable() {
        let secs = |v| retry_after(&retry_after_header(v)).map(|d| d.as_secs());
        assert_eq!(secs("27000"), Some(27_000));
        assert_eq!(secs("7"), Some(7));
        assert_eq!(secs("1.2"), Some(2), "rounded up, never shorter");
        assert_eq!(secs("0"), None);
        assert_eq!(secs("-5"), None);
        assert_eq!(secs("NaN"), None);
        assert_eq!(secs("soon"), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn error_body_keeps_only_spotifys_short_message() {
        let web = br#"{"error":{"status":403,"message":"Check settings"},"debug":"secret"}"#;
        assert_eq!(
            SpotifyError::from_body(web),
            SpotifyError {
                code: None,
                message: Some("Check settings".to_string()),
            }
        );
        let accounts = br#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#;
        assert_eq!(
            SpotifyError::from_body(accounts),
            SpotifyError {
                code: Some("invalid_grant".to_string()),
                message: Some("invalid_grant: Refresh token revoked".to_string()),
            }
        );
        assert_eq!(
            SpotifyError::from_body(b"<html>Bad gateway</html>"),
            SpotifyError::default()
        );
        let long = json!({"error": {"status": 500, "message": "é".repeat(500)}}).to_string();
        let message = SpotifyError::from_body(long.as_bytes()).message.unwrap();
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(message.ends_with('…'));
    }

    #[test]
    fn truncate_chars_cuts_and_drops_control_characters() {
        assert_eq!(truncate_chars("a\nb\tc", 10), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        assert_eq!(truncate_chars("abc", 3), "abc");
    }

    #[test]
    fn errors_have_a_kind_and_a_status() {
        let e = ClientError::Status {
            what: "recently-played",
            status: 503,
            message: "Service unavailable".into(),
        };
        assert_eq!(e.kind(), "http_status");
        assert_eq!(e.status(), Some(503));
        assert_eq!(
            e.to_string(),
            "Spotify recently-played failed with HTTP 503: Service unavailable"
        );
        assert_eq!(ClientError::InvalidGrant.kind(), "invalid_grant");
        assert_eq!(ClientError::Unauthorized.status(), Some(401));
    }
}
