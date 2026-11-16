//! Per-user listening statistics.
//!
//! Endpoints (all require a bearer token and only read the caller's plays):
//!
//! - `GET /stats/summary`
//! - `GET /stats/top/tracks`, `/stats/top/artists`, `/stats/top/albums`
//! - `GET /stats/listening-time?bucket=day|week|month`
//! - `GET /stats/hours?tz=` (hour of day and day of week)
//! - `GET /stats/streaks?tz=`
//! - `GET /stats/compare?with=<user_id>`
//! - `GET /history`
//!
//! # Period
//!
//! Except for `/history`, endpoints accept either `range` (`4w`, `6m`, `1y`,
//! `lifetime`; default `lifetime`) or explicit `from` / `to`. `range` is
//! relative to the current time (`4w` = 28 days, `6m`/`1y` = calendar months).
//! `from` (inclusive) and `to` (exclusive) are each either unix milliseconds
//! or an RFC 3339 timestamp; either may be omitted. `range` cannot be combined
//! with `from`/`to`, and `from` must be before `to`. Invalid values give 400.
//!
//! # Listings
//!
//! Top endpoints take `sort` (`plays`, default, or `duration`), `limit`
//! (default 50, 1..=200) and `offset`. Ties are broken deterministically by
//! the secondary metric, then by name. `listening-time` buckets are in UTC
//! (weeks start on Monday) and only non-empty buckets are returned, oldest
//! first. `/history` is newest first, takes `limit` and `before` (exclusive
//! `played_at` cursor, unix ms or RFC 3339) and returns `next_before`, the
//! cursor for the next page (null when the page was not full).
//!
//! # Time zone (`tz`)
//!
//! `/stats/hours` and `/stats/streaks` accept `tz`, an IANA time zone name
//! such as `Europe/Paris` (default `UTC`; an unknown name gives 400). Hours,
//! weekdays and calendar days are computed in that zone, daylight saving time
//! included. Everything else (periods, timestamps in responses) stays in UTC.
//!
//! # Hours
//!
//! `/stats/hours` takes the period parameters and `tz`, and returns `hours`
//! (24 entries `{hour, plays, ms_played}`, hour 0..=23) and `weekdays` (7
//! entries `{weekday, plays, ms_played}`, 1 = Monday .. 7 = Sunday). Both are
//! always complete, empty buckets are zeros.
//!
//! # Streaks
//!
//! `/stats/streaks` takes `tz` and looks at the whole history. A day counts
//! when it has at least one play in the zone. It returns `current`, `longest`
//! (each `{days, start, end}` with `YYYY-MM-DD` dates; `days` is 0 and the
//! dates are null when there is none) and `total_days_listened`. `current` is
//! the run of consecutive days ending today or yesterday (in the zone), else
//! empty. Among equally long runs, `longest` is the earliest.
//!
//! # Compare
//!
//! `/stats/compare?with=<user_id>` takes the period parameters and `limit`
//! (default 20, 1..=100). It compares the caller with another user of the
//! instance (404 if unknown, 400 if it is the caller) and returns the other
//! user's `id` and `display_name` only (never their plays), `similarity`,
//! `common_artists` and `common_tracks`. Common items are those both users
//! played in the period, each with `my_plays` and `their_plays`, ranked by the
//! smaller of the two (then by the sum, then by name).
//!
//! `similarity` is the weighted Jaccard index of the two users' play counts per
//! artist: with `a_i` and `b_i` the plays of artist `i`, it is
//! `sum(min(a_i, b_i)) / sum(max(a_i, b_i))` over every artist played by either
//! user. It is between 0 (nothing in common, or no plays) and 1 (identical
//! play counts) and is computed on all artists, not only the returned page.

mod period;

use std::cmp::Reverse;
use std::collections::BTreeSet;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use period::{Period, parse_instant, parse_limit, parse_offset};

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stats/summary", get(summary))
        .route("/stats/top/tracks", get(top_tracks))
        .route("/stats/top/artists", get(top_artists))
        .route("/stats/top/albums", get(top_albums))
        .route("/stats/listening-time", get(listening_time))
        .route("/stats/hours", get(hours))
        .route("/stats/streaks", get(streaks))
        .route("/stats/compare", get(compare))
        .route("/history", get(history))
}

