//! Spotify metadata of what is played: ids, images and genres.
//!
//! A polled play brings its track's artists (with their ids) and album (with
//! its id and cover): [`track_metadata`] turns them into what is stored. An
//! imported play only has names, so the backfill ([`backfill_metadata`]) reads
//! the rest from Spotify, a little at a time, from the poller's loop:
//! - nothing at all is sent while Spotify rate limits the app (see
//!   [`super::rate_limit`]), and a 429 received here blocks the whole app like
//!   any other;
//! - at most `METADATA_CALLS_PER_TICK` lookups per tick (0 = off);
//! - tracks first, the most played (all users) first, then artists (image and
//!   genres), the most played first;
//! - 50 ids per call with `/tracks?ids=` and `/artists?ids=` while Spotify
//!   serves them. Development Mode apps lost those in 2026: the first 400, 403
//!   or 404 to a batch switches to `/tracks/{id}` and `/artists/{id}`, one call
//!   per item, until the process restarts;
//! - an item Spotify does not know is marked as looked up, and not asked again;
//! - the calls use the access token of a user whose login still holds (the
//!   one expiring last), refreshed first only when it is about to expire.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::client::{
    self, ArtistObject, ClientError, FullArtist, MAX_IDS_PER_LOOKUP, TrackObject, widest_image,
};
use super::poller::{
    PollError, RELOGIN_ERROR, UserState, elapsed_ms, record_failure, valid_access_token,
};
use super::rate_limit;
use crate::AppState;
use crate::clock::fmt_ms;
use crate::db::{self, AlbumMetadata, ArtistDetails, ArtistRef, TrackMetadata};

/// What a polled or looked-up track says of its artists and album, when it
/// names at least one artist and every artist has an id; `None` otherwise (a
/// local file, an incomplete answer). The album is left out when it has no id.
pub fn track_metadata(track: &TrackObject) -> Option<TrackMetadata> {
    let artists = artist_refs(&track.artists)?;
    if artists.is_empty() {
        return None;
    }
    let album = track.album.as_ref().and_then(|album| {
        let spotify_id = album.id.clone().filter(|id| !id.is_empty())?;
        Some(AlbumMetadata {
            spotify_id,
            name: album.name.clone(),
            image_url: widest_image(album.images.as_deref()),
            artists: album
                .artists
                .as_deref()
                .and_then(artist_refs)
                .unwrap_or_default(),
        })
    });
    Some(TrackMetadata { artists, album })
}

/// The artists with their ids; `None` when one has none.
fn artist_refs(artists: &[ArtistObject]) -> Option<Vec<ArtistRef>> {
    artists
        .iter()
        .map(|a| {
            let spotify_id = a.id.clone().filter(|id| !id.is_empty())?;
            Some(ArtistRef {
                spotify_id,
                name: a.name.clone(),
            })
        })
        .collect()
}

/// What one backfill round did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Backfill {
    /// Lookups sent to Spotify (a token refresh is not counted).
    pub calls: usize,
    /// Tracks looked up, found or not.
    pub tracks: usize,
    /// Artists looked up, found or not.
    pub artists: usize,
    /// The end of the block when nothing was sent because Spotify rate limits
    /// the app.
    pub blocked_until: Option<i64>,
}

/// Items to look up: (row id, Spotify id).
type Pending = Vec<(i64, String)>;

enum Work {
    Tracks(Pending),
    Artists(Pending),
}

/// The user whose access token the lookups use.
struct Caller {
    user_id: i64,
    user: UserState,
    access: String,
}

