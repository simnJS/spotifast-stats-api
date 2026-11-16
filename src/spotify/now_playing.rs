//! `GET /now-playing`: what the user is listening to, without spending quota.
//!
//! Spotify's quota is shared by the whole app, so every call is justified:
//! - an answer (a track, or "nothing playing") is kept in memory per user for
//!   [`CACHE_TTL_MS`]; within that time no Spotify call is made;
//! - while the user is rate limited (`rate_limited_until_ms` in the future) no
//!   call is made either, not even a token refresh: the last cached answer is
//!   returned (however old), or 503 with `Retry-After` when there is none;
//! - a 429 received here is stored like the poller stores its own
//!   ([`record_failure`]);
//! - the token is refreshed only when it is about to expire, by the poller's
//!   own code ([`valid_access_token`]);
//! - a 403 means the token was issued before `user-read-currently-playing` was
//!   requested: the user must log in again. It is remembered for that access
//!   token, so it is not asked again before a new login.
//!
//! Concurrent requests of one user queue on that user's slot, so a burst
//! causes one Spotify call, not one per request.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use super::client::{self, ClientError, CurrentlyPlaying};
use super::poller::{PollError, UserState, record_failure, valid_access_token};
use crate::AppState;
use crate::auth::AuthUser;
use crate::error::AppError;

/// How long an answer is served without asking Spotify again.
const CACHE_TTL_MS: i64 = 30_000;

const MISSING_SCOPE: &str = "Spotify access was granted without the permission to read what you \
                             are playing: log in again (GET /auth/login) to grant it";

#[derive(Debug, Clone, Serialize)]
pub struct NowPlaying {
    pub is_playing: bool,
    pub track: NowTrack,
    pub progress_ms: Option<i64>,
    /// When Spotify was asked (RFC 3339, UTC).
    pub fetched_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NowTrack {
    pub spotify_id: String,
    pub name: String,
    pub artist_name: String,
    pub album_name: String,
    pub duration_ms: Option<i64>,
}

/// One user's state: the last answer and the missing-scope memo.
#[derive(Default)]
struct Slot {
    /// Unix ms of the last Spotify answer, and it (`None` = nothing playing).
    cached: Option<(i64, Option<NowPlaying>)>,
    /// Access token for which Spotify answered 403.
    missing_scope_token: Option<String>,
}

type SlotKey = (String, i64);

static SLOTS: LazyLock<Mutex<HashMap<SlotKey, Arc<AsyncMutex<Slot>>>>> =
    LazyLock::new(Default::default);

/// The user's slot. Keyed by the Spotify API base as well as the user id, so
/// several apps in one process (tests) never share an answer.
fn slot_for(state: &AppState, user_id: i64) -> Arc<AsyncMutex<Slot>> {
    let key = (state.config.spotify_api_base.clone(), user_id);
    let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    slots.entry(key).or_default().clone()
}

enum Failure {
    /// 503 with `Retry-After` (seconds).
    Unavailable(u64),
    App(AppError),
}

impl From<AppError> for Failure {
    fn from(e: AppError) -> Self {
        Self::App(e)
    }
}

impl From<sqlx::Error> for Failure {
    fn from(e: sqlx::Error) -> Self {
        Self::App(e.into())
    }
}

pub async fn handler(State(state): State<AppState>, user: AuthUser) -> Response {
    let slot = slot_for(&state, user.user_id);
    let mut slot = slot.lock().await;
    match resolve(&state, user.user_id, &mut slot).await {
        Ok(Some(now_playing)) => Json(now_playing).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(Failure::Unavailable(secs)) => {
            let body = Json(json!({
                "error": format!("Spotify is rate limiting this account, retry in {secs} s")
            }));
            let mut resp = (StatusCode::SERVICE_UNAVAILABLE, body).into_response();
            resp.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(secs));
            resp
        }
        Err(Failure::App(e)) => e.into_response(),
    }
}

