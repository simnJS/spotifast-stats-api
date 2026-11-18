//! Per-user listening statistics.
//!
//! Endpoints (all require a bearer token and only read the caller's plays):
//!
//! - `GET /stats/summary`
//! - `GET /stats/top/tracks`, `/stats/top/artists`, `/stats/top/albums`,
//!   `/stats/top/genres`
//! - `GET /stats/item/{track|artist|album}/{spotify_id}`
//! - `GET /stats/listening-time?bucket=day|week|month&tz=`
//! - `GET /stats/hours?tz=` (hour of day and day of week)
//! - `GET /stats/streaks?tz=`
//! - `GET /stats/compare?with=<user_id>`
//! - `GET /history`
//!
//! # Period
//!
//! Except for `/history` and `/stats/item`, endpoints accept either `range`
//! (`4w`, `6m`, `1y`, `lifetime`; default `lifetime`) or explicit `from` /
//! `to`. `range` is relative to the current time (`4w` = 28 days, `6m`/`1y` =
//! calendar months). `from` (inclusive) and `to` (exclusive) are each either
//! unix milliseconds or an RFC 3339 timestamp; either may be omitted. `range`
//! cannot be combined with `from`/`to`, and `from` must be before `to`. Invalid
//! values give 400.
//!
//! # What a play counts for
//!
//! See [`catalog`]: its track, every artist credited on it, its album, and the
//! genres of those artists (once per play). Tracks whose artists and album are
//! not known yet (imported, not backfilled) count for their artist and album
//! names. `summary`'s distinct counts and the tops use the same rule.
//!
//! # Tops
//!
//! Top endpoints take `sort` (`plays`, default, or `duration`), `limit`
//! (default 50, 1..=200) and `offset`. Ties are broken deterministically by
//! the secondary metric, then by name. Every item has `rank` (1 = first, in
//! that order) and `previous_rank`: its rank, in the same order, over the
//! period of the same length that ends where this one starts (`4w`: the 28
//! days before; an open `to` stands for now); `null` when it was not played
//! then, and always for a period without a start (`lifetime`).
//!
//! # Listings
//!
//! `listening-time` buckets are calendar days, weeks (starting on Monday) or
//! months in the time zone `tz`; `start` is the bucket's first instant (unix
//! ms) and only non-empty buckets are returned, oldest first. `/history` is
//! newest first, takes `limit` and `before` (exclusive `played_at` cursor, unix
//! ms or RFC 3339) and returns `next_before`, the cursor for the next page
//! (null when the page was not full).
//!
//! # Time zone (`tz`)
//!
//! `/stats/listening-time`, `/stats/hours` and `/stats/streaks` accept `tz`, an
//! IANA time zone name such as `Europe/Paris` (default `UTC`; an unknown name
//! gives 400). Hours, weekdays and calendar days are computed in that zone,
//! daylight saving time included. Everything else (periods, timestamps in
//! responses) stays in UTC.
//!
//! # Hours
//!
//! `/stats/hours` takes the period parameters and `tz`, and returns `hours`
//! (24 entries `{hour, plays, ms_played}`, hour 0..=23) and `weekdays` (7
//! entries `{weekday, plays, ms_played}`, 1 = Monday .. 7 = Sunday). Both are
//! always complete, empty buckets are zeros.
//!
//! # Item
//!
//! `/stats/item/{kind}/{spotify_id}` gives, over the whole history, the plays
//! and listening time of one track, artist or album, its first and last play,
//! its rank in the lifetime top and in the `4w` top (by plays; `null` when not
//! played in the last 28 days) and, for an artist or an album, its 5 most
//! played tracks. 404 when the caller never played it.
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

