//! SQLite pool, migrations and shared helpers.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{SqliteConnection, SqlitePool};

/// Source of a play row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaySource {
    Poll,
    Import,
}

impl PlaySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Poll => "poll",
            Self::Import => "import",
        }
    }
}

/// Open (creating the file if needed) the database, with WAL and foreign keys,
/// and run the migrations.
pub async fn connect(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    if let Some(parent) = options.get_filename().parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(sqlx::Error::Io)?;
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Run the embedded migrations.
pub async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| sqlx::Error::Migrate(Box::new(e)))
}

/// A fresh, migrated, in-memory pool for tests (single connection so the
/// in-memory database is shared by every query).
pub async fn test_pool() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .expect("valid url")
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("in-memory pool");
    migrate(&pool).await.expect("migrations");
    pool
}

/// Insert or update a track by its Spotify id; returns the internal track id.
pub async fn upsert_track(
    pool: &SqlitePool,
    spotify_id: &str,
    name: &str,
    artist_name: &str,
    album_name: &str,
    duration_ms: Option<i64>,
) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO tracks (spotify_id, name, artist_name, album_name, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(spotify_id) DO UPDATE SET
             name = excluded.name,
             artist_name = excluded.artist_name,
             album_name = excluded.album_name,
             duration_ms = COALESCE(excluded.duration_ms, tracks.duration_ms)
         RETURNING id",
    )
    .bind(spotify_id)
    .bind(name)
    .bind(artist_name)
    .bind(album_name)
    .bind(duration_ms)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Insert a play; returns `false` when it already existed
/// (same user, track and `played_at_ms`).
pub async fn insert_play(
    pool: &SqlitePool,
    user_id: i64,
    track_id: i64,
    played_at_ms: i64,
    ms_played: i64,
    source: PlaySource,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "INSERT OR IGNORE INTO plays (user_id, track_id, played_at, ms_played, source)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(user_id)
    .bind(track_id)
    .bind(played_at_ms)
    .bind(ms_played)
    .bind(source.as_str())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// An artist as named on a track or an album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistRef {
    pub spotify_id: String,
    pub name: String,
}

/// A track's album, as Spotify gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumMetadata {
    pub spotify_id: String,
    pub name: String,
    pub image_url: Option<String>,
    pub artists: Vec<ArtistRef>,
}

/// What Spotify says of a track beyond its name: its credited artists, in
/// order, and its album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMetadata {
    pub artists: Vec<ArtistRef>,
    pub album: Option<AlbumMetadata>,
}

/// Insert or rename an artist by its Spotify id; returns its row id.
async fn upsert_artist(
    conn: &mut SqliteConnection,
    artist: &ArtistRef,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO artists (spotify_id, name) VALUES (?1, ?2)
         ON CONFLICT(spotify_id) DO UPDATE SET name = excluded.name
         RETURNING id",
    )
    .bind(&artist.spotify_id)
    .bind(&artist.name)
    .fetch_one(&mut *conn)
    .await
}

