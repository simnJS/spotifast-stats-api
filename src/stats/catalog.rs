//! What each play counts for, and the names, Spotify ids and images shown.
//!
//! A play counts for its track, for each artist credited on it, for its album
//! and for each genre of those artists (once per play, however many of its
//! artists share a genre). A track's artists and album are the ones Spotify
//! gave (`track_artists`, `tracks.album_id`). A track that has none yet (an
//! imported one the backfill has not reached) counts for its `artist_name`
//! and for its album by `album_name` and `artist_name`: the stored artist
//! (album) of that name when there is one, so that it is not listed twice
//! while the backfill runs, else a bare name without a Spotify id.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use sqlx::SqlitePool;

use super::period::Period;

/// Plays and listening time of one item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Count {
    pub plays: i64,
    pub ms_played: i64,
    pub first_played_at: Option<i64>,
    pub last_played_at: Option<i64>,
}

impl Count {
    pub fn add(&mut self, other: &Count) {
        self.plays += other.plays;
        self.ms_played += other.ms_played;
        self.first_played_at = match (self.first_played_at, other.first_played_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.last_played_at = match (self.last_played_at, other.last_played_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}

/// The plays of a user in a period, per track (row id).
pub async fn track_counts(
    pool: &SqlitePool,
    user_id: i64,
    period: Period,
) -> Result<HashMap<i64, Count>, sqlx::Error> {
    let rows: Vec<(i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT track_id, COUNT(*), SUM(ms_played), MIN(played_at), MAX(played_at)
         FROM plays
         WHERE user_id = ?1 AND played_at >= ?2 AND played_at < ?3
         GROUP BY track_id",
    )
    .bind(user_id)
    .bind(period.from)
    .bind(period.to)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(track_id, plays, ms_played, first, last)| {
            let count = Count {
                plays,
                ms_played,
                first_played_at: Some(first),
                last_played_at: Some(last),
            };
            (track_id, count)
        })
        .collect())
}

/// What a top or an item is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Track,
    Artist,
    Album,
    Genre,
}

/// Ranking order: the first metric, then the other, then the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Plays,
    Duration,
}

/// An artist: a stored one (row id), or a bare name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ArtistKey {
    Known(i64),
    Named(String),
}

/// An album: a stored one (row id), or a bare name and artist name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AlbumKey {
    Known(i64),
    Named { name: String, artist: String },
}

/// One ranked item.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    /// A track, by row id.
    Track(i64),
    Artist(ArtistKey),
    Album(AlbumKey),
    Genre(String),
}

pub struct Track {
    pub spotify_id: String,
    pub name: String,
    pub artist_name: String,
    pub album_name: String,
    pub duration_ms: Option<i64>,
    /// Credited artists, main one first; at least one.
    pub artists: Vec<ArtistKey>,
    pub album: AlbumKey,
}

pub struct Artist {
    pub spotify_id: String,
    pub name: String,
    pub image_url: Option<String>,
}

pub struct Album {
    pub spotify_id: String,
    pub name: String,
    pub image_url: Option<String>,
    /// Artist row ids, in Spotify's order.
    pub artists: Vec<i64>,
}

/// An artist or an album as shown: its Spotify id when known, its name, its
/// image when known.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Shown<'a> {
    pub spotify_id: Option<&'a str>,
    pub name: &'a str,
    pub image_url: Option<&'a str>,
}

/// The tracks a set of plays is about, with their artists, albums and genres.
#[derive(Default)]
pub struct Catalog {
    pub tracks: HashMap<i64, Track>,
    pub artists: HashMap<i64, Artist>,
    pub albums: HashMap<i64, Album>,
    /// Stored artist of each name looked up (the first stored).
    artist_by_name: HashMap<String, i64>,
    /// Genres of each stored artist.
    genres: HashMap<i64, Vec<String>>,
}

