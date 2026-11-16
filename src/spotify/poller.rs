//! Background poller recording each user's recently played tracks.
//!
//! Spotify's development-mode quota is shared by the whole app and
//! `recently-played` only exposes the last 50 plays, so every call counts and a
//! rate limit is never ignored. All the scheduling state lives in the `users`
//! table (see `migrations/0002_poller_state_and_token_usage.sql`), so it
//! survives a restart:
//! - the scheduler wakes up every [`TICK`] and polls, one user after the other,
//!   only the users that are due (`next_poll_at_ms` NULL or past) and not rate
//!   limited (`rate_limited_until_ms` NULL or past);
//! - a poll makes at most one `recently-played` call, with `after` = the cursor
//!   (`last_polled_at_ms`), preceded by a token refresh only when the access
//!   token expires within [`REFRESH_MARGIN_SECS`];
//! - the interval adapts (`poll_interval_secs`): back to `POLL_INTERVAL_SECS`
//!   after new plays or a full page, doubled up to `POLL_MAX_INTERVAL_SECS` when
//!   idle, and every poll is scheduled with ±10 % jitter (`next_poll_at_ms`);
//! - a 429 stores `rate_limited_until_ms` = now + `Retry-After` (or an
//!   exponential back-off without one) for that user only: nothing at all, not
//!   even a token refresh, is sent for that user before then.

use std::time::{Duration, Instant};

use rand::Rng;

use super::client::{self, ClientError, RECENTLY_PLAYED_LIMIT};
use crate::AppState;
use crate::config::Config;
use crate::db::{PlaySource, insert_play, upsert_track};

/// How often the scheduler looks for due users (never more often than
/// `POLL_INTERVAL_SECS`).
const TICK: Duration = Duration::from_secs(15);
/// Refresh the access token when it expires within this many seconds.
const REFRESH_MARGIN_SECS: i64 = 60;
/// Back-off after a 429 without a usable `Retry-After`: this much first, then
/// doubled while the 429s keep coming...
const RATE_LIMIT_BACKOFF_MIN_MS: i64 = 60_000;
/// ...up to this much.
const RATE_LIMIT_BACKOFF_MAX_MS: i64 = 3_600_000;
/// Back-off after Spotify refused the refresh token.
const REVOKED_BACKOFF_MS: i64 = 6 * 3_600_000;
/// Every scheduled poll is moved by up to this fraction of the interval.
const JITTER: f64 = 0.1;
/// `last_error` is cut to this many characters.
const MAX_ERROR_CHARS: usize = 300;
/// `last_error` once Spotify refused the refresh token. While it is set, a new
/// login (fresh tokens) lifts the back-off, see [`due_users`].
const RELOGIN_ERROR: &str = "Spotify refused the refresh token (invalid_grant): it was revoked \
                             or has expired, the user must log in again (GET /auth/login)";

/// Why polling one user failed.
#[derive(Debug)]
pub enum PollError {
    /// A 429 stored earlier still holds until `until_ms`: nothing was sent to
    /// Spotify and nothing was recorded.
    StillRateLimited { until_ms: i64 },
    /// A Spotify call failed (token refresh or recently-played).
    Spotify(ClientError),
    /// Local database failure.
    Database(sqlx::Error),
}

impl PollError {
    /// Short machine-readable category, used in logs.
    ///
    /// One of `still_rate_limited`, `rate_limited`, `unauthorized`,
    /// `invalid_grant`, `http_status`, `network`, `invalid_response`, `database`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::StillRateLimited { .. } => "still_rate_limited",
            Self::Spotify(e) => e.kind(),
            Self::Database(_) => "database",
        }
    }

    /// HTTP status Spotify answered with, when there was an error status.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Spotify(e) => e.status(),
            Self::StillRateLimited { .. } | Self::Database(_) => None,
        }
    }

    /// The `Retry-After` delay of a 429, when Spotify sent a usable one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Spotify(ClientError::RateLimited { retry_after }) => *retry_after,
            _ => None,
        }
    }
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StillRateLimited { until_ms } => {
                write!(f, "rate limited by Spotify until {}", fmt_ms(*until_ms))
            }
            Self::Spotify(e) => write!(f, "{e}"),
            Self::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for PollError {}

