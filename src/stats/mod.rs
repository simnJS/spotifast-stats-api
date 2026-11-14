//! Per-user listening statistics.
//!
//! Endpoints (all require a bearer token and only read the caller's plays):
//!
//! - `GET /stats/summary`
//! - `GET /stats/top/tracks`, `/stats/top/artists`, `/stats/top/albums`
//! - `GET /stats/listening-time?bucket=day|week|month`
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

mod period;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
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
