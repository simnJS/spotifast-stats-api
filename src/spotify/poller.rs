//! Background poller recording each user's recently played tracks.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::client::{self, ClientError};
use crate::AppState;
use crate::db::{PlaySource, insert_play, upsert_track};

/// Refresh the access token when it expires within this many seconds.
const REFRESH_MARGIN_SECS: i64 = 60;
/// Back-off when Spotify answers 429 without a usable `Retry-After`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Why polling one user failed.
#[derive(Debug)]
pub enum PollError {
    /// Spotify asked us to slow down for this long.
    RateLimited(Duration),
    Other(String),
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited(d) => write!(f, "rate limited, retry after {d:?}"),
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl From<ClientError> for PollError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::RateLimited(d) => Self::RateLimited(d.unwrap_or(DEFAULT_RETRY_AFTER)),
            ClientError::Other(m) => Self::Other(m),
        }
    }
}

impl From<sqlx::Error> for PollError {
    fn from(e: sqlx::Error) -> Self {
        Self::Other(format!("database error: {e}"))
    }
}

/// Poll Spotify once for `user_id`: refresh the token if needed, fetch the plays
/// since `last_polled_at_ms`, record them and advance the cursor.
/// Returns the number of newly inserted plays.
pub async fn poll_user(state: &AppState, user_id: i64) -> Result<usize, PollError> {
    let (mut access, refresh, expires_at, last_polled): (String, String, i64, Option<i64>) =
        sqlx::query_as(
            "SELECT access_token, refresh_token, token_expires_at, last_polled_at_ms
             FROM users WHERE id = ?1",
        )
        .bind(user_id)
        .fetch_one(&state.pool)
        .await?;

    let now = chrono::Utc::now().timestamp();
    if expires_at <= now + REFRESH_MARGIN_SECS {
        let t = client::refresh_token(state, &refresh).await?;
        let new_refresh = t.refresh_token.unwrap_or(refresh);
        sqlx::query(
            "UPDATE users SET access_token = ?1, refresh_token = ?2, token_expires_at = ?3
             WHERE id = ?4",
        )
        .bind(&t.access_token)
        .bind(&new_refresh)
        .bind(now + t.expires_in)
        .bind(user_id)
        .execute(&state.pool)
        .await?;
        access = t.access_token;
    }

    let recent = client::get_recently_played(state, &access, last_polled).await?;

    let mut inserted = 0;
    let mut newest = last_polled;
    for item in recent.items {
        let Some(track) = item.track else { continue };
        let Some(spotify_track_id) = track.id.as_deref() else {
            continue; // local file: no Spotify id
        };
        let played_at_ms = match chrono::DateTime::parse_from_rfc3339(&item.played_at) {
            Ok(t) => t.timestamp_millis(),
            Err(e) => {
                tracing::warn!(user_id, played_at = %item.played_at, error = %e, "skipping play with bad timestamp");
                continue;
            }
        };
        let artist = track.artists.first().map(|a| a.name.as_str()).unwrap_or("");
        let album = track.album.as_ref().map(|a| a.name.as_str()).unwrap_or("");
        let track_id = upsert_track(
            &state.pool,
            spotify_track_id,
            &track.name,
            artist,
            album,
            track.duration_ms,
        )
        .await?;
        let ms_played = track.duration_ms.unwrap_or(0);
        if insert_play(
            &state.pool,
            user_id,
            track_id,
            played_at_ms,
            ms_played,
            PlaySource::Poll,
        )
        .await?
        {
            inserted += 1;
        }
        newest = Some(newest.map_or(played_at_ms, |n| n.max(played_at_ms)));
    }

    if newest != last_polled {
        sqlx::query("UPDATE users SET last_polled_at_ms = ?1 WHERE id = ?2")
            .bind(newest)
            .bind(user_id)
            .execute(&state.pool)
            .await?;
    }
    Ok(inserted)
}

/// Poll every user once. `blocked` holds per-user rate-limit deadlines; a failure
/// for one user is logged and never stops the others.
pub async fn poll_all_users(state: &AppState, blocked: &mut HashMap<i64, Instant>) {
    let ids: Vec<(i64,)> = match sqlx::query_as("SELECT id FROM users ORDER BY id")
        .fetch_all(&state.pool)
        .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!(error = %e, "poller: cannot list users");
            return;
        }
    };
    for (user_id,) in ids {
        if let Some(until) = blocked.get(&user_id) {
            if Instant::now() < *until {
                continue;
            }
            blocked.remove(&user_id);
        }
        match poll_user(state, user_id).await {
            Ok(n) => tracing::debug!(user_id, inserted = n, "poll done"),
            Err(PollError::RateLimited(d)) => {
                tracing::warn!(user_id, retry_after = ?d, "poller: rate limited by Spotify");
                blocked.insert(user_id, Instant::now() + d);
            }
            Err(e) => tracing::warn!(user_id, error = %e, "poller: poll failed"),
        }
    }
}

/// Start the background poller that records plays for every user.
/// Called once from `main`.
pub fn spawn_poller(state: AppState) {
    let interval = Duration::from_secs(state.config.poll_interval_secs.max(1));
    tokio::spawn(async move {
        let mut blocked = HashMap::new();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            poll_all_users(&state, &mut blocked).await;
        }
    });
}