/// `items` as a JSON array, bound to `json_each(?)`: no limit on its length,
/// unlike a list of SQL parameters.
fn json_list<T: Serialize>(items: &[T]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

type TrackRow = (
    i64,
    String,
    String,
    String,
    String,
    Option<i64>,
    Option<i64>,
);

impl Catalog {
    /// Load the tracks `track_ids` (row ids) and what they count for. Genres
    /// are only read when `with_genres`.
    pub async fn load(
        pool: &SqlitePool,
        track_ids: &[i64],
        with_genres: bool,
    ) -> Result<Self, sqlx::Error> {
        let mut catalog = Catalog::default();
        if track_ids.is_empty() {
            return Ok(catalog);
        }
        let ids = json_list(track_ids);
        let rows: Vec<TrackRow> = sqlx::query_as(
            "SELECT id, spotify_id, name, artist_name, album_name, duration_ms, album_id
             FROM tracks WHERE id IN (SELECT value FROM json_each(?1))",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?;

        // Credited artists of the tracks that have them.
        let credited: Vec<(i64, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT ta.track_id, a.id, a.spotify_id, a.name, a.image_url
             FROM track_artists ta JOIN artists a ON a.id = ta.artist_id
             WHERE ta.track_id IN (SELECT value FROM json_each(?1))
             ORDER BY ta.track_id, ta.position",
        )
        .bind(&ids)
        .fetch_all(pool)
        .await?;
        let mut artists_of: HashMap<i64, Vec<ArtistKey>> = HashMap::new();
        for (track_id, artist_id, spotify_id, name, image_url) in credited {
            artists_of
                .entry(track_id)
                .or_default()
                .push(ArtistKey::Known(artist_id));
            catalog.artists.entry(artist_id).or_insert(Artist {
                spotify_id,
                name,
                image_url,
            });
        }

        // Stored artists named like the artist of a track without credits.
        let mut names: Vec<&str> = rows
            .iter()
            .filter(|r| !artists_of.contains_key(&r.0) && !r.3.is_empty())
            .map(|r| r.3.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        if !names.is_empty() {
            let named: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
                "SELECT id, spotify_id, name, image_url FROM artists
                 WHERE name IN (SELECT value FROM json_each(?1))
                 ORDER BY id",
            )
            .bind(json_list(&names))
            .fetch_all(pool)
            .await?;
            for (artist_id, spotify_id, name, image_url) in named {
                catalog
                    .artist_by_name
                    .entry(name.clone())
                    .or_insert(artist_id);
                catalog.artists.entry(artist_id).or_insert(Artist {
                    spotify_id,
                    name,
                    image_url,
                });
            }
        }

        // Stored albums named like the album of a track without one, with the
        // same main artist.
        let mut album_names: Vec<&str> = rows
            .iter()
            .filter(|r| r.6.is_none() && !r.4.is_empty())
            .map(|r| r.4.as_str())
            .collect();
        album_names.sort_unstable();
        album_names.dedup();
        let mut album_by_name: HashMap<(String, String), i64> = HashMap::new();
        if !album_names.is_empty() {
            let named: Vec<(String, String, i64)> = sqlx::query_as(
                "SELECT al.name, a.name, al.id
                 FROM albums al
                 JOIN album_artists aa ON aa.album_id = al.id AND aa.position = 0
                 JOIN artists a ON a.id = aa.artist_id
                 WHERE al.name IN (SELECT value FROM json_each(?1))
                 ORDER BY al.id",
            )
            .bind(json_list(&album_names))
            .fetch_all(pool)
            .await?;
            for (album, artist, album_id) in named {
                album_by_name.entry((album, artist)).or_insert(album_id);
            }
        }

        // The albums, and their artists.
        let mut album_ids: Vec<i64> = rows
            .iter()
            .filter_map(|r| r.6)
            .chain(album_by_name.values().copied())
            .collect();
        album_ids.sort_unstable();
        album_ids.dedup();
        if !album_ids.is_empty() {
            let album_ids = json_list(&album_ids);
            let albums: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
                "SELECT id, spotify_id, name, image_url FROM albums
                 WHERE id IN (SELECT value FROM json_each(?1))",
            )
            .bind(&album_ids)
            .fetch_all(pool)
            .await?;
            for (album_id, spotify_id, name, image_url) in albums {
                catalog.albums.insert(
                    album_id,
                    Album {
                        spotify_id,
                        name,
                        image_url,
                        artists: Vec::new(),
                    },
                );
            }
            let album_artists: Vec<(i64, i64, String, String, Option<String>)> = sqlx::query_as(
                "SELECT aa.album_id, a.id, a.spotify_id, a.name, a.image_url
                 FROM album_artists aa JOIN artists a ON a.id = aa.artist_id
                 WHERE aa.album_id IN (SELECT value FROM json_each(?1))
                 ORDER BY aa.album_id, aa.position",
            )
            .bind(&album_ids)
            .fetch_all(pool)
            .await?;
            for (album_id, artist_id, spotify_id, name, image_url) in album_artists {
                if let Some(album) = catalog.albums.get_mut(&album_id) {
                    album.artists.push(artist_id);
                }
                catalog.artists.entry(artist_id).or_insert(Artist {
                    spotify_id,
                    name,
                    image_url,
                });
            }
        }

        for (track_id, spotify_id, name, artist_name, album_name, duration_ms, album_id) in rows {
            let artists = match artists_of.remove(&track_id) {
                Some(artists) => artists,
                None => vec![match catalog.artist_by_name.get(&artist_name) {
                    Some(&artist_id) => ArtistKey::Known(artist_id),
                    None => ArtistKey::Named(artist_name.clone()),
                }],
            };
            let album_id = album_id.or_else(|| {
                album_by_name
                    .get(&(album_name.clone(), artist_name.clone()))
                    .copied()
            });
            let album = match album_id {
                Some(album_id) if catalog.albums.contains_key(&album_id) => {
                    AlbumKey::Known(album_id)
                }
                _ => AlbumKey::Named {
                    name: album_name.clone(),
                    artist: artist_name.clone(),
                },
            };
            catalog.tracks.insert(
                track_id,
                Track {
                    spotify_id,
                    name,
                    artist_name,
                    album_name,
                    duration_ms,
                    artists,
                    album,
                },
            );
        }

        if with_genres && !catalog.artists.is_empty() {
            let artist_ids: Vec<i64> = catalog.artists.keys().copied().collect();
            let genres: Vec<(i64, String)> = sqlx::query_as(
                "SELECT artist_id, genre FROM artist_genres
                 WHERE artist_id IN (SELECT value FROM json_each(?1))
                 ORDER BY artist_id, genre",
            )
            .bind(json_list(&artist_ids))
            .fetch_all(pool)
            .await?;
            for (artist_id, genre) in genres {
                catalog.genres.entry(artist_id).or_default().push(genre);
            }
        }
        Ok(catalog)
    }

    /// The items of `kind` a play of `track_id` counts for, each once, into
    /// `out` (cleared first).
    pub fn keys_of(&self, kind: Kind, track_id: i64, out: &mut Vec<Key>) {
        out.clear();
        let Some(track) = self.tracks.get(&track_id) else {
            return;
        };
        let mut push = |key: Key| {
            if !out.contains(&key) {
                out.push(key);
            }
        };
        match kind {
            Kind::Track => push(Key::Track(track_id)),
            Kind::Artist => {
                for artist in &track.artists {
                    push(Key::Artist(artist.clone()));
                }
            }
            Kind::Album => push(Key::Album(track.album.clone())),
            Kind::Genre => {
                for artist in &track.artists {
                    if let ArtistKey::Known(artist_id) = artist {
                        for genre in self.genres.get(artist_id).into_iter().flatten() {
                            push(Key::Genre(genre.clone()));
                        }
                    }
                }
            }
        }
    }

    /// Every item of `kind` the plays `counts` (per track) count for, ranked:
    /// by `sort`, then by name, then by key.
    pub fn ranking(
        &self,
        counts: &HashMap<i64, Count>,
        kind: Kind,
        sort: Sort,
    ) -> Vec<(Key, Count)> {
        let mut totals: HashMap<Key, Count> = HashMap::new();
        let mut keys = Vec::new();
        for (&track_id, count) in counts {
            self.keys_of(kind, track_id, &mut keys);
            for key in keys.drain(..) {
                totals.entry(key).or_default().add(count);
            }
        }
        let mut ranked: Vec<((String, String), Key, Count)> = totals
            .into_iter()
            .map(|(key, count)| (self.label(&key), key, count))
            .collect();
        ranked.sort_by(|a, b| {
            let (x, y) = (&a.2, &b.2);
            let by_metric = match sort {
                Sort::Plays => y.plays.cmp(&x.plays).then(y.ms_played.cmp(&x.ms_played)),
                Sort::Duration => y.ms_played.cmp(&x.ms_played).then(y.plays.cmp(&x.plays)),
            };
            by_metric
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        ranked
            .into_iter()
            .map(|(_, key, count)| (key, count))
            .collect()
    }

    /// How many distinct items of `kind` the plays `counts` count for.
    pub fn distinct(&self, counts: &HashMap<i64, Count>, kind: Kind) -> usize {
        let mut seen: HashSet<Key> = HashSet::new();
        let mut keys = Vec::new();
        for &track_id in counts.keys() {
            self.keys_of(kind, track_id, &mut keys);
            seen.extend(keys.drain(..));
        }
        seen.len()
    }

    /// Tie-break of a ranking: (name, secondary name). Tracks: name, artist;
    /// albums: name, main artist; artists and genres: their name.
    fn label(&self, key: &Key) -> (String, String) {
        match key {
            Key::Track(track_id) => self
                .tracks
                .get(track_id)
                .map(|t| (t.name.clone(), t.artist_name.clone()))
                .unwrap_or_default(),
            Key::Artist(artist) => (self.artist(artist).name.to_string(), String::new()),
            Key::Album(album) => {
                let main_artist = self
                    .album_artists(album)
                    .first()
                    .map(|a| a.name.to_string())
                    .unwrap_or_default();
                (self.album(album).name.to_string(), main_artist)
            }
            Key::Genre(genre) => (genre.clone(), String::new()),
        }
    }

    /// An artist as shown.
    pub fn artist<'a>(&'a self, key: &'a ArtistKey) -> Shown<'a> {
        match key {
            ArtistKey::Known(artist_id) => match self.artists.get(artist_id) {
                Some(a) => Shown {
                    spotify_id: Some(&a.spotify_id),
                    name: &a.name,
                    image_url: a.image_url.as_deref(),
                },
                None => Shown {
                    spotify_id: None,
                    name: "",
                    image_url: None,
                },
            },
            ArtistKey::Named(name) => self.artist_named(name),
        }
    }

    /// The stored artist of that name when there is one, else the bare name.
    fn artist_named<'a>(&'a self, name: &'a str) -> Shown<'a> {
        match self
            .artist_by_name
            .get(name)
            .and_then(|id| self.artists.get(id))
        {
            Some(a) => Shown {
                spotify_id: Some(&a.spotify_id),
                name: &a.name,
                image_url: a.image_url.as_deref(),
            },
            None => Shown {
                spotify_id: None,
                name,
                image_url: None,
            },
        }
    }

    /// An album as shown.
    pub fn album<'a>(&'a self, key: &'a AlbumKey) -> Shown<'a> {
        match key {
            AlbumKey::Known(album_id) => match self.albums.get(album_id) {
                Some(a) => Shown {
                    spotify_id: Some(&a.spotify_id),
                    name: &a.name,
                    image_url: a.image_url.as_deref(),
                },
                None => Shown {
                    spotify_id: None,
                    name: "",
                    image_url: None,
                },
            },
            AlbumKey::Named { name, .. } => Shown {
                spotify_id: None,
                name,
                image_url: None,
            },
        }
    }

    /// An album's artists as shown, main one first.
    pub fn album_artists<'a>(&'a self, key: &'a AlbumKey) -> Vec<Shown<'a>> {
        match key {
            AlbumKey::Known(album_id) => self
                .albums
                .get(album_id)
                .map(|album| {
                    album
                        .artists
                        .iter()
                        .filter_map(|id| self.artists.get(id))
                        .map(|a| Shown {
                            spotify_id: Some(&a.spotify_id),
                            name: &a.name,
                            image_url: a.image_url.as_deref(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            AlbumKey::Named { artist, .. } => vec![self.artist_named(artist)],
        }
    }

    /// The item of `kind` with this Spotify id, when one of the loaded tracks
    /// counts for it.
    pub fn find(&self, kind: Kind, spotify_id: &str) -> Option<Key> {
        match kind {
            Kind::Track => self
                .tracks
                .iter()
                .find(|(_, t)| t.spotify_id == spotify_id)
                .map(|(&id, _)| Key::Track(id)),
            Kind::Artist => self
                .artists
                .iter()
                .find(|(_, a)| a.spotify_id == spotify_id)
                .map(|(&id, _)| Key::Artist(ArtistKey::Known(id))),
            Kind::Album => self
                .albums
                .iter()
                .find(|(_, a)| a.spotify_id == spotify_id)
                .map(|(&id, _)| Key::Album(AlbumKey::Known(id))),
            Kind::Genre => None,
        }
    }
}
