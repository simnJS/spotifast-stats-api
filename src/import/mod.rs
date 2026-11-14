//! Import of Spotify "Extended streaming history" files.
//!
//! `POST /import` takes the raw JSON array of one `Streaming_History_Audio_*.json`
//! file and stores its plays (source `import`) for the authenticated user.
//!
//! # Duplicates
//!
//! An entry's `ts` is the END of the stream; it was listened during
//! `[ts - ms_played, ts]`. Polled plays store Spotify's recently-played
//! `played_at`, and it is not established whether that marks the start or the
//! end of the listen. An entry is therefore a duplicate when the user already
//! has a play of the same track whose `played_at` is within
//! `[ts - ms_played - DEDUP_WINDOW_MS, ts + DEDUP_WINDOW_MS]`: the whole listen
//! plus a 60 s tolerance. That covers a poll stamped at the start, at the end,
//! or anywhere between, and re-imports (existing import rows have
//! `played_at == ts`). A genuinely separate earlier listen of the same track,
//! stamped before that interval, is still imported. The cost: an immediate
//! replay whose earlier poll row sits inside the interval is treated as a
//! duplicate; this is accepted to avoid double counting.
//! The comparison is made against plays existing before the import, so close
//! repeats inside one file are kept (exact repeats are caught by the unique key).

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

use crate::AppState;
use crate::auth::AuthUser;
use crate::db::PlaySource;
use crate::error::{AppError, AppResult};

/// Maximum accepted body size for this route (files are tens of MB).
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Entries played for less than this are skips, not listens.
pub const MIN_MS_PLAYED: i64 = 30_000;

/// Duplicate tolerance, in ms, added on both sides of the listen (see the
/// module doc). Absorbs clock/rounding skew between the export and Spotify's API.
pub const DEDUP_WINDOW_MS: i64 = 60_000;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new().route(
        "/import",
        post(import_history).layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
    )
}

/// One entry of the file; unknown fields are ignored, optional ones may be null.
#[derive(Debug, Deserialize)]
struct Entry {
    ts: String,
    ms_played: i64,
    master_metadata_track_name: Option<String>,
    master_metadata_album_artist_name: Option<String>,
    master_metadata_album_album_name: Option<String>,
    spotify_track_uri: Option<String>,
}

/// Counts returned by the import.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct ImportSummary {
    pub received: usize,
    pub imported: usize,
    pub skipped_short: usize,
    pub skipped_not_track: usize,
    pub skipped_duplicate: usize,
}

/// An entry that passed the static filters.
struct Candidate {
    spotify_id: String,
    name: String,
    artist: String,
    album: String,
    played_at_ms: i64,
    ms_played: i64,
}

async fn import_history(
    State(state): State<AppState>,
    user: AuthUser,
    body: Bytes,
) -> AppResult<Json<ImportSummary>> {
    let entries: Vec<Entry> = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid streaming history JSON: {e}")))?;

    // Validate everything before touching the database.
    let mut summary = ImportSummary {
        received: entries.len(),
        ..Default::default()
    };
    let mut candidates = Vec::new();
    for (i, e) in entries.into_iter().enumerate() {
        let ts = chrono::DateTime::parse_from_rfc3339(&e.ts)
            .map_err(|err| {
                AppError::BadRequest(format!("entry {i}: invalid `ts` {:?}: {err}", e.ts))
            })?
            .timestamp_millis();
        let spotify_id = e
            .spotify_track_uri
            .as_deref()
            .and_then(|u| u.strip_prefix("spotify:track:"))
            .filter(|id| !id.is_empty());
        let Some(spotify_id) = spotify_id else {
            summary.skipped_not_track += 1;
            continue;
        };
        if e.ms_played < MIN_MS_PLAYED {
            summary.skipped_short += 1;
            continue;
        }
        candidates.push(Candidate {
            spotify_id: spotify_id.to_string(),
            name: e.master_metadata_track_name.unwrap_or_default(),
            artist: e.master_metadata_album_artist_name.unwrap_or_default(),
            album: e.master_metadata_album_album_name.unwrap_or_default(),
            played_at_ms: ts,
            ms_played: e.ms_played,
        });
    }

    let mut tx = state.pool.begin().await?;
    let (imported, duplicates) = store(&mut tx, user.user_id, candidates).await?;
    tx.commit().await?;
    summary.imported = imported;
    summary.skipped_duplicate = duplicates;
    Ok(Json(summary))
}

