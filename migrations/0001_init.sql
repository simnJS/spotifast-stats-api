CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    spotify_id TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    token_expires_at INTEGER NOT NULL,
    last_polled_at_ms INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE api_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE tracks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    spotify_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    artist_name TEXT NOT NULL,
    album_name TEXT NOT NULL,
    duration_ms INTEGER
);

CREATE TABLE plays (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    track_id INTEGER NOT NULL REFERENCES tracks(id),
    played_at INTEGER NOT NULL,
    ms_played INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('poll', 'import')),
    UNIQUE (user_id, track_id, played_at)
);

CREATE INDEX idx_plays_user_played_at ON plays (user_id, played_at);
