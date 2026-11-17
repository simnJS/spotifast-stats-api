//! Background poller recording each user's recently played tracks.
//!
//! Spotify's development-mode quota is shared by the whole app and
//! `recently-played` only exposes the last 50 plays, so every call counts and a
//! rate limit is never ignored. All the scheduling state lives in the `users`
//! table (see `migrations/0002_poller_state_and_token_usage.sql` and
//! `migrations/0003_adaptive_poller.sql`), so it survives a restart:
//! - the scheduler wakes up every [`TICK`] and polls, one user after the other,
//!   only the users that are due (`next_poll_at_ms` NULL or past), and nobody
//!   while Spotify rate limits the app (see [`super::rate_limit`]);
//! - a poll makes at most one `recently-played` call, with `after` = the cursor
//!   (`last_polled_at_ms`), preceded by a token refresh only when the access
//!   token expires within [`REFRESH_MARGIN_SECS`];
//! - the interval (`poll_interval_secs`) follows each user's pace: a page with
//!   at least two plays gives the seconds per play (time between its oldest and
//!   newest play / number of plays), smoothed into `secs_per_play` by an
//!   exponential moving average; the next interval is
//!   [`TARGET_PLAYS_PER_POLL`] times that, so the 50-play page is about half
//!   full at the next poll, within `POLL_INTERVAL_SECS` and
//!   `POLL_MAX_INTERVAL_SECS`. A full page goes back to the minimum at once
//!   (with a warning: plays may have fallen out of the page), a poll without new
//!   plays goes to the maximum, and every poll is scheduled with ±10 % jitter
//!   (`next_poll_at_ms`);
//! - a 429 blocks the whole app until now + `Retry-After` (or an exponential
//!   back-off without one), stored in the database: nothing at all, not even a
//!   token refresh, is sent to Spotify for anyone before then, and the cycle
//!   stops at once.

use std::time::{Duration, Instant};

use rand::Rng;

use super::client::{self, ClientError, RECENTLY_PLAYED_LIMIT};
use super::rate_limit;
use crate::AppState;
use crate::clock::fmt_ms;
use crate::config::Config;
use crate::db::{PlaySource, insert_play, upsert_track};

/// How often the scheduler looks for due users (never more often than
/// `POLL_INTERVAL_SECS`).
const TICK: Duration = Duration::from_secs(15);
/// Refresh the access token when it expires within this many seconds.
const REFRESH_MARGIN_SECS: i64 = 60;
/// Back-off after Spotify refused the refresh token.
const REVOKED_BACKOFF_MS: i64 = 6 * 3_600_000;
/// Every scheduled poll is moved by up to this fraction of the interval.
const JITTER: f64 = 0.1;
/// The interval is the time this many plays take at the user's pace: half of
/// Spotify's 50-play page, so the next poll finds it about half full.
const TARGET_PLAYS_PER_POLL: f64 = 25.0;
/// Weight of the newest page in the smoothed pace (exponential moving average).
const PACE_SMOOTHING: f64 = 0.5;
/// `last_error` is cut to this many characters.
const MAX_ERROR_CHARS: usize = 300;
/// `last_error` once Spotify refused the refresh token. While it is set, a new
/// login (fresh tokens) lifts the back-off, see [`due_users`].
const RELOGIN_ERROR: &str = "Spotify refused the refresh token (invalid_grant): it was revoked \
                             or has expired, the user must log in again (GET /auth/login)";

/// Why polling one user failed.
#[derive(Debug)]
pub enum PollError {
    /// Spotify rate limits the app until `until_ms` (a 429 stored earlier, for
    /// any user): nothing was sent to Spotify and nothing was recorded.
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
    /// Smoothed pace, in seconds per play (NULL = not measured yet).
    secs_per_play: Option<f64>,
}

