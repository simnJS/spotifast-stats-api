//! Import of Spotify "Extended streaming history" files.
//!
//! `POST /import` takes the raw JSON array of one `Streaming_History_Audio_*.json`
//! file (`Content-Type: application/json`) and stores its plays (source
//! `import`) for the authenticated user.
//!
//! With `Content-Type: application/zip` the body is the whole export archive:
//! every `Streaming_History_Audio_*.json` entry (in any folder) is imported, in
//! name order, in a single transaction, and the response is a per-file report.
//! Everything else in the archive is ignored. The archive is read in memory
//! only (nothing is written to disk), with limits on the number of entries, the
//! path names and the uncompressed size. Files after the first are deduplicated
//! against the plays stored by the earlier ones.
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
//! The comparison is made against plays existing before the import of the
//! file, so close repeats inside one file are kept (exact repeats are caught by
//! the unique key).

use std::collections::HashMap;
use std::io::{Cursor, Read};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

use crate::AppState;
use crate::auth::AuthUser;
use crate::db::PlaySource;
use crate::error::{AppError, AppResult};

/// Maximum accepted body size for a single JSON file (files are tens of MB).
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Maximum accepted body size for a zip archive (the compressed export).
pub const MAX_ZIP_BODY_BYTES: usize = 200 * 1024 * 1024;

/// Maximum total uncompressed size of the imported files of one archive,
/// enforced while reading (declared sizes are not trusted).
pub const MAX_UNCOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;

/// Maximum number of entries (files and folders) in an archive.
pub const MAX_ZIP_ENTRIES: usize = 10_000;

/// Maximum length, in bytes, of an entry path.
pub const MAX_ENTRY_PATH_BYTES: usize = 512;

/// Entries played for less than this are skips, not listens.
pub const MIN_MS_PLAYED: i64 = 30_000;

/// Duplicate tolerance, in ms, added on both sides of the listen (see the
/// module doc). Absorbs clock/rounding skew between the export and Spotify's API.
pub const DEDUP_WINDOW_MS: i64 = 60_000;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    // The route-wide limit fits zips; a single JSON body is capped by hand.
    Router::new().route(
        "/import",
        post(import_history).layer(DefaultBodyLimit::max(MAX_ZIP_BODY_BYTES)),
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

impl ImportSummary {
    fn add(&mut self, other: &ImportSummary) {
        self.received += other.received;
        self.imported += other.imported;
        self.skipped_short += other.skipped_short;
        self.skipped_not_track += other.skipped_not_track;
        self.skipped_duplicate += other.skipped_duplicate;
    }
}

/// Report line of one file of an archive.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FileReport {
    pub name: String,
    #[serde(flatten)]
    pub counts: ImportSummary,
}

/// Response for a zip archive.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ArchiveReport {
    pub files: Vec<FileReport>,
    pub totals: ImportSummary,
    pub ignored_files: Vec<String>,
}

/// Response of `POST /import`: a summary for a JSON body, a report for a zip.
#[derive(Serialize)]
#[serde(untagged)]
enum ImportResponse {
    Single(ImportSummary),
    Archive(ArchiveReport),
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

/// A file parsed and validated, not yet stored.
struct ParsedFile {
    /// Counters known before touching the database.
    counts: ImportSummary,
    candidates: Vec<Candidate>,
}

/// Parse and validate the content of one history file.
fn parse_history(bytes: &[u8]) -> Result<ParsedFile, String> {
    let entries: Vec<Entry> = serde_json::from_slice(bytes)
        .map_err(|e| format!("invalid streaming history JSON: {e}"))?;

    let mut counts = ImportSummary {
        received: entries.len(),
        ..Default::default()
    };
    let mut candidates = Vec::new();
    for (i, e) in entries.into_iter().enumerate() {
        let ts = chrono::DateTime::parse_from_rfc3339(&e.ts)
            .map_err(|err| format!("entry {i}: invalid `ts` {:?}: {err}", e.ts))?
            .timestamp_millis();
        let spotify_id = e
            .spotify_track_uri
            .as_deref()
            .and_then(|u| u.strip_prefix("spotify:track:"))
            .filter(|id| !id.is_empty());
        let Some(spotify_id) = spotify_id else {
            counts.skipped_not_track += 1;
            continue;
        };
        if e.ms_played < MIN_MS_PLAYED {
            counts.skipped_short += 1;
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
    Ok(ParsedFile { counts, candidates })
}

/// Is the `Content-Type` a zip archive?
fn is_zip(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            let media = v
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            media == "application/zip" || media == "application/x-zip-compressed"
        })
}

/// Reject absolute paths, `..` segments, NULs and over-long names.
fn check_entry_path(name: &str) -> AppResult<()> {
    let bad =
        |why: &str| AppError::BadRequest(format!("unsafe path in zip archive {name:?}: {why}"));
    if name.len() > MAX_ENTRY_PATH_BYTES {
        return Err(bad("path too long"));
    }
    if name.contains('\0') {
        return Err(bad("NUL character"));
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(bad("absolute path"));
    }
    if name.split(['/', '\\']).any(|seg| seg == "..") {
        return Err(bad("`..` segment"));
    }
    Ok(())
}

/// Does this archive path name an audio history file (in any folder)?
fn is_audio_history(name: &str) -> bool {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.starts_with("Streaming_History_Audio_") && base.ends_with(".json")
}

/// The files of an archive, parsed, plus the names of the ignored entries.
struct ParsedArchive {
    files: Vec<(String, ParsedFile)>,
    ignored: Vec<String>,
}

/// Read the archive in memory and parse every matching file, in name order.
/// Any problem is a 400 naming it.
fn parse_archive(body: Bytes) -> AppResult<ParsedArchive> {
    let mut archive = zip::ZipArchive::new(Cursor::new(body))
        .map_err(|e| AppError::BadRequest(format!("invalid zip archive: {e}")))?;
    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(AppError::BadRequest(format!(
            "zip archive has too many entries ({}, maximum {MAX_ZIP_ENTRIES})",
            archive.len()
        )));
    }

