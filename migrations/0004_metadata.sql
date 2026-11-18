-- Spotify metadata of what is played: the artists and album of each track,
-- with their Spotify ids and images, and the artists' genres. Polled plays
-- bring it with them; imported tracks only have names, and the backfill reads
-- the rest from Spotify later. All *_ms columns are unix milliseconds (UTC).

CREATE TABLE artists (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    spotify_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    -- Widest image Spotify gives. NULL = none, or not read yet.
    image_url TEXT,
    -- When the full artist (image, genres) was read from Spotify, found or
    -- not. NULL = only its id and name are known (from a track): to backfill.
    details_at_ms INTEGER
);
CREATE INDEX idx_artists_name ON artists (name);

CREATE TABLE albums (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    spotify_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    -- Widest cover Spotify gives. NULL = none.
    image_url TEXT
);
CREATE INDEX idx_albums_name ON albums (name);

-- The album's artists, in Spotify's order.
CREATE TABLE album_artists (
    album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    PRIMARY KEY (album_id, position)
);

-- The artists credited on a track, in Spotify's order (position 0 = main).
CREATE TABLE track_artists (
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    PRIMARY KEY (track_id, position)
);
CREATE INDEX idx_track_artists_artist ON track_artists (artist_id);

CREATE TABLE artist_genres (
    artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    genre TEXT NOT NULL,
    PRIMARY KEY (artist_id, genre)
);

-- The track's album. NULL = unknown (imported and not backfilled yet, or
-- Spotify gave none).
ALTER TABLE tracks ADD COLUMN album_id INTEGER REFERENCES albums(id);
-- When the full track (artists with their ids, album) was stored, from a poll
-- or the backfill, found on Spotify or not. NULL = only the names of the
-- history export are known: to backfill.
ALTER TABLE tracks ADD COLUMN metadata_at_ms INTEGER;

-- Plays per track, for the backfill's most-played-first order.
CREATE INDEX idx_plays_track ON plays (track_id);