async fn resolve(
    state: &AppState,
    user_id: i64,
    slot: &mut Slot,
) -> Result<Option<NowPlaying>, Failure> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    if let Some((at_ms, answer)) = &slot.cached
        && now_ms.saturating_sub(*at_ms) < CACHE_TTL_MS
    {
        return Ok(answer.clone());
    }

    let user = UserState::load(state, user_id).await?;
    if let Some(until_ms) = user.rate_limited_until_ms
        && until_ms > now_ms
    {
        tracing::debug!(user_id, "now-playing: rate limited, no call");
        return stale_or_unavailable(slot, until_ms, now_ms);
    }
    if slot.missing_scope_token.as_deref() == Some(user.access_token.as_str()) {
        return Err(AppError::Forbidden(MISSING_SCOPE.into()).into());
    }

    let access = match valid_access_token(state, user_id, &user, now_ms).await {
        Ok(token) => token,
        Err(e) => return failed(state, user_id, &user, e, slot).await,
    };
    match client::get_currently_playing(state, &access).await {
        Ok(playing) => {
            let fetched = chrono::Utc::now();
            let answer = playing.and_then(|p| to_now_playing(p, fetched));
            slot.cached = Some((fetched.timestamp_millis(), answer.clone()));
            slot.missing_scope_token = None;
            Ok(answer)
        }
        Err(ClientError::Status { status: 403, .. }) => {
            tracing::info!(user_id, "now-playing: token lacks the scope, login again");
            slot.missing_scope_token = Some(access);
            Err(AppError::Forbidden(MISSING_SCOPE.into()).into())
        }
        Err(e) => failed(state, user_id, &user, PollError::Spotify(e), slot).await,
    }
}

/// The last answer, however old, or 503 when there is none.
fn stale_or_unavailable(
    slot: &Slot,
    until_ms: i64,
    now_ms: i64,
) -> Result<Option<NowPlaying>, Failure> {
    match &slot.cached {
        Some((_, answer)) => Ok(answer.clone()),
        None => {
            let secs = until_ms.saturating_sub(now_ms).saturating_add(999) / 1000;
            Err(Failure::Unavailable(
                u64::try_from(secs).unwrap_or(1).max(1),
            ))
        }
    }
}

/// A failed token refresh or Spotify call. What the poller would store for it
/// (429 deadline, refused refresh token, expired access token) is stored too;
/// other failures leave the poller's schedule alone.
async fn failed(
    state: &AppState,
    user_id: i64,
    user: &UserState,
    err: PollError,
    slot: &Slot,
) -> Result<Option<NowPlaying>, Failure> {
    let at_ms = chrono::Utc::now().timestamp_millis();
    tracing::warn!(user_id, kind = err.kind(), status = ?err.status(), error = %err, "now-playing: failed");
    match &err {
        PollError::Spotify(ClientError::RateLimited { .. }) => {
            let until_ms = record_failure(state, user_id, user, &err, at_ms).await;
            stale_or_unavailable(slot, until_ms, at_ms)
        }
        PollError::Spotify(ClientError::InvalidGrant) => {
            record_failure(state, user_id, user, &err, at_ms).await;
            Err(AppError::Forbidden(
                "Spotify no longer accepts your login: log in again (GET /auth/login)".into(),
            )
            .into())
        }
        PollError::Spotify(ClientError::Unauthorized) => {
            record_failure(state, user_id, user, &err, at_ms).await;
            Err(
                AppError::BadGateway("Spotify rejected the access token, retry later".into())
                    .into(),
            )
        }
        PollError::Database(e) => Err(AppError::Internal(e.to_string()).into()),
        other => Err(AppError::BadGateway(other.to_string()).into()),
    }
}

/// `None` when nothing that can be shown is playing (an episode, an ad, a local
/// file or an unknown item).
fn to_now_playing(
    playing: CurrentlyPlaying,
    fetched_at: chrono::DateTime<chrono::Utc>,
) -> Option<NowPlaying> {
    if !matches!(
        playing.currently_playing_type.as_deref(),
        None | Some("track")
    ) {
        return None;
    }
    let item = playing.item?;
    Some(NowPlaying {
        is_playing: playing.is_playing,
        track: NowTrack {
            spotify_id: item.id?,
            name: item.name,
            artist_name: item
                .artists
                .first()
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            album_name: item.album.map(|a| a.name).unwrap_or_default(),
            duration_ms: item.duration_ms,
        },
        progress_ms: playing.progress_ms,
        fetched_at: fetched_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    })
}
