//! Thin HTTP client for the Spotify accounts service and Web API.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;

use crate::AppState;

pub const SCOPES: &str = "user-read-recently-played user-read-private";

/// Failure talking to Spotify.
#[derive(Debug)]
pub enum ClientError {
    /// HTTP 429, with the `Retry-After` delay when Spotify gave one.
    RateLimited(Option<Duration>),
    /// Any other failure (network, non-2xx status, malformed body).
    Other(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited(Some(d)) => write!(f, "rate limited by Spotify (retry in {d:?})"),
            Self::RateLimited(None) => write!(f, "rate limited by Spotify"),
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        Self::Other(e.without_url().to_string())
    }
}

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

async fn parse<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    what: &str,
) -> Result<T, ClientError> {
    let status = resp.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        let delay = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(ClientError::RateLimited(delay));
    }
    if !status.is_success() {
        return Err(ClientError::Other(format!(
            "Spotify {what} failed with status {status}"
        )));
    }
    resp.json::<T>()
        .await
        .map_err(|e| ClientError::Other(format!("Spotify {what}: invalid response: {e}")))
}

async fn token_request(
    state: &AppState,
    form: &[(&str, &str)],
    what: &str,
) -> Result<TokenResponse, ClientError> {
    let url = format!("{}/api/token", state.config.spotify_accounts_base);
    let resp = state
        .http
        .post(url)
        .basic_auth(
            &state.config.spotify_client_id,
            Some(&state.config.spotify_client_secret),
        )
        .form(form)
        .send()
        .await?;
    parse(resp, what).await
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
    let resp = state
        .http
        .get(format!("{}/me", state.config.spotify_api_base))
        .bearer_auth(access_token)
        .send()
        .await?;
    parse(resp, "profile").await
}

pub async fn get_recently_played(
    state: &AppState,
    access_token: &str,
    after_ms: Option<i64>,
) -> Result<RecentlyPlayed, ClientError> {
    let mut req = state
        .http
        .get(format!(
            "{}/me/player/recently-played",
            state.config.spotify_api_base
        ))
        .bearer_auth(access_token)
        .query(&[("limit", "50")]);
    if let Some(after) = after_ms {
        req = req.query(&[("after", after.to_string())]);
    }
    parse(req.send().await?, "recently-played").await
}