impl UserState {
    pub(super) async fn load(state: &AppState, user_id: i64) -> Result<Self, sqlx::Error> {
        sqlx::query_as(
            "SELECT access_token, refresh_token, token_expires_at, last_polled_at_ms,
                    poll_interval_secs, secs_per_play
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
    /// Plays on the page, all newer than the cursor, recorded or not (already
    /// imported, local file, episode): they all show the user listening.
    page_plays: usize,
    /// Spotify returned a full page: plays may have fallen out of its window.
    full_page: bool,
    /// Seconds per play measured on this page, when it can be (see [`page_pace`]).
    page_secs_per_play: Option<f64>,
    /// New cursor.
    newest: Option<i64>,
}

/// When the next poll is scheduled.
struct Schedule {
    interval_secs: i64,
    next_poll_at_ms: i64,
    /// The smoothed pace after this poll.
    secs_per_play: Option<f64>,
}

/// Poll Spotify once for `user_id` at `now_ms` (unix ms).
///
/// Refreshes the token if it expires within [`REFRESH_MARGIN_SECS`], fetches
/// the plays after the cursor with one `recently-played` call, records them,
/// advances the cursor and schedules the next poll. Every outcome is logged and
/// stored on the user row; a failure leaves the cursor alone.
///
/// Returns the number of newly recorded plays. Refuses, without any call, while
/// Spotify rate limits the app.
pub async fn poll_user(state: &AppState, user_id: i64, now_ms: i64) -> Result<usize, PollError> {
    let started = Instant::now();
    if let Some(until_ms) = rate_limit::blocked_until(&state.pool, now_ms).await? {
        tracing::debug!(user_id, until = %fmt_ms(until_ms), "poller: Spotify limits the app, no call");
        return Err(PollError::StillRateLimited { until_ms });
    }
    let user = UserState::load(state, user_id).await?;

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
            let pace = s.secs_per_play.map(round_tenth);
            if f.inserted > 0 {
                tracing::info!(
                    user_id,
                    new_plays = f.inserted,
                    secs_per_play = ?pace,
                    interval_s = s.interval_secs,
                    next_poll_at = %fmt_ms(s.next_poll_at_ms),
                    "poller: recorded new plays"
                );
            } else {
                tracing::debug!(
                    user_id,
                    page_plays = f.page_plays,
                    secs_per_play = ?pace,
                    interval_s = s.interval_secs,
                    next_poll_at = %fmt_ms(s.next_poll_at_ms),
                    "poller: no new play"
                );
            }
            if f.full_page && user.last_polled_at_ms.is_some() {
                tracing::warn!(
                    user_id,
                    plays = RECENTLY_PLAYED_LIMIT,
                    interval_s = s.interval_secs,
                    "poller: full page from Spotify, plays older than its last-50 window may be \
                     lost; back to the minimum interval"
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
    let mut page_plays = 0;
    let mut page_span: Option<(i64, i64)> = None;
    for item in page.items {
        let played_at_ms = match chrono::DateTime::parse_from_rfc3339(&item.played_at) {
            Ok(t) => t.timestamp_millis(),
            Err(e) => {
                tracing::warn!(user_id, played_at = %item.played_at, error = %e, "poller: skipping play with bad timestamp");
                continue;
            }
        };
        page_plays += 1;
        page_span = Some(page_span.map_or((played_at_ms, played_at_ms), |(lo, hi)| {
            (lo.min(played_at_ms), hi.max(played_at_ms))
        }));
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
    // Without a cursor (first poll) the page is the backlog of the last 50
    // plays, whatever their age: it says nothing about the current pace.
    let page_secs_per_play = match (user.last_polled_at_ms, page_span) {
        (Some(_), Some((oldest, newest_on_page))) => page_pace(oldest, newest_on_page, page_plays),
        _ => None,
    };
    Ok(Fetched {
        inserted,
        page_plays,
        full_page,
        page_secs_per_play,
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

/// Store a successful poll: new cursor, smoothed pace, interval for that pace,
/// next poll, no error.
async fn record_success(
    state: &AppState,
    user_id: i64,
    user: &UserState,
    fetched: &Fetched,
    at_ms: i64,
) -> Result<Schedule, PollError> {
    let (min, max) = interval_bounds(&state.config);
    let secs_per_play = match fetched.page_secs_per_play {
        Some(measured) => Some(smoothed_pace(user.secs_per_play, measured)),
        None => user.secs_per_play,
    };
    let interval_secs = next_interval_secs(
        fetched.page_plays,
        fetched.full_page,
        secs_per_play,
        min,
        max,
    );
    let next_poll_at_ms = at_ms.saturating_add(jittered_ms(interval_secs));
    sqlx::query(
        "UPDATE users SET last_polled_at_ms = ?1, poll_interval_secs = ?2, next_poll_at_ms = ?3,
                last_poll_ok_at_ms = ?4, last_error = NULL, last_error_at_ms = NULL,
                secs_per_play = ?5
         WHERE id = ?6",
    )
    .bind(fetched.newest)
    .bind(interval_secs)
    .bind(next_poll_at_ms)
    .bind(at_ms)
    .bind(secs_per_play)
    .bind(user_id)
    .execute(&state.pool)
    .await?;
    Ok(Schedule {
        interval_secs,
        next_poll_at_ms,
        secs_per_play,
    })
}

/// Store a failed poll; returns the earliest time of the next call for this user.
///
/// The cursor is left alone. Then:
/// - 429: the whole app is blocked ([`rate_limit::record_429`]) and this user's
///   next poll is at the end of the block;
/// - refresh token refused: [`RELOGIN_ERROR`], next poll in 6 h;
/// - 401: the access token is marked expired, so the next poll refreshes it;
/// - anything else: the interval doubles, up to the maximum.
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
    let mut expire_token = false;
    let (next_poll_at_ms, last_error) = match err {
        PollError::StillRateLimited { until_ms } => return *until_ms,
        PollError::Spotify(ClientError::RateLimited { retry_after }) => {
            let until = rate_limit::record_429(&state.pool, *retry_after, at_ms).await;
            (
                until,
                format!(
                    "{err}; no call to Spotify, for any user, before {}",
                    fmt_ms(until)
                ),
            )
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
            interval = backoff_interval_secs(current, min, max);
            (at_ms.saturating_add(jittered_ms(interval)), err.to_string())
        }
    };
    let stored = sqlx::query(
        "UPDATE users SET
             next_poll_at_ms = ?1,
             poll_interval_secs = ?2,
             token_expires_at = CASE WHEN ?3 THEN 0 ELSE token_expires_at END,
             last_error = ?4,
             last_error_at_ms = ?5
         WHERE id = ?6",
    )
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

/// Users to poll at `now_ms`: the due ones.
///
/// Due means `next_poll_at_ms` NULL or past. A user backed off because the
/// refresh token was refused is due again as soon as they log in again: their
/// token then expires later than the error time plus the refresh margin, which
/// a refused refresh never leaves.
async fn due_users(state: &AppState, now_ms: i64) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM users
         WHERE next_poll_at_ms IS NULL OR next_poll_at_ms <= ?1
            OR (last_error = ?2 AND token_expires_at * 1000 > last_error_at_ms + ?3)
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
/// Polls, one after the other, the users that are due; nobody while Spotify
/// rate limits the app. A failure for one user is logged and recorded by
/// [`poll_user`] and does not stop the others, except a 429 (or a block stored
/// meanwhile by `/now-playing` or a login): the cycle stops there, the users
/// left stay due. Returns the ids of the users polled.
pub async fn poll_all_users(state: &AppState, now_ms: i64) -> Vec<i64> {
    let started = Instant::now();
    match rate_limit::blocked_until(&state.pool, now_ms).await {
        Ok(None) => {}
        Ok(Some(until_ms)) => {
            tracing::debug!(until = %fmt_ms(until_ms), "poller: Spotify limits the app, nobody is polled");
            return Vec::new();
        }
        Err(e) => {
            tracing::error!(error = %e, "poller: cannot read the Spotify rate limit");
            return Vec::new();
        }
    }
    let due = match due_users(state, now_ms).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!(error = %e, "poller: cannot list the users to poll");
            return Vec::new();
        }
    };
    let mut polled = Vec::with_capacity(due.len());
    for user_id in due {
        // Earlier polls of this cycle took time: keep each user's clock right.
        let user_now_ms = now_ms.saturating_add(elapsed_ms(started));
        match poll_user(state, user_id, user_now_ms).await {
            Err(PollError::StillRateLimited { .. }) => break,
            Err(PollError::Spotify(ClientError::RateLimited { .. })) => {
                polled.push(user_id);
                break;
            }
            _ => polled.push(user_id),
        }
    }
    polled
}

/// Start the background poller that records plays for every user.
/// Called once from `main`; first says whether a Spotify rate limit stored by a
/// previous run still holds.
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
        match rate_limit::blocked_until(&state.pool, state.clock.now_ms()).await {
            Ok(Some(until_ms)) => tracing::warn!(
                until = %fmt_ms(until_ms),
                "poller: Spotify still rate limits the app (stored 429), no call to Spotify \
                 before the end of the block"
            ),
            Ok(None) => {}
            Err(e) => tracing::error!(error = %e, "poller: cannot read the Spotify rate limit"),
        }
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            poll_all_users(&state, state.clock.now_ms()).await;
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

/// Seconds per play on one page: the time between its oldest and newest play,
/// divided by the number of plays. `None` below two plays (no time span).
fn page_pace(oldest_ms: i64, newest_ms: i64, plays: usize) -> Option<f64> {
    if plays < 2 {
        return None;
    }
    let span_secs = newest_ms.saturating_sub(oldest_ms) as f64 / 1000.0;
    Some(span_secs / plays as f64)
}

/// The smoothed pace once `measured` is taken into account (exponential
/// moving average, the newest page weighing [`PACE_SMOOTHING`]).
fn smoothed_pace(previous: Option<f64>, measured: f64) -> f64 {
    match previous {
        Some(p) if p.is_finite() && p >= 0.0 => {
            PACE_SMOOTHING * measured + (1.0 - PACE_SMOOTHING) * p
        }
        _ => measured,
    }
}

/// The interval for a pace: the time [`TARGET_PLAYS_PER_POLL`] plays take,
/// within `[min, max]`.
fn paced_interval_secs(secs_per_play: f64, min: i64, max: i64) -> i64 {
    let secs = (secs_per_play * TARGET_PLAYS_PER_POLL).round();
    if secs.is_nan() {
        return min;
    }
    // `as` saturates: a huge pace gives i64::MAX, clamped to the maximum.
    (secs as i64).clamp(min, max)
}

/// The next interval after a successful poll.
///
/// - a full page: the minimum, at once (plays may already have been lost);
/// - no play on the page: the maximum (idle);
/// - otherwise the interval for the smoothed pace, or the minimum while the
///   pace is unknown.
fn next_interval_secs(
    page_plays: usize,
    full_page: bool,
    secs_per_play: Option<f64>,
    min: i64,
    max: i64,
) -> i64 {
    if full_page {
        min
    } else if page_plays == 0 {
        max
    } else {
        secs_per_play.map_or(min, |pace| paced_interval_secs(pace, min, max))
    }
}

/// The interval after a failed poll (other than a 429, a refused refresh
/// token or a 401): twice the current one, up to the maximum.
fn backoff_interval_secs(current: i64, min: i64, max: i64) -> i64 {
    current.saturating_mul(2).clamp(min, max)
}

/// `secs_per_play` rounded to a tenth, for logs and `/me/status`.
pub(crate) fn round_tenth(secs: f64) -> f64 {
    (secs * 10.0).round() / 10.0
}

/// `interval_secs` in milliseconds, ±[`JITTER`].
fn jittered_ms(interval_secs: i64) -> i64 {
    let factor: f64 = rand::rng().random_range((1.0 - JITTER)..=(1.0 + JITTER));
    (interval_secs as f64 * 1000.0 * factor).round() as i64
}

fn elapsed_ms(since: Instant) -> i64 {
    i64::try_from(since.elapsed().as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn page_pace_is_the_span_over_the_number_of_plays() {
        // 20 plays 40 s apart: 19 × 40 s = 760 s, / 20 = 38 s per play.
        assert!(close(page_pace(1_000, 761_000, 20).unwrap(), 38.0));
        assert!(close(page_pace(0, 300_000, 2).unwrap(), 150.0));
        assert_eq!(page_pace(5_000, 5_000, 1), None, "one play has no span");
        assert_eq!(page_pace(0, 0, 0), None);
    }

    #[test]
    fn pace_is_smoothed_by_an_exponential_moving_average() {
        assert!(close(smoothed_pace(None, 40.0), 40.0));
        assert!(close(smoothed_pace(Some(100.0), 40.0), 70.0));
        assert!(close(smoothed_pace(Some(f64::NAN), 40.0), 40.0));
    }

    #[test]
    fn interval_is_twenty_five_plays_at_the_pace_within_the_bounds() {
        assert_eq!(paced_interval_secs(30.0, 1200, 2400), 1200);
        assert_eq!(paced_interval_secs(60.0, 1200, 2400), 1500);
        assert_eq!(paced_interval_secs(72.0, 1200, 2400), 1800);
        assert_eq!(paced_interval_secs(200.0, 1200, 2400), 2400);
        assert_eq!(paced_interval_secs(f64::INFINITY, 1200, 2400), 2400);
        assert_eq!(paced_interval_secs(f64::NAN, 1200, 2400), 1200);
    }

    #[test]
    fn full_page_is_the_minimum_idle_the_maximum_unknown_pace_the_minimum() {
        assert_eq!(next_interval_secs(50, true, Some(200.0), 1200, 2400), 1200);
        assert_eq!(next_interval_secs(0, false, Some(30.0), 1200, 2400), 2400);
        assert_eq!(next_interval_secs(0, false, None, 1200, 2400), 2400);
        assert_eq!(next_interval_secs(3, false, None, 1200, 2400), 1200);
        assert_eq!(next_interval_secs(3, false, Some(72.0), 1200, 2400), 1800);
    }

    #[test]
    fn failures_double_the_interval_up_to_the_maximum() {
        assert_eq!(backoff_interval_secs(1200, 1200, 2400), 2400);
        assert_eq!(backoff_interval_secs(2400, 1200, 2400), 2400);
        assert_eq!(backoff_interval_secs(300, 300, 2400), 600);
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        for _ in 0..1_000 {
            let ms = jittered_ms(120);
            assert!((108_000..=132_000).contains(&ms), "{ms}");
        }
    }
}