/// Insert or update an album and its artists; returns its row id. A known
/// cover is never erased by a missing one.
async fn upsert_album(
    conn: &mut SqliteConnection,
    album: &AlbumMetadata,
) -> Result<i64, sqlx::Error> {
    let album_id: i64 = sqlx::query_scalar(
        "INSERT INTO albums (spotify_id, name, image_url) VALUES (?1, ?2, ?3)
         ON CONFLICT(spotify_id) DO UPDATE SET
             name = excluded.name,
             image_url = COALESCE(excluded.image_url, albums.image_url)
         RETURNING id",
    )
    .bind(&album.spotify_id)
    .bind(&album.name)
    .bind(&album.image_url)
    .fetch_one(&mut *conn)
    .await?;
    if !album.artists.is_empty() {
        sqlx::query("DELETE FROM album_artists WHERE album_id = ?1")
            .bind(album_id)
            .execute(&mut *conn)
            .await?;
        for (position, artist) in album.artists.iter().enumerate() {
            let artist_id = upsert_artist(conn, artist).await?;
            sqlx::query(
                "INSERT INTO album_artists (album_id, position, artist_id) VALUES (?1, ?2, ?3)",
            )
            .bind(album_id)
            .bind(position as i64)
            .bind(artist_id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(album_id)
}

/// Store a track's artists and album (replacing what was known) and mark its
/// metadata as read at `at_ms`. Its `artist_name` and `album_name` become the
/// main artist's and the album's names.
pub async fn store_track_metadata(
    pool: &SqlitePool,
    track_id: i64,
    metadata: &TrackMetadata,
    at_ms: i64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut artist_ids = Vec::with_capacity(metadata.artists.len());
    for artist in &metadata.artists {
        artist_ids.push(upsert_artist(&mut tx, artist).await?);
    }
    let album_id = match &metadata.album {
        Some(album) => Some(upsert_album(&mut tx, album).await?),
        None => None,
    };
    if !artist_ids.is_empty() {
        sqlx::query("DELETE FROM track_artists WHERE track_id = ?1")
            .bind(track_id)
            .execute(&mut *tx)
            .await?;
        for (position, artist_id) in artist_ids.iter().enumerate() {
            sqlx::query(
                "INSERT INTO track_artists (track_id, position, artist_id) VALUES (?1, ?2, ?3)",
            )
            .bind(track_id)
            .bind(position as i64)
            .bind(artist_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    let main_artist = metadata.artists.first().map_or("", |a| a.name.as_str());
    let album_name = metadata.album.as_ref().map_or("", |a| a.name.as_str());
    sqlx::query(
        "UPDATE tracks SET
             album_id = COALESCE(?1, album_id),
             metadata_at_ms = ?2,
             artist_name = COALESCE(NULLIF(?3, ''), artist_name),
             album_name = COALESCE(NULLIF(?4, ''), album_name)
         WHERE id = ?5",
    )
    .bind(album_id)
    .bind(at_ms)
    .bind(main_artist)
    .bind(album_name)
    .bind(track_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Mark a track's metadata as looked up at `at_ms` without anything to store
/// (Spotify does not know it): it is not looked up again.
pub async fn mark_track_metadata_missing(
    pool: &SqlitePool,
    track_id: i64,
    at_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tracks SET metadata_at_ms = ?1 WHERE id = ?2")
        .bind(at_ms)
        .bind(track_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// What Spotify says of an artist beyond its name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtistDetails {
    pub name: String,
    pub image_url: Option<String>,
    pub genres: Vec<String>,
}

/// Store an artist's name, image and genres (replacing the known genres), read
/// from Spotify at `at_ms`. A known name or image is never erased by a missing
/// one. Nothing happens for an artist that is not stored yet.
pub async fn store_artist_details(
    pool: &SqlitePool,
    spotify_id: &str,
    details: &ArtistDetails,
    at_ms: i64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let artist_id: Option<i64> = sqlx::query_scalar(
        "UPDATE artists SET
             name = COALESCE(NULLIF(?1, ''), name),
             image_url = COALESCE(?2, image_url),
             details_at_ms = ?3
         WHERE spotify_id = ?4
         RETURNING id",
    )
    .bind(&details.name)
    .bind(&details.image_url)
    .bind(at_ms)
    .bind(spotify_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(artist_id) = artist_id {
        sqlx::query("DELETE FROM artist_genres WHERE artist_id = ?1")
            .bind(artist_id)
            .execute(&mut *tx)
            .await?;
        for genre in details.genres.iter().filter(|g| !g.trim().is_empty()) {
            sqlx::query("INSERT OR IGNORE INTO artist_genres (artist_id, genre) VALUES (?1, ?2)")
                .bind(artist_id)
                .bind(genre.trim())
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await
}

/// Mark an artist's details as looked up at `at_ms` without anything to store
/// (Spotify does not know it): it is not looked up again.
pub async fn mark_artist_details_missing(
    pool: &SqlitePool,
    artist_id: i64,
    at_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE artists SET details_at_ms = ?1 WHERE id = ?2")
        .bind(at_ms)
        .bind(artist_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Insert a dummy user row and return its id (helper for tests).
pub async fn insert_test_user(pool: &SqlitePool, spotify_id: &str) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO users (spotify_id, display_name, access_token, refresh_token, token_expires_at)
         VALUES (?1, ?2, 'access', 'refresh', 0) RETURNING id",
    )
    .bind(spotify_id)
    .bind(format!("User {spotify_id}"))
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upsert_track_is_idempotent() {
        let pool = test_pool().await;
        let a = upsert_track(&pool, "sp1", "Song", "Artist", "Album", Some(1000))
            .await
            .unwrap();
        let b = upsert_track(&pool, "sp1", "Song", "Artist", "Album", None)
            .await
            .unwrap();
        assert_eq!(a, b);
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tracks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let (dur,): (Option<i64>,) = sqlx::query_as("SELECT duration_ms FROM tracks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(dur, Some(1000));
    }

    #[tokio::test]
    async fn insert_play_detects_duplicates() {
        let pool = test_pool().await;
        let user = insert_test_user(&pool, "u1").await.unwrap();
        let track = upsert_track(&pool, "sp1", "S", "A", "B", None)
            .await
            .unwrap();
        assert!(
            insert_play(&pool, user, track, 1_000, 200, PlaySource::Poll)
                .await
                .unwrap()
        );
        assert!(
            !insert_play(&pool, user, track, 1_000, 200, PlaySource::Import)
                .await
                .unwrap()
        );
        assert!(
            insert_play(&pool, user, track, 2_000, 200, PlaySource::Import)
                .await
                .unwrap()
        );
    }
}