    let mut matched: Vec<(String, usize)> = Vec::new();
    let mut ignored: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let file = archive
            .by_index_raw(i)
            .map_err(|e| AppError::BadRequest(format!("invalid zip archive: entry {i}: {e}")))?;
        let name = file
            .name()
            .map_err(|e| AppError::BadRequest(format!("invalid zip archive: entry {i}: {e}")))?
            .into_owned();
        check_entry_path(&name)?;
        if file.is_dir() {
            continue;
        }
        if is_audio_history(&name) {
            matched.push((name, i));
        } else {
            ignored.push(name);
        }
    }
    if matched.is_empty() {
        return Err(AppError::BadRequest(
            "no Streaming_History_Audio_*.json file found in the zip archive".to_string(),
        ));
    }
    matched.sort();
    ignored.sort();

    let mut total: u64 = 0;
    let mut files = Vec::with_capacity(matched.len());
    for (name, index) in matched {
        let file = archive
            .by_index(index)
            .map_err(|e| AppError::BadRequest(format!("invalid zip archive: {name}: {e}")))?;
        let remaining = MAX_UNCOMPRESSED_BYTES - total;
        let mut buf = Vec::new();
        // Declared sizes are not trusted: stop reading once the budget is spent.
        file.take(remaining + 1)
            .read_to_end(&mut buf)
            .map_err(|e| AppError::BadRequest(format!("invalid zip archive: {name}: {e}")))?;
        if buf.len() as u64 > remaining {
            return Err(AppError::BadRequest(format!(
                "zip archive too large once uncompressed (maximum {MAX_UNCOMPRESSED_BYTES} bytes), at {name}"
            )));
        }
        total += buf.len() as u64;
        let parsed =
            parse_history(&buf).map_err(|m| AppError::BadRequest(format!("{name}: {m}")))?;
        files.push((name, parsed));
    }
    Ok(ParsedArchive { files, ignored })
}

async fn import_history(
    State(state): State<AppState>,
    user: AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Response> {
    if is_zip(&headers) {
        // Decompression and parsing are CPU work: keep them off the async threads.
        let parsed = tokio::task::spawn_blocking(move || parse_archive(body))
            .await
            .map_err(|e| AppError::Internal(format!("zip task failed: {e}")))??;

        // One transaction for the whole archive: all or nothing. Files are
        // stored in order, so later files are deduplicated against earlier ones.
        let mut tx = state.pool.begin().await?;
        let mut files = Vec::with_capacity(parsed.files.len());
        let mut totals = ImportSummary::default();
        for (name, file) in parsed.files {
            let mut counts = file.counts;
            let (imported, duplicates) = store(&mut tx, user.user_id, file.candidates).await?;
            counts.imported = imported;
            counts.skipped_duplicate = duplicates;
            totals.add(&counts);
            files.push(FileReport { name, counts });
        }
        tx.commit().await?;
        return Ok(Json(ImportResponse::Archive(ArchiveReport {
            files,
            totals,
            ignored_files: parsed.ignored,
        }))
        .into_response());
    }

    if body.len() > MAX_BODY_BYTES {
        return Ok(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    }
    // Validate everything before touching the database.
    let ParsedFile {
        mut counts,
        candidates,
    } = parse_history(&body).map_err(AppError::BadRequest)?;

    let mut tx = state.pool.begin().await?;
    let (imported, duplicates) = store(&mut tx, user.user_id, candidates).await?;
    tx.commit().await?;
    counts.imported = imported;
    counts.skipped_duplicate = duplicates;
    Ok(Json(ImportResponse::Single(counts)).into_response())
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