impl From<ClientError> for PollError {
    fn from(e: ClientError) -> Self {
        Self::Spotify(e)
    }
}

impl From<sqlx::Error> for PollError {
    fn from(e: sqlx::Error) -> Self {
        Self::Database(e)
    }
}

/// The user row as the poller needs it.
#[derive(sqlx::FromRow)]
pub(super) struct UserState {
    pub(super) access_token: String,
    refresh_token: String,
    /// Unix seconds.
    token_expires_at: i64,
    /// Cursor: newest `played_at` seen.
    last_polled_at_ms: Option<i64>,
    poll_interval_secs: Option<i64>,
    pub(super) rate_limited_until_ms: Option<i64>,
    last_error_at_ms: Option<i64>,
}

impl UserState {
    pub(super) async fn load(state: &AppState, user_id: i64) -> Result<Self, sqlx::Error> {
        sqlx::query_as(
            "SELECT access_token, refresh_token, token_expires_at, last_polled_at_ms,
                    poll_interval_secs, rate_limited_until_ms, last_error_at_ms
             FROM users WHERE id = ?1",
        )
        .bind(user_id)
        .fetch_one(&state.pool)
        .await
    }
}

/// What a successful `recently-played` call brought.
struct Fetched {
    /// Plays newly recorded.
    inserted: usize,
    /// Spotify returned a full page: plays may have fallen out of its window.
    full_page: bool,
    /// New cursor.
    newest: Option<i64>,
}

/// When the next poll is scheduled.
struct Schedule {
    interval_secs: i64,
    next_poll_at_ms: i64,
}

/// Poll Spotify once for `user_id` at `now_ms` (unix ms).
///
/// Refreshes the token if it expires within [`REFRESH_MARGIN_SECS`], fetches
/// the plays after the cursor with one `recently-played` call, records them,
/// advances the cursor and schedules the next poll. Every outcome is logged and
/// stored on the user row; a failure leaves the cursor alone.
///
/// Returns the number of newly recorded plays. Refuses, without any call, while
/// the user is rate limited.
pub async fn poll_user(state: &AppState, user_id: i64, now_ms: i64) -> Result<usize, PollError> {
    let started = Instant::now();
    let user = UserState::load(state, user_id).await?;
    if let Some(until_ms) = user.rate_limited_until_ms
        && until_ms > now_ms
    {
        tracing::debug!(user_id, until = %fmt_ms(until_ms), "poller: still rate limited, no call");
        return Err(PollError::StillRateLimited { until_ms });
    }

    let fetched = fetch(state, user_id, &user, now_ms).await;
    // Outcome times include the time spent waiting for Spotify, so a deadline
    // computed from `Retry-After` is never earlier than Spotify's own.
    let at_ms = now_ms.saturating_add(elapsed_ms(started));
    let outcome = match fetched {
        Ok(f) => {
            let recorded = record_success(state, user_id, &user, &f, at_ms).await;
            recorded.map(|s| (f, s))
        }
        Err(e) => Err(e),
    };
    match outcome {
        Ok((f, s)) => {
            if f.inserted > 0 {
                tracing::info!(
                    user_id,
                    new_plays = f.inserted,
                    interval_s = s.interval_secs,
                    next_poll_at = %fmt_ms(s.next_poll_at_ms),
                    "poller: recorded new plays"
                );
            } else {
                tracing::debug!(
                    user_id,
                    interval_s = s.interval_secs,
                    next_poll_at = %fmt_ms(s.next_poll_at_ms),
                    "poller: no new play"
                );
            }
            if f.full_page && user.last_polled_at_ms.is_some() {
                tracing::warn!(
                    user_id,
                    plays = RECENTLY_PLAYED_LIMIT,
                    "poller: full page from Spotify, plays older than its last-50 window may be \
                     lost; polling again at the minimum interval"
                );
            }
            Ok(f.inserted)
        }
        Err(e) => {
            let retry_at = record_failure(state, user_id, &user, &e, at_ms).await;
            tracing::warn!(
                user_id,
                kind = e.kind(),
                status = ?e.status(),
                retry_at = %fmt_ms(retry_at),
                error = %e,
                "poller: poll failed"
            );
            Err(e)
        }
    }
}