/// One backfill round at `now_ms` (unix ms): at most `METADATA_CALLS_PER_TICK`
/// lookups, none while Spotify rate limits the app. Stops at the first failure;
/// what is left is looked up on a later round.
pub async fn backfill_metadata(state: &AppState, now_ms: i64) -> Backfill {
    let mut done = Backfill::default();
    let budget = usize::try_from(state.config.metadata_calls_per_tick).unwrap_or(usize::MAX);
    if budget == 0 {
        return done;
    }
    match rate_limit::blocked_until(&state.pool, now_ms).await {
        Ok(None) => {}
        Ok(Some(until_ms)) => {
            tracing::debug!(until = %fmt_ms(until_ms), "metadata: Spotify limits the app, no lookup");
            done.blocked_until = Some(until_ms);
            return done;
        }
        Err(e) => {
            tracing::error!(error = %e, "metadata: cannot read the Spotify rate limit");
            return done;
        }
    }
    let started = Instant::now();
    let mut caller: Option<Caller> = None;
    while done.calls < budget {
        let at_ms = now_ms.saturating_add(elapsed_ms(started));
        let batch = state.spotify_batch_lookups.load(Ordering::Relaxed);
        let work = match next_work(state, batch, at_ms).await {
            Ok(Some(work)) => work,
            Ok(None) => break,
            Err(e) => {
                tracing::error!(error = %e, "metadata: cannot list what to look up");
                break;
            }
        };
        if caller.is_none() {
            caller = find_caller(state, at_ms).await;
        }
        let Some(c) = caller.as_ref() else { break };
        let outcome = match &work {
            Work::Tracks(items) => lookup_tracks(state, &c.access, items, batch, at_ms).await,
            Work::Artists(items) => lookup_artists(state, &c.access, items, batch, at_ms).await,
        };
        done.calls += 1;
        match outcome {
            Ok(n) => match work {
                Work::Tracks(_) => done.tracks += n,
                Work::Artists(_) => done.artists += n,
            },
            Err(PollError::Spotify(ClientError::Status {
                status: 400 | 403 | 404,
                ..
            })) if batch => {
                state.spotify_batch_lookups.store(false, Ordering::Relaxed);
                tracing::info!(
                    "metadata: Spotify refuses batch lookups to this app (Development Mode), \
                     looking items up one by one from now on"
                );
            }
            Err(e) => {
                let at_ms = now_ms.saturating_add(elapsed_ms(started));
                failed(state, c.user_id, &c.user, e, at_ms).await;
                break;
            }
        }
    }
    if done.tracks > 0 || done.artists > 0 {
        tracing::info!(
            calls = done.calls,
            tracks = done.tracks,
            artists = done.artists,
            "metadata: looked up on Spotify"
        );
    }
    done
}

/// Is this a plausible Spotify id (base62)? Anything else is never sent.
fn is_spotify_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// What to look up next: up to one call's worth of tracks without metadata,
/// else of artists without details, the most played first. Ids that cannot be
/// Spotify ids are marked as looked up on the way, without a call.
async fn next_work(state: &AppState, batch: bool, at_ms: i64) -> Result<Option<Work>, sqlx::Error> {
    let per_call = if batch { MAX_IDS_PER_LOOKUP } else { 1 };
    let per_call = i64::try_from(per_call).unwrap_or(1);
    loop {
        let tracks: Pending = sqlx::query_as(
            "SELECT t.id, t.spotify_id FROM tracks t
             LEFT JOIN plays p ON p.track_id = t.id
             WHERE t.metadata_at_ms IS NULL
             GROUP BY t.id
             ORDER BY COUNT(p.id) DESC, t.id ASC
             LIMIT ?1",
        )
        .bind(per_call)
        .fetch_all(&state.pool)
        .await?;
        if !tracks.is_empty() {
            let (valid, invalid): (Pending, Pending) =
                tracks.into_iter().partition(|(_, id)| is_spotify_id(id));
            for (track_id, _) in &invalid {
                db::mark_track_metadata_missing(&state.pool, *track_id, at_ms).await?;
            }
            if valid.is_empty() {
                continue;
            }
            return Ok(Some(Work::Tracks(valid)));
        }
        let artists: Pending = sqlx::query_as(
            "SELECT a.id, a.spotify_id FROM artists a
             LEFT JOIN track_artists ta ON ta.artist_id = a.id
             LEFT JOIN plays p ON p.track_id = ta.track_id
             WHERE a.details_at_ms IS NULL
             GROUP BY a.id
             ORDER BY COUNT(p.id) DESC, a.id ASC
             LIMIT ?1",
        )
        .bind(per_call)
        .fetch_all(&state.pool)
        .await?;
        if artists.is_empty() {
            return Ok(None);
        }
        let (valid, invalid): (Pending, Pending) =
            artists.into_iter().partition(|(_, id)| is_spotify_id(id));
        for (artist_id, _) in &invalid {
            db::mark_artist_details_missing(&state.pool, *artist_id, at_ms).await?;
        }
        if valid.is_empty() {
            continue;
        }
        return Ok(Some(Work::Artists(valid)));
    }
}