mod catalog;
mod period;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::{AppError, AppResult};
use catalog::{Catalog, Count, Key, Kind, Sort, track_counts};
use period::{Period, parse_instant, parse_limit, parse_offset};

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stats/summary", get(summary))
        .route("/stats/top/tracks", get(top_tracks))
        .route("/stats/top/artists", get(top_artists))
        .route("/stats/top/albums", get(top_albums))
        .route("/stats/top/genres", get(top_genres))
        .route("/stats/item/{kind}/{spotify_id}", get(item))
        .route("/stats/listening-time", get(listening_time))
        .route("/stats/hours", get(hours))
        .route("/stats/streaks", get(streaks))
        .route("/stats/compare", get(compare))
        .route("/history", get(history))
}

/// Items of an item page's `top_tracks`.
const ITEM_TOP_TRACKS: usize = 5;

#[derive(Debug, Deserialize)]
struct PeriodParams {
    range: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

impl PeriodParams {
    fn resolve(&self, now_ms: i64) -> AppResult<Period> {
        Period::resolve(
            self.range.as_deref(),
            self.from.as_deref(),
            self.to.as_deref(),
            now_ms,
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
    tz: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HistoryParams {
    limit: Option<String>,
    before: Option<String>,
}

/// The tracks behind a set of per-track counts, sorted.
fn track_ids<'a>(counts: impl IntoIterator<Item = &'a HashMap<i64, Count>>) -> Vec<i64> {
    let mut ids: Vec<i64> = counts.into_iter().flat_map(|c| c.keys().copied()).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

async fn summary(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<PeriodParams>,
) -> AppResult<Json<Value>> {
    let period = params.resolve(state.clock.now_ms())?;
    let counts = track_counts(&state.pool, user.user_id, period).await?;
    let catalog = Catalog::load(&state.pool, &track_ids([&counts]), false).await?;
    let mut total = Count::default();
    for count in counts.values() {
        total.add(count);
    }
    Ok(Json(json!({
        "plays": total.plays,
        "ms_played": total.ms_played,
        "distinct_tracks": counts.len(),
        "distinct_artists": catalog.distinct(&counts, Kind::Artist),
        "distinct_albums": catalog.distinct(&counts, Kind::Album),
        "first_played_at": total.first_played_at,
        "last_played_at": total.last_played_at,
    })))
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
    limit: usize,
    offset: usize,
}

impl TopParams {
    fn resolve(&self, now_ms: i64) -> AppResult<Top> {
        Ok(Top {
            period: Period::resolve(
                self.range.as_deref(),
                self.from.as_deref(),
                self.to.as_deref(),
                now_ms,
            )?,
            sort: parse_sort(self.sort.as_deref())?,
            limit: usize::try_from(parse_limit(self.limit.as_deref())?).unwrap_or(usize::MAX),
            offset: usize::try_from(parse_offset(self.offset.as_deref())?).unwrap_or(usize::MAX),
        })
    }
}

async fn top_tracks(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<TopParams>,
) -> AppResult<Json<Value>> {
    top(&state, user, &params, Kind::Track).await
}

async fn top_artists(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<TopParams>,
) -> AppResult<Json<Value>> {
    top(&state, user, &params, Kind::Artist).await
}

async fn top_albums(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<TopParams>,
) -> AppResult<Json<Value>> {
    top(&state, user, &params, Kind::Album).await
}

async fn top_genres(
    State(state): State<AppState>,
    user: AuthUser,
    Query(params): Query<TopParams>,
) -> AppResult<Json<Value>> {
    top(&state, user, &params, Kind::Genre).await
}

/// One page of the top of `kind`, each item with its rank and its rank over
/// the period before.
async fn top(
    state: &AppState,
    user: AuthUser,
    params: &TopParams,
    kind: Kind,
) -> AppResult<Json<Value>> {
    let now_ms = state.clock.now_ms();
    let top = params.resolve(now_ms)?;
    let current = track_counts(&state.pool, user.user_id, top.period).await?;
    let before = match top.period.previous(now_ms) {
        Some(previous) => Some(track_counts(&state.pool, user.user_id, previous).await?),
        None => None,
    };
    let ids = track_ids(std::iter::once(&current).chain(before.as_ref()));
    let catalog = Catalog::load(&state.pool, &ids, kind == Kind::Genre).await?;
    let previous_ranks: Option<HashMap<Key, usize>> = before.map(|before| {
        catalog
            .ranking(&before, kind, top.sort)
            .into_iter()
            .enumerate()
            .map(|(i, (key, _))| (key, i + 1))
            .collect()
    });
    let items: Vec<Value> = catalog
        .ranking(&current, kind, top.sort)
        .into_iter()
        .enumerate()
        .skip(top.offset)
        .take(top.limit)
        .map(|(i, (key, count))| {
            let previous_rank = previous_ranks.as_ref().and_then(|r| r.get(&key)).copied();
            item_json(&catalog, &key, i + 1, previous_rank, &count)
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// A top item: what it is (ids, names, images) and its figures.
fn item_json(
    catalog: &Catalog,
    key: &Key,
    rank: usize,
    previous_rank: Option<usize>,
    count: &Count,
) -> Value {
    let mut item = match key {
        Key::Track(track_id) => match catalog.tracks.get(track_id) {
            Some(track) => {
                let album = catalog.album(&track.album);
                let artists: Vec<Value> = track
                    .artists
                    .iter()
                    .map(|a| {
                        let artist = catalog.artist(a);
                        json!({ "spotify_id": artist.spotify_id, "name": artist.name })
                    })
                    .collect();
                json!({
                    "spotify_id": track.spotify_id,
                    "uri": format!("spotify:track:{}", track.spotify_id),
                    "name": track.name,
                    "image_url": album.image_url,
                    "duration_ms": track.duration_ms,
                    "artists": artists,
                    "album": {
                        "spotify_id": album.spotify_id,
                        "name": album.name,
                        "image_url": album.image_url,
                    },
                    "artist_name": track.artist_name,
                    "album_name": track.album_name,
                })
            }
            None => json!({}),
        },
        Key::Artist(a) => {
            let artist = catalog.artist(a);
            json!({
                "spotify_id": artist.spotify_id,
                "uri": artist.spotify_id.map(|id| format!("spotify:artist:{id}")),
                "name": artist.name,
                "image_url": artist.image_url,
                "artist_name": artist.name,
            })
        }
        Key::Album(a) => {
            let album = catalog.album(a);
            let artists = catalog.album_artists(a);
            let main_artist = artists.first().map_or("", |artist| artist.name);
            let artists: Vec<Value> = artists
                .iter()
                .map(|artist| json!({ "spotify_id": artist.spotify_id, "name": artist.name }))
                .collect();
            json!({
                "spotify_id": album.spotify_id,
                "uri": album.spotify_id.map(|id| format!("spotify:album:{id}")),
                "name": album.name,
                "image_url": album.image_url,
                "artists": artists,
                "album_name": album.name,
                "artist_name": main_artist,
            })
        }
        Key::Genre(genre) => json!({ "genre": genre }),
    };
    if let Some(fields) = item.as_object_mut() {
        fields.insert("rank".into(), json!(rank));
        fields.insert("previous_rank".into(), json!(previous_rank));
        fields.insert("plays".into(), json!(count.plays));
        fields.insert("ms_played".into(), json!(count.ms_played));
    }
    item
}

/// `/stats/item/{kind}/{spotify_id}`: one track's, artist's or album's figures
/// over the whole history.
async fn item(
    State(state): State<AppState>,
    user: AuthUser,
    Path((kind, spotify_id)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let kind = match kind.as_str() {
        "track" => Kind::Track,
        "artist" => Kind::Artist,
        "album" => Kind::Album,
        other => {
            return Err(AppError::BadRequest(format!(
                "invalid kind `{other}`: expected track, artist or album"
            )));
        }
    };
    let not_played = || AppError::NotFound("no play of this item".into());
    let now_ms = state.clock.now_ms();
    let lifetime = Period::resolve(None, None, None, now_ms)?;
    let last_4w = Period::resolve(Some("4w"), None, None, now_ms)?;
    let all = track_counts(&state.pool, user.user_id, lifetime).await?;
    // Every play of the last 28 days is in the lifetime ones: same catalog.
    let recent = track_counts(&state.pool, user.user_id, last_4w).await?;
    let catalog = Catalog::load(&state.pool, &track_ids([&all]), false).await?;
    let key = catalog.find(kind, &spotify_id).ok_or_else(not_played)?;
    let ranked = catalog.ranking(&all, kind, Sort::Plays);
    let (position, count) = ranked
        .iter()
        .enumerate()
        .find(|(_, (k, _))| *k == key)
        .map(|(i, (_, count))| (i, *count))
        .ok_or_else(not_played)?;
    let rank_4w = catalog
        .ranking(&recent, kind, Sort::Plays)
        .iter()
        .position(|(k, _)| *k == key)
        .map(|i| i + 1);
    let top_tracks: Vec<Value> = if kind == Kind::Track {
        Vec::new()
    } else {
        let mut keys = Vec::new();
        let of_item: HashMap<i64, Count> = all
            .iter()
            .filter(|(track_id, _)| {
                catalog.keys_of(kind, **track_id, &mut keys);
                keys.contains(&key)
            })
            .map(|(track_id, count)| (*track_id, *count))
            .collect();
        catalog
            .ranking(&of_item, Kind::Track, Sort::Plays)
            .into_iter()
            .take(ITEM_TOP_TRACKS)
            .filter_map(|(k, count)| match k {
                Key::Track(track_id) => catalog.tracks.get(&track_id).map(|t| {
                    json!({
                        "spotify_id": t.spotify_id,
                        "name": t.name,
                        "plays": count.plays,
                        "ms_played": count.ms_played,
                    })
                }),
                _ => None,
            })
            .collect()
    };
    Ok(Json(json!({
        "plays": count.plays,
        "ms_played": count.ms_played,
        "first_played_at": count.first_played_at,
        "last_played_at": count.last_played_at,
        "rank_lifetime": position + 1,
        "rank_4w": rank_4w,
        "top_tracks": top_tracks,
    })))
}

#[derive(Debug, Clone, Copy)]
enum Bucket {
    Day,
    Week,
    Month,
}

/// The first instant of `date` in `tz`, in unix ms: its midnight, or the end of
/// a daylight saving time gap that skips it.
fn start_of_day_ms(tz: Tz, date: NaiveDate) -> i64 {
    let midnight = date.and_time(NaiveTime::MIN);
    (0..=24 * 4)
        .map(|quarter| midnight + chrono::Duration::minutes(15 * quarter))
        .find_map(|t| tz.from_local_datetime(&t).earliest())
        .map_or_else(
            || midnight.and_utc().timestamp_millis(),
            |t| t.timestamp_millis(),
        )
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
        state.clock.now_ms(),
    )?;
    let bucket = match params.bucket.as_deref() {
        Some("day") => Bucket::Day,
        Some("week") => Bucket::Week,
        Some("month") => Bucket::Month,
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "invalid `bucket` `{other}`: expected day, week or month"
            )));
        }
        None => return Err(AppError::BadRequest("missing `bucket` parameter".into())),
    };
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
    // (plays, ms_played) per bucket start; the start of each first day, cached.
    let mut buckets: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    let mut starts: HashMap<NaiveDate, i64> = HashMap::new();
    for (played_at, ms) in rows {
        let date = local(tz, played_at)?.date_naive();
        let first_day = match bucket {
            Bucket::Day => date,
            Bucket::Week => {
                date - chrono::Duration::days(i64::from(date.weekday().num_days_from_monday()))
            }
            Bucket::Month => date.with_day(1).unwrap_or(date),
        };
        let start = *starts
            .entry(first_day)
            .or_insert_with(|| start_of_day_ms(tz, first_day));
        let entry = buckets.entry(start).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += ms;
    }
    let items: Vec<Value> = buckets
        .into_iter()
        .map(|(start, (plays, ms))| json!({ "start": start, "plays": plays, "ms_played": ms }))
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
        state.clock.now_ms(),
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
    let today = local(tz, state.clock.now_ms())?.date_naive();
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
        state.clock.now_ms(),
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