/// The Spotify part of a poll.
///
/// A token refresh when needed, then exactly one `recently-played` call, whose
/// plays are recorded.
async fn fetch(
    state: &AppState,
    user_id: i64,
    user: &UserState,
    now_ms: i64,
) -> Result<Fetched, PollError> {
    let access = valid_access_token(state, user_id, user, now_ms).await?;

    // `after` is exclusive and Spotify only exposes the last 50 plays, so one
    // page holds every readable play after the cursor: taking the newest
    // `played_at` as the next cursor never skips a play Spotify still has.
    let page = client::get_recently_played(state, &access, user.last_polled_at_ms).await?;
    let full_page = page.items.len() >= RECENTLY_PLAYED_LIMIT;

    let mut inserted = 0;
    let mut newest = user.last_polled_at_ms;
    for item in page.items {
        let played_at_ms = match chrono::DateTime::parse_from_rfc3339(&item.played_at) {
            Ok(t) => t.timestamp_millis(),
            Err(e) => {
                tracing::warn!(user_id, played_at = %item.played_at, error = %e, "poller: skipping play with bad timestamp");
                continue;
            }
        };
        // The cursor also moves past plays that are not recorded (local files,
        // episodes) so they are not fetched again.
        newest = Some(newest.map_or(played_at_ms, |n| n.max(played_at_ms)));
        let Some(track) = item.track else { continue };
        let Some(spotify_track_id) = track.id.as_deref() else {
            continue; // local file: no Spotify id
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
    }
    Ok(Fetched {
        inserted,
        full_page,
        newest,
    })
}

/// The user's access token, refreshed and stored first only when it expires
/// within [`REFRESH_MARGIN_SECS`]. Shared with the now-playing endpoint.
pub(super) async fn valid_access_token(
    state: &AppState,
    user_id: i64,
    user: &UserState,
    now_ms: i64,
) -> Result<String, PollError> {
    let now_secs = now_ms.div_euclid(1000);
    if user.token_expires_at <= now_secs + REFRESH_MARGIN_SECS {
        refresh_access_token(state, user_id, &user.refresh_token, now_secs).await
    } else {
        Ok(user.access_token.clone())
    }
}

/// Refresh and store the access token; returns the new one.
async fn refresh_access_token(
    state: &AppState,
    user_id: i64,
    refresh_token: &str,
    now_secs: i64,
) -> Result<String, PollError> {
    let t = client::refresh_token(state, refresh_token).await?;
    let new_refresh = t.refresh_token.as_deref().unwrap_or(refresh_token);
    sqlx::query(
        "UPDATE users SET access_token = ?1, refresh_token = ?2, token_expires_at = ?3
         WHERE id = ?4",
    )
    .bind(&t.access_token)
    .bind(new_refresh)
    .bind(now_secs.saturating_add(t.expires_in))
    .bind(user_id)
    .execute(&state.pool)
    .await?;
    tracing::debug!(user_id, "poller: access token refreshed");
    Ok(t.access_token)
}

/// Store a successful poll: new cursor, adapted interval, next poll, no error.
async fn record_success(
    state: &AppState,
    user_id: i64,
    user: &UserState,
    fetched: &Fetched,
    at_ms: i64,
) -> Result<Schedule, PollError> {
    let (min, max) = interval_bounds(&state.config);
    let active = fetched.inserted > 0 || fetched.full_page;
    let interval_secs = next_interval_secs(current_interval(user, min, max), active, min, max);
    let next_poll_at_ms = at_ms.saturating_add(jittered_ms(interval_secs));
    sqlx::query(
        "UPDATE users SET last_polled_at_ms = ?1, poll_interval_secs = ?2, next_poll_at_ms = ?3,
                last_poll_ok_at_ms = ?4, last_error = NULL, last_error_at_ms = NULL,
                rate_limited_until_ms = NULL
         WHERE id = ?5",
    )
    .bind(fetched.newest)
    .bind(interval_secs)
    .bind(next_poll_at_ms)
    .bind(at_ms)
    .bind(user_id)
    .execute(&state.pool)
    .await?;
    Ok(Schedule {
        interval_secs,
        next_poll_at_ms,
    })
}

/// Store a failed poll; returns the earliest time of the next call for this user.
///
/// The cursor is left alone. Then:
/// - 429: `rate_limited_until_ms` = now + `Retry-After`, or the exponential
///   back-off without a usable one;
/// - refresh token refused: [`RELOGIN_ERROR`], next poll in 6 h;
/// - 401: the access token is marked expired, so the next poll refreshes it;
/// - anything else: the interval doubles, as when idle.
pub(super) async fn record_failure(
    state: &AppState,
    user_id: i64,
    user: &UserState,
    err: &PollError,
    at_ms: i64,
) -> i64 {
    let (min, max) = interval_bounds(&state.config);
    let current = current_interval(user, min, max);
    let mut interval = current;
    let mut rate_limited_until_ms = None;
    let mut expire_token = false;
    let (next_poll_at_ms, last_error) = match err {
        PollError::StillRateLimited { until_ms } => return *until_ms,
        PollError::Spotify(ClientError::RateLimited { retry_after }) => {
            let wait_ms = match retry_after {
                Some(d) => duration_ms(*d),
                None => rate_limit_backoff_ms(user.rate_limited_until_ms, user.last_error_at_ms),
            };
            let until = at_ms.saturating_add(wait_ms);
            rate_limited_until_ms = Some(until);
            (until, format!("{err}; no call before {}", fmt_ms(until)))
        }
        PollError::Spotify(ClientError::InvalidGrant) => (
            at_ms.saturating_add(REVOKED_BACKOFF_MS),
            RELOGIN_ERROR.to_string(),
        ),
        PollError::Spotify(ClientError::Unauthorized) => {
            expire_token = true;
            (
                at_ms.saturating_add(jittered_ms(current)),
                format!("{err}; it will be refreshed on the next poll"),
            )
        }
        _ => {
            interval = next_interval_secs(current, false, min, max);
            (at_ms.saturating_add(jittered_ms(interval)), err.to_string())
        }
    };
    let stored = sqlx::query(
        "UPDATE users SET
             rate_limited_until_ms = COALESCE(?1, rate_limited_until_ms),
             next_poll_at_ms = ?2,
             poll_interval_secs = ?3,
             token_expires_at = CASE WHEN ?4 THEN 0 ELSE token_expires_at END,
             last_error = ?5,
             last_error_at_ms = ?6
         WHERE id = ?7",
    )
    .bind(rate_limited_until_ms)
    .bind(next_poll_at_ms)
    .bind(interval)
    .bind(expire_token)
    .bind(client::truncate_chars(&last_error, MAX_ERROR_CHARS))
    .bind(at_ms)
    .bind(user_id)
    .execute(&state.pool)
    .await;
    if let Err(e) = stored {
        tracing::error!(user_id, error = %e, "poller: cannot record the failed poll");
    }
    next_poll_at_ms
}

/// Users to poll at `now_ms`: not rate limited, and due.
///
/// Due means `next_poll_at_ms` NULL or past. A user backed off because the
/// refresh token was refused is due again as soon as they log in again: their
/// token then expires later than the error time plus the refresh margin, which
/// a refused refresh never leaves.
async fn due_users(state: &AppState, now_ms: i64) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM users
         WHERE (rate_limited_until_ms IS NULL OR rate_limited_until_ms <= ?1)
           AND (next_poll_at_ms IS NULL OR next_poll_at_ms <= ?1
                OR (last_error = ?2 AND token_expires_at * 1000 > last_error_at_ms + ?3))
         ORDER BY id",
    )
    .bind(now_ms)
    .bind(RELOGIN_ERROR)
    .bind(REFRESH_MARGIN_SECS * 1000)
    .fetch_all(&state.pool)
    .await
}