/// The user whose token the lookups use: one whose login still holds, the
/// token expiring last first, refreshed when it is about to expire. `None` when
/// there is none, or its refresh failed (logged and stored).
async fn find_caller(state: &AppState, now_ms: i64) -> Option<Caller> {
    let user_id: Option<i64> = match sqlx::query_scalar(
        "SELECT id FROM users
         WHERE last_error IS NULL OR last_error <> ?1
         ORDER BY token_expires_at DESC, id ASC
         LIMIT 1",
    )
    .bind(RELOGIN_ERROR)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "metadata: cannot pick a user to call Spotify with");
            return None;
        }
    };
    let Some(user_id) = user_id else {
        tracing::debug!("metadata: no user to call Spotify with");
        return None;
    };
    let user = match UserState::load(state, user_id).await {
        Ok(user) => user,
        Err(e) => {
            tracing::error!(user_id, error = %e, "metadata: cannot load the user");
            return None;
        }
    };
    match valid_access_token(state, user_id, &user, now_ms).await {
        Ok(access) => Some(Caller {
            user_id,
            user,
            access,
        }),
        Err(e) => {
            failed(state, user_id, &user, e, now_ms).await;
            None
        }
    }
}

/// Look up `items` (all in one batch call, or the first one alone) and store
/// what Spotify says; returns how many tracks were looked up.
async fn lookup_tracks(
    state: &AppState,
    access: &str,
    items: &[(i64, String)],
    batch: bool,
    at_ms: i64,
) -> Result<usize, PollError> {
    let found: Vec<(i64, Option<TrackObject>)> = if batch {
        let ids: Vec<&str> = items.iter().map(|(_, id)| id.as_str()).collect();
        let mut tracks = client::get_several_tracks(state, access, &ids)
            .await?
            .into_iter();
        items
            .iter()
            .map(|(row, _)| (*row, tracks.next().flatten()))
            .collect()
    } else {
        let Some((row, id)) = items.first() else {
            return Ok(0);
        };
        vec![(*row, client::get_track(state, access, id).await?)]
    };
    let looked_up = found.len();
    for (track_id, track) in found {
        match track.as_ref().and_then(track_metadata) {
            Some(metadata) => {
                db::store_track_metadata(&state.pool, track_id, &metadata, at_ms).await?
            }
            None => db::mark_track_metadata_missing(&state.pool, track_id, at_ms).await?,
        }
    }
    Ok(looked_up)
}

/// Look up `items` (all in one batch call, or the first one alone) and store
/// their images and genres; returns how many artists were looked up.
async fn lookup_artists(
    state: &AppState,
    access: &str,
    items: &[(i64, String)],
    batch: bool,
    at_ms: i64,
) -> Result<usize, PollError> {
    let found: Vec<(i64, &str, Option<FullArtist>)> = if batch {
        let ids: Vec<&str> = items.iter().map(|(_, id)| id.as_str()).collect();
        let mut artists = client::get_several_artists(state, access, &ids)
            .await?
            .into_iter();
        items
            .iter()
            .map(|(row, id)| (*row, id.as_str(), artists.next().flatten()))
            .collect()
    } else {
        let Some((row, id)) = items.first() else {
            return Ok(0);
        };
        vec![(
            *row,
            id.as_str(),
            client::get_artist(state, access, id).await?,
        )]
    };
    let looked_up = found.len();
    for (artist_id, spotify_id, artist) in found {
        match artist {
            Some(artist) => {
                let details = ArtistDetails {
                    image_url: widest_image(artist.images.as_deref()),
                    genres: artist.genres.unwrap_or_default(),
                    name: artist.name,
                };
                db::store_artist_details(&state.pool, spotify_id, &details, at_ms).await?
            }
            None => db::mark_artist_details_missing(&state.pool, artist_id, at_ms).await?,
        }
    }
    Ok(looked_up)
}

/// A failed lookup or token refresh: logged; what the poller would store for
/// it (app-wide 429 block, refused refresh token, expired access token) is
/// stored too, like for `/now-playing`.
async fn failed(state: &AppState, user_id: i64, user: &UserState, err: PollError, at_ms: i64) {
    if matches!(
        err,
        PollError::Spotify(
            ClientError::RateLimited { .. } | ClientError::InvalidGrant | ClientError::Unauthorized
        )
    ) {
        record_failure(state, user_id, user, &err, at_ms).await;
    }
    tracing::warn!(
        user_id,
        kind = err.kind(),
        status = ?err.status(),
        error = %err,
        "metadata: lookup failed, the rest waits for a later round"
    );
}