#[derive(Debug, Deserialize)]
struct PeriodParams {
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

impl PeriodParams {
    fn resolve(&self) -> AppResult<Period> {
        Period::resolve(
            self.range.as_deref(),
            self.from.as_deref(),
            self.to.as_deref(),
            chrono::Utc::now().timestamp_millis(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct TopParams {
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
    sort: Option<String>,
    limit: Option<String>,
    offset: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BucketParams {
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
    bucket: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HistoryParams {
    limit: Option<String>,
    before: Option<String>,
}

async fn summary(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<PeriodParams>,
) -> AppResult<Json<Value>> {
    let period = params.resolve()?;
    let row: (i64, i64, i64, i64, i64, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*),
                COALESCE(SUM(p.ms_played), 0),
                COUNT(DISTINCT p.track_id),
                COUNT(DISTINCT t.artist_name),
                COUNT(DISTINCT t.album_name || char(31) || t.artist_name),
                MIN(p.played_at),
                MAX(p.played_at)
         FROM plays p JOIN tracks t ON t.id = p.track_id
         WHERE p.user_id = ?1 AND p.played_at >= ?2 AND p.played_at < ?3",
    )
    .bind(user.user_id)
    .bind(period.from)
    .bind(period.to)
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(json!({
        "plays": row.0,
        "ms_played": row.1,
        "distinct_tracks": row.2,
        "distinct_artists": row.3,
        "distinct_albums": row.4,
        "first_played_at": row.5,
        "last_played_at": row.6,
    })))
}

#[derive(Clone, Copy)]
enum Sort {
    Plays,
    Duration,
}

fn parse_sort(value: Option<&str>) -> AppResult<Sort> {
    match value {
        None | Some("plays") => Ok(Sort::Plays),
        Some("duration") => Ok(Sort::Duration),
        Some(other) => Err(AppError::BadRequest(format!(
            "invalid `sort` `{other}`: expected plays or duration"
        ))),
    }
}

/// Validated parameters of a top endpoint.
struct Top {
    period: Period,
    sort: Sort,
    limit: i64,
    offset: i64,
}

impl TopParams {
    fn resolve(&self) -> AppResult<Top> {
        Ok(Top {
            period: Period::resolve(
                self.range.as_deref(),
                self.from.as_deref(),
                self.to.as_deref(),
                chrono::Utc::now().timestamp_millis(),
            )?,
            sort: parse_sort(self.sort.as_deref())?,
            limit: parse_limit(self.limit.as_deref())?,
            offset: parse_offset(self.offset.as_deref())?,
        })
    }
}

/// Fixed ORDER BY clauses (never built from user input).
fn order_by(sort: Sort, tiebreak: &'static str) -> String {
    match sort {
        Sort::Plays => format!("ORDER BY plays DESC, ms_played DESC, {tiebreak}"),
        Sort::Duration => format!("ORDER BY ms_played DESC, plays DESC, {tiebreak}"),
    }
}

async fn top_tracks(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<TopParams>,
) -> AppResult<Json<Value>> {
    let top = params.resolve()?;
    let sql = format!(
        "SELECT t.spotify_id, t.name, t.artist_name, t.album_name,
                COUNT(*) AS plays, SUM(p.ms_played) AS ms_played
         FROM plays p JOIN tracks t ON t.id = p.track_id
         WHERE p.user_id = ?1 AND p.played_at >= ?2 AND p.played_at < ?3
         GROUP BY t.id
         {} LIMIT ?4 OFFSET ?5",
        order_by(top.sort, "t.name ASC, t.artist_name ASC, t.id ASC")
    );
    let rows: Vec<(String, String, String, String, i64, i64)> = sqlx::query_as(&sql)
        .bind(user.user_id)
        .bind(top.period.from)
        .bind(top.period.to)
        .bind(top.limit)
        .bind(top.offset)
        .fetch_all(&state.pool)
        .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, artist, album, plays, ms)| {
            json!({
                "spotify_id": id, "name": name, "artist_name": artist,
                "album_name": album, "plays": plays, "ms_played": ms,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn top_artists(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<TopParams>,
) -> AppResult<Json<Value>> {
    let top = params.resolve()?;
    let sql = format!(
        "SELECT t.artist_name, COUNT(*) AS plays, SUM(p.ms_played) AS ms_played
         FROM plays p JOIN tracks t ON t.id = p.track_id
         WHERE p.user_id = ?1 AND p.played_at >= ?2 AND p.played_at < ?3
         GROUP BY t.artist_name
         {} LIMIT ?4 OFFSET ?5",
        order_by(top.sort, "t.artist_name ASC")
    );
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(&sql)
        .bind(user.user_id)
        .bind(top.period.from)
        .bind(top.period.to)
        .bind(top.limit)
        .bind(top.offset)
        .fetch_all(&state.pool)
        .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(artist, plays, ms)| json!({ "artist_name": artist, "plays": plays, "ms_played": ms }),
        )
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn top_albums(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<TopParams>,
) -> AppResult<Json<Value>> {
    let top = params.resolve()?;
    let sql = format!(
        "SELECT t.album_name, t.artist_name, COUNT(*) AS plays, SUM(p.ms_played) AS ms_played
         FROM plays p JOIN tracks t ON t.id = p.track_id
         WHERE p.user_id = ?1 AND p.played_at >= ?2 AND p.played_at < ?3
         GROUP BY t.album_name, t.artist_name
         {} LIMIT ?4 OFFSET ?5",
        order_by(top.sort, "t.album_name ASC, t.artist_name ASC")
    );
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(&sql)
        .bind(user.user_id)
        .bind(top.period.from)
        .bind(top.period.to)
        .bind(top.limit)
        .bind(top.offset)
        .fetch_all(&state.pool)
        .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(album, artist, plays, ms)| {
            json!({
                "album_name": album, "artist_name": artist,
                "plays": plays, "ms_played": ms,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn listening_time(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<BucketParams>,
) -> AppResult<Json<Value>> {
    let period = Period::resolve(
        params.range.as_deref(),
        params.from.as_deref(),
        params.to.as_deref(),
        chrono::Utc::now().timestamp_millis(),
    )?;
    // The bucket expression comes from a fixed match, never from user input.
    let bucket_expr = match params.bucket.as_deref() {
        Some("day") => "date(p.played_at / 1000, 'unixepoch')",
        Some("week") => "date(p.played_at / 1000, 'unixepoch', '-6 days', 'weekday 1')",
        Some("month") => "date(p.played_at / 1000, 'unixepoch', 'start of month')",
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "invalid `bucket` `{other}`: expected day, week or month"
            )));
        }
        None => return Err(AppError::BadRequest("missing `bucket` parameter".into())),
    };
    let sql = format!(
        "SELECT CAST(strftime('%s', {bucket_expr}) AS INTEGER) * 1000 AS start,
                COUNT(*), SUM(p.ms_played)
         FROM plays p
         WHERE p.user_id = ?1 AND p.played_at >= ?2 AND p.played_at < ?3
         GROUP BY start ORDER BY start ASC"
    );
    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(&sql)
        .bind(user.user_id)
        .bind(period.from)
        .bind(period.to)
        .fetch_all(&state.pool)
        .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(start, plays, ms)| json!({ "start": start, "plays": plays, "ms_played": ms }))
        .collect();
    Ok(Json(json!({ "items": items })))
}

async fn history(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<HistoryParams>,
) -> AppResult<Json<Value>> {
    let limit = parse_limit(params.limit.as_deref())?;
    let before = params
        .before
        .as_deref()
        .map(|v| parse_instant("before", v))
        .transpose()?
        .unwrap_or(i64::MAX);
    let rows: Vec<(i64, i64, String, String, String, String)> = sqlx::query_as(
        "SELECT p.played_at, p.ms_played, t.spotify_id, t.name, t.artist_name, t.album_name
         FROM plays p JOIN tracks t ON t.id = p.track_id
         WHERE p.user_id = ?1 AND p.played_at < ?2
         ORDER BY p.played_at DESC, p.id DESC
         LIMIT ?3",
    )
    .bind(user.user_id)
    .bind(before)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;
    let next_before = if rows.len() as i64 == limit {
        rows.last().map(|r| r.0)
    } else {
        None
    };
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(played_at, ms, id, name, artist, album)| {
            json!({
                "played_at": played_at, "ms_played": ms, "spotify_id": id,
                "name": name, "artist_name": artist, "album_name": album,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

fn parse_tz(value: Option<&str>) -> AppResult<Tz> {
    match value {
        None => Ok(Tz::UTC),
        Some(name) => name.parse::<Tz>().map_err(|_| {
            AppError::BadRequest(format!(
                "invalid `tz` `{name}`: expected an IANA time zone name such as Europe/Paris"
            ))
        }),
    }
}

/// A unix ms instant seen in the time zone `tz`.
fn local(tz: Tz, ms: i64) -> AppResult<DateTime<Tz>> {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&tz))
        .ok_or_else(|| AppError::Internal("timestamp out of range".into()))
}

#[derive(Debug, Deserialize)]
struct HoursParams {
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
    tz: Option<String>,
}

async fn hours(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<HoursParams>,
) -> AppResult<Json<Value>> {
    let period = Period::resolve(
        params.range.as_deref(),
        params.from.as_deref(),
        params.to.as_deref(),
        Utc::now().timestamp_millis(),
    )?;
    let tz = parse_tz(params.tz.as_deref())?;
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT p.played_at, p.ms_played
         FROM plays p
         WHERE p.user_id = ?1 AND p.played_at >= ?2 AND p.played_at < ?3",
    )
    .bind(user.user_id)
    .bind(period.from)
    .bind(period.to)
    .fetch_all(&state.pool)
    .await?;
    // (plays, ms_played) per bucket.
    let mut by_hour = [(0i64, 0i64); 24];
    let mut by_weekday = [(0i64, 0i64); 7];
    for (played_at, ms) in rows {
        let at = local(tz, played_at)?;
        let h = at.hour() as usize;
        let w = at.weekday().num_days_from_monday() as usize;
        by_hour[h].0 += 1;
        by_hour[h].1 += ms;
        by_weekday[w].0 += 1;
        by_weekday[w].1 += ms;
    }
    let hours: Vec<Value> = by_hour
        .iter()
        .enumerate()
        .map(|(h, (plays, ms))| json!({ "hour": h, "plays": plays, "ms_played": ms }))
        .collect();
    let weekdays: Vec<Value> = by_weekday
        .iter()
        .enumerate()
        .map(|(w, (plays, ms))| json!({ "weekday": w + 1, "plays": plays, "ms_played": ms }))
        .collect();
    Ok(Json(json!({ "hours": hours, "weekdays": weekdays })))
}

#[derive(Debug, Deserialize)]
struct StreakParams {
    tz: Option<String>,
}

/// A run of consecutive listening days.
struct Run {
    days: i64,
    start: NaiveDate,
    end: NaiveDate,
}

fn run_json(run: Option<&Run>) -> Value {
    match run {
        Some(r) => json!({
            "days": r.days,
            "start": r.start.to_string(),
            "end": r.end.to_string(),
        }),
        None => json!({ "days": 0, "start": null, "end": null }),
    }
}

async fn streaks(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<StreakParams>,
) -> AppResult<Json<Value>> {
    let tz = parse_tz(params.tz.as_deref())?;
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT p.played_at FROM plays p WHERE p.user_id = ?1")
        .bind(user.user_id)
        .fetch_all(&state.pool)
        .await?;
    let mut dates = BTreeSet::new();
    for (played_at,) in rows {
        dates.insert(local(tz, played_at)?.date_naive());
    }
    // Runs of consecutive days, oldest first.
    let mut runs: Vec<Run> = Vec::new();
    for date in &dates {
        match runs.last_mut() {
            Some(r) if r.end.succ_opt() == Some(*date) => {
                r.end = *date;
                r.days += 1;
            }
            _ => runs.push(Run {
                days: 1,
                start: *date,
                end: *date,
            }),
        }
    }
    let today = Utc::now().with_timezone(&tz).date_naive();
    let current = runs
        .last()
        .filter(|r| r.end == today || r.end.succ_opt() == Some(today));
    let longest = runs.iter().max_by_key(|r| (r.days, Reverse(r.start)));
    Ok(Json(json!({
        "current": run_json(current),
        "longest": run_json(longest),
        "total_days_listened": dates.len(),
    })))
}

#[derive(Debug, Deserialize)]
struct CompareParams {
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
    with: Option<String>,
    limit: Option<String>,
}

/// Parse the `limit` of `/stats/compare` (default 20, between 1 and 100).
fn parse_compare_limit(value: Option<&str>) -> AppResult<usize> {
    match value {
        None => Ok(20),
        Some(v) => {
            let n: usize = v
                .parse()
                .map_err(|_| AppError::BadRequest("invalid `limit`: expected an integer".into()))?;
            if !(1..=100).contains(&n) {
                return Err(AppError::BadRequest(
                    "invalid `limit`: must be between 1 and 100".into(),
                ));
            }
            Ok(n)
        }
    }
}

async fn compare(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<CompareParams>,
) -> AppResult<Json<Value>> {
    let period = Period::resolve(
        params.range.as_deref(),
        params.from.as_deref(),
        params.to.as_deref(),
        Utc::now().timestamp_millis(),
    )?;
    let limit = parse_compare_limit(params.limit.as_deref())?;
    let other: i64 = params
        .with
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("missing `with` parameter".into()))?
        .parse()
        .map_err(|_| AppError::BadRequest("invalid `with`: expected a user id".into()))?;
    if other == user.user_id {
        return Err(AppError::BadRequest(
            "cannot compare a user with themselves".into(),
        ));
    }
    let display_name: (String,) = sqlx::query_as("SELECT display_name FROM users WHERE id = ?1")
        .bind(other)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| AppError::NotFound("user not found".into()))?;

    // Every artist played by either user, with both play counts. Ranked by the
    // smaller count, then by the sum, then by name.
    let artists: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT artist_name, mine, theirs FROM (
             SELECT t.artist_name AS artist_name,
                    SUM(CASE WHEN p.user_id = ?1 THEN 1 ELSE 0 END) AS mine,
                    SUM(CASE WHEN p.user_id = ?2 THEN 1 ELSE 0 END) AS theirs
             FROM plays p JOIN tracks t ON t.id = p.track_id
             WHERE p.user_id IN (?1, ?2) AND p.played_at >= ?3 AND p.played_at < ?4
             GROUP BY t.artist_name
         )
         ORDER BY MIN(mine, theirs) DESC, mine + theirs DESC, artist_name ASC",
    )
    .bind(user.user_id)
    .bind(other)
    .bind(period.from)
    .bind(period.to)
    .fetch_all(&state.pool)
    .await?;
    let sum_min: i64 = artists.iter().map(|(_, a, b)| *a.min(b)).sum();
    let sum_max: i64 = artists.iter().map(|(_, a, b)| *a.max(b)).sum();
    let similarity = if sum_max == 0 {
        0.0
    } else {
        sum_min as f64 / sum_max as f64
    };
    let common_artists: Vec<Value> = artists
        .into_iter()
        .filter(|(_, mine, theirs)| *mine > 0 && *theirs > 0)
        .take(limit)
        .map(|(artist, mine, theirs)| {
            json!({ "artist_name": artist, "my_plays": mine, "their_plays": theirs })
        })
        .collect();

    let tracks: Vec<(String, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT spotify_id, name, artist_name, album_name, mine, theirs FROM (
             SELECT t.spotify_id AS spotify_id, t.name AS name,
                    t.artist_name AS artist_name, t.album_name AS album_name,
                    SUM(CASE WHEN p.user_id = ?1 THEN 1 ELSE 0 END) AS mine,
                    SUM(CASE WHEN p.user_id = ?2 THEN 1 ELSE 0 END) AS theirs
             FROM plays p JOIN tracks t ON t.id = p.track_id
             WHERE p.user_id IN (?1, ?2) AND p.played_at >= ?3 AND p.played_at < ?4
             GROUP BY t.id
         )
         WHERE mine > 0 AND theirs > 0
         ORDER BY MIN(mine, theirs) DESC, mine + theirs DESC,
                  name ASC, artist_name ASC, spotify_id ASC
         LIMIT ?5",
    )
    .bind(user.user_id)
    .bind(other)
    .bind(period.from)
    .bind(period.to)
    .bind(limit as i64)
    .fetch_all(&state.pool)
    .await?;
    let common_tracks: Vec<Value> = tracks
        .into_iter()
        .map(|(id, name, artist, album, mine, theirs)| {
            json!({
                "spotify_id": id, "name": name, "artist_name": artist,
                "album_name": album, "my_plays": mine, "their_plays": theirs,
            })
        })
        .collect();

    Ok(Json(json!({
        "with": { "id": other, "display_name": display_name.0 },
        "similarity": similarity,
        "common_artists": common_artists,
        "common_tracks": common_tracks,
    })))
}