/// One scheduler cycle at `now_ms` (unix ms).
///
/// Polls, one after the other, the users that are due. A failure for one user
/// is logged and recorded by [`poll_user`] and never stops the others. Returns
/// the ids of the users polled.
pub async fn poll_all_users(state: &AppState, now_ms: i64) -> Vec<i64> {
    let started = Instant::now();
    let due = match due_users(state, now_ms).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!(error = %e, "poller: cannot list the users to poll");
            return Vec::new();
        }
    };
    for &user_id in &due {
        // Earlier polls of this cycle took time: keep each user's clock right.
        let user_now_ms = now_ms.saturating_add(elapsed_ms(started));
        let _ = poll_user(state, user_id, user_now_ms).await;
    }
    due
}

/// Start the background poller that records plays for every user.
/// Called once from `main`.
pub fn spawn_poller(state: AppState) {
    let tick = TICK.min(Duration::from_secs(state.config.poll_interval_secs.max(1)));
    let (min, max) = interval_bounds(&state.config);
    tracing::info!(
        tick_s = tick.as_secs(),
        min_interval_s = min,
        max_interval_s = max,
        "poller started"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            poll_all_users(&state, chrono::Utc::now().timestamp_millis()).await;
        }
    });
}

/// (minimum, maximum) poll interval in seconds.
fn interval_bounds(config: &Config) -> (i64, i64) {
    let min = i64::try_from(config.poll_interval_secs.max(1)).unwrap_or(i64::MAX);
    let max = i64::try_from(config.poll_max_interval_secs)
        .unwrap_or(i64::MAX)
        .max(min);
    (min, max)
}

