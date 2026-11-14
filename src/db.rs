//! SQLite pool, migrations and shared helpers.

use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

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