/// Insert the candidates; returns `(imported, duplicates)`.
///
/// Duplicates are detected against the plays that existed BEFORE this import
/// (so two genuine close replays inside one file are both kept), plus exact
/// `(track, played_at)` repeats inside the file.
async fn store(
    conn: &mut SqliteConnection,
    user_id: i64,
    candidates: Vec<Candidate>,
) -> Result<(usize, usize), sqlx::Error> {
    // Phase 1: decide which candidates are new, before inserting anything.
    let mut known_tracks: HashMap<String, Option<i64>> = HashMap::new();
    let mut fresh = Vec::with_capacity(candidates.len());
    let mut duplicates = 0;
    for c in candidates {
        if !known_tracks.contains_key(&c.spotify_id) {
            let id: Option<(i64,)> = sqlx::query_as("SELECT id FROM tracks WHERE spotify_id = ?1")
                .bind(&c.spotify_id)
                .fetch_optional(&mut *conn)
                .await?;
            known_tracks.insert(c.spotify_id.clone(), id.map(|r| r.0));
        }
        if let Some(track_id) = known_tracks[&c.spotify_id] {
            let (exists,): (bool,) = sqlx::query_as(
                "SELECT EXISTS(SELECT 1 FROM plays
                 WHERE user_id = ?1 AND played_at BETWEEN ?2 AND ?3 AND track_id = ?4)",
            )
            .bind(user_id)
            .bind(c.played_at_ms - c.ms_played - DEDUP_WINDOW_MS)
            .bind(c.played_at_ms + DEDUP_WINDOW_MS)
            .bind(track_id)
            .fetch_one(&mut *conn)
            .await?;
            if exists {
                duplicates += 1;
                continue;
            }
        }
        fresh.push(c);
    }

    // Phase 2: upsert tracks and insert plays.
    let mut track_ids: HashMap<String, i64> = HashMap::new();
    let mut imported = 0;
    for c in fresh {
        let track_id = match track_ids.get(&c.spotify_id) {
            Some(id) => *id,
            None => {
                // Existing name/artist/album/duration are never overwritten by
                // empty values; duration stays NULL when unknown.
                let (id,): (i64,) = sqlx::query_as(
                    "INSERT INTO tracks (spotify_id, name, artist_name, album_name, duration_ms)
                     VALUES (?1, ?2, ?3, ?4, NULL)
                     ON CONFLICT(spotify_id) DO UPDATE SET
                         name = COALESCE(NULLIF(tracks.name, ''), excluded.name),
                         artist_name = COALESCE(NULLIF(tracks.artist_name, ''), excluded.artist_name),
                         album_name = COALESCE(NULLIF(tracks.album_name, ''), excluded.album_name)
                     RETURNING id",
                )
                .bind(&c.spotify_id)
                .bind(&c.name)
                .bind(&c.artist)
                .bind(&c.album)
                .fetch_one(&mut *conn)
                .await?;
                track_ids.insert(c.spotify_id.clone(), id);
                id
            }
        };
        let result = sqlx::query(
            "INSERT OR IGNORE INTO plays (user_id, track_id, played_at, ms_played, source)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(user_id)
        .bind(track_id)
        .bind(c.played_at_ms)
        .bind(c.ms_played)
        .bind(PlaySource::Import.as_str())
        .execute(&mut *conn)
        .await?;
        if result.rows_affected() == 1 {
            imported += 1;
        } else {
            duplicates += 1;
        }
    }
    Ok((imported, duplicates))
}