/// The user's current interval (NULL = the minimum).
fn current_interval(user: &UserState, min: i64, max: i64) -> i64 {
    user.poll_interval_secs.unwrap_or(min).clamp(min, max)
}

/// The next interval after a poll.
///
/// The minimum after activity (new plays, or a full page that may hide a gap),
/// otherwise twice the current one, up to the maximum.
fn next_interval_secs(current: i64, active: bool, min: i64, max: i64) -> i64 {
    if active {
        min
    } else {
        current.saturating_mul(2).clamp(min, max)
    }
}

/// Back-off after a 429 without a usable `Retry-After`.
///
/// 60 s, doubled while the 429s follow one another, capped at 1 h. The previous
/// back-off is `rate_limited_until_ms - last_error_at_ms`, both written by the
/// previous 429 (a success clears the deadline, another error moves the error
/// time past it).
fn rate_limit_backoff_ms(previous_until_ms: Option<i64>, previous_error_at_ms: Option<i64>) -> i64 {
    match (previous_until_ms, previous_error_at_ms) {
        (Some(until), Some(at)) if until > at => until
            .saturating_sub(at)
            .saturating_mul(2)
            .clamp(RATE_LIMIT_BACKOFF_MIN_MS, RATE_LIMIT_BACKOFF_MAX_MS),
        _ => RATE_LIMIT_BACKOFF_MIN_MS,
    }
}

/// `interval_secs` in milliseconds, ±[`JITTER`].
fn jittered_ms(interval_secs: i64) -> i64 {
    let factor: f64 = rand::rng().random_range((1.0 - JITTER)..=(1.0 + JITTER));
    (interval_secs as f64 * 1000.0 * factor).round() as i64
}

fn duration_ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn elapsed_ms(since: Instant) -> i64 {
    duration_ms(since.elapsed())
}

/// Unix ms as RFC 3339, for logs and messages.
fn fmt_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || format!("{ms} ms"),
        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_resets_on_activity_and_doubles_up_to_the_maximum() {
        assert_eq!(next_interval_secs(480, true, 120, 600), 120);
        assert_eq!(next_interval_secs(120, false, 120, 600), 240);
        assert_eq!(next_interval_secs(480, false, 120, 600), 600);
        assert_eq!(next_interval_secs(600, false, 120, 600), 600);
    }

    #[test]
    fn rate_limit_backoff_starts_at_a_minute_doubles_and_caps_at_an_hour() {
        assert_eq!(rate_limit_backoff_ms(None, None), 60_000);
        assert_eq!(
            rate_limit_backoff_ms(Some(1_060_000), Some(1_000_000)),
            120_000
        );
        assert_eq!(
            rate_limit_backoff_ms(Some(3_400_000), Some(1_000_000)),
            3_600_000
        );
        // The deadline predates the last error: not a run of 429s, start over.
        assert_eq!(
            rate_limit_backoff_ms(Some(1_000_000), Some(2_000_000)),
            60_000
        );
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        for _ in 0..1_000 {
            let ms = jittered_ms(120);
            assert!((108_000..=132_000).contains(&ms), "{ms}");
        }
    }

    #[test]
    fn times_are_formatted_as_rfc3339() {
        assert_eq!(fmt_ms(1_800_000_000_000), "2027-01-15T08:00:00Z");
    }
}
