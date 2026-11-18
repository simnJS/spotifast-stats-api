//! Spotify metadata: what a poll stores, and the backfill of imported tracks
//! and of artists, against a wiremock server, an in-memory DB and a simulated
//! clock.

use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spotifast_stats_api::clock::Clock;
use spotifast_stats_api::config::Config;
use spotifast_stats_api::db::{PlaySource, insert_play, upsert_track};
use spotifast_stats_api::spotify::{Backfill, backfill_metadata, poll_user};
use spotifast_stats_api::{AppState, app, auth, db};
use tower::ServiceExt;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Injected "now": 2027-01-15T08:00:00Z, in unix ms.
const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
/// Outcome times include the real duration of the calls: allow this much.
const SLACK_MS: i64 = 5_000;

async fn setup() -> (AppState, MockServer) {
    let server = MockServer::start().await;
    let mut config = Config::for_tests();
    config.spotify_api_base = format!("{}/v1", server.uri());
    config.spotify_accounts_base = server.uri();
    let state = AppState::new(db::test_pool().await, config).with_clock(Clock::simulated(NOW));
    (state, server)
}

/// A user whose access token is valid for a day after `NOW`, and an API token.
async fn user(state: &AppState, spotify_id: &str) -> (i64, String) {
    let id = db::insert_test_user(&state.pool, spotify_id).await.unwrap();
    sqlx::query("UPDATE users SET access_token = 'access-1', token_expires_at = ?1 WHERE id = ?2")
        .bind(NOW / 1000 + 86_400)
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
    let token = auth::issue_token(&state.pool, id).await.unwrap();
    (id, token)
}

/// An imported track (names only), played `plays` times.
async fn imported(
    state: &AppState,
    user_id: i64,
    spotify_id: &str,
    artist: &str,
    plays: i64,
) -> i64 {
    let track = upsert_track(
        &state.pool,
        spotify_id,
        &format!("Song {spotify_id}"),
        artist,
        "Album",
        None,
    )
    .await
    .unwrap();
    for i in 0..plays {
        insert_play(
            &state.pool,
            user_id,
            track,
            NOW - (i + 1) * HOUR,
            60_000,
            PlaySource::Import,
        )
        .await
        .unwrap();
    }
    track
}

/// A Spotify track object: its artists, its album with a cover.
fn track_object(id: &str, artists: &[(&str, &str)], album: (&str, &str)) -> Value {
    json!({
        "id": id,
        "name": format!("Song {id}"),
        "duration_ms": 180_000,
        "artists": artists.iter().map(|(i, n)| json!({ "id": i, "name": n })).collect::<Vec<_>>(),
        "album": {
            "id": album.0,
            "name": album.1,
            "images": [
                { "url": format!("https://img/{}/640", album.0), "width": 640, "height": 640 },
                { "url": format!("https://img/{}/64", album.0), "width": 64, "height": 64 },
            ],
            "artists": [{ "id": artists[0].0, "name": artists[0].1 }],
        },
    })
}

fn artist_object(id: &str, name: &str, genres: &[&str]) -> Value {
    json!({
        "id": id,
        "name": name,
        "images": [{ "url": format!("https://img/{id}"), "width": 640, "height": 640 }],
        "genres": genres,
        "type": "artist",
    })
}

/// Paths of every request the server received so far.
async fn paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect()
}

async fn ok(state: &AppState, uri: &str, token: &str) -> Value {
    let req = Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// `metadata_at_ms` of a track.
async fn metadata_at(state: &AppState, track_id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT metadata_at_ms FROM tracks WHERE id = ?1")
        .bind(track_id)
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

/// The app-wide Spotify rate limit stored in the database (end of the block).
async fn app_block(state: &AppState) -> Option<i64> {
    sqlx::query_scalar("SELECT rate_limited_until_ms FROM app_state WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn backfill_sends_nothing_while_spotify_rate_limits_the_app() {
    let (state, server) = setup().await;
    let (u, _) = user(&state, "u").await;
    let track = imported(&state, u, "t1", "Artist", 2).await;
    sqlx::query(
        "UPDATE app_state SET rate_limited_until_ms = ?1, rate_limited_at_ms = ?2 WHERE id = 1",
    )
    .bind(NOW + HOUR)
    .bind(NOW - 60_000)
    .execute(&state.pool)
    .await
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/v1/tracks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tracks": [track_object("t1", &[("a1", "Artist")], ("al1", "Album"))],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/artists"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "artists": [artist_object("a1", "Artist", &["pop"])],
        })))
        .mount(&server)
        .await;

    let done = backfill_metadata(&state, NOW).await;
    assert_eq!(
        done,
        Backfill {
            blocked_until: Some(NOW + HOUR),
            ..Backfill::default()
        }
    );
    assert!(paths(&server).await.is_empty(), "no call during the block");
    assert_eq!(metadata_at(&state, track).await, None);

    // Once the block is over, the track is looked up.
    let done = backfill_metadata(&state, NOW + HOUR).await;
    assert_eq!(
        done,
        Backfill {
            calls: 2, // the track, then its artist
            tracks: 1,
            artists: 1,
            blocked_until: None,
        }
    );
    assert!(metadata_at(&state, track).await.is_some());
}

#[tokio::test]
async fn backfill_is_off_with_a_zero_budget() {
    let (mut state, server) = setup().await;
    let mut config = (*state.config).clone();
    config.metadata_calls_per_tick = 0;
    state.config = std::sync::Arc::new(config);
    let (u, _) = user(&state, "u").await;
    imported(&state, u, "t1", "Artist", 1).await;
    assert_eq!(backfill_metadata(&state, NOW).await, Backfill::default());
    assert!(paths(&server).await.is_empty());
}

#[tokio::test]
async fn backfill_looks_up_tracks_then_artists_in_batches_most_played_first() {
    let (state, server) = setup().await;
    let (u, token) = user(&state, "u").await;
    imported(&state, u, "t1", "Artist One", 3).await;
    let t2 = imported(&state, u, "t2", "Gone", 1).await;
    Mock::given(method("GET"))
        .and(path("/v1/tracks"))
        .and(query_param("ids", "t1,t2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            // t2 is not on Spotify any more.
            "tracks": [
                track_object("t1", &[("a1", "Artist One"), ("a2", "Artist Two")], ("al1", "Album One")),
                null,
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/artists"))
        .and(query_param("ids", "a1,a2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "artists": [
                artist_object("a1", "Artist One", &["pop", "rock"]),
                artist_object("a2", "Artist Two", &["pop"]),
            ],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let done = backfill_metadata(&state, NOW).await;
    assert_eq!(
        done,
        Backfill {
            calls: 2,
            tracks: 2,
            artists: 2,
            blocked_until: None,
        }
    );
    // Nothing left: the next round sends nothing.
    assert_eq!(
        backfill_metadata(&state, NOW + 15_000).await,
        Backfill::default()
    );
    assert!(
        metadata_at(&state, t2).await.is_some(),
        "not found: not asked again"
    );
    assert!(state.spotify_batch_lookups.load(Ordering::Relaxed));
    // The calls used the user's access token.
    let auth: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            r.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(auth, ["Bearer access-1", "Bearer access-1"]);

    let tracks = ok(&state, "/stats/top/tracks", &token).await;
    let first = &tracks["items"][0];
    assert_eq!(first["spotify_id"], "t1");
    assert_eq!(
        first["artists"],
        json!([
            { "spotify_id": "a1", "name": "Artist One" },
            { "spotify_id": "a2", "name": "Artist Two" },
        ])
    );
    assert_eq!(
        first["album"],
        json!({ "spotify_id": "al1", "name": "Album One", "image_url": "https://img/al1/640" })
    );
    assert_eq!(first["image_url"], "https://img/al1/640");
    // Spotify does not know t2: it keeps its names.
    let second = &tracks["items"][1];
    assert_eq!(
        second["artists"],
        json!([{ "spotify_id": null, "name": "Gone" }])
    );
    let artists = ok(&state, "/stats/top/artists", &token).await;
    assert_eq!(artists["items"][0]["spotify_id"], "a1");
    assert_eq!(artists["items"][0]["image_url"], "https://img/a1");
    let genres = ok(&state, "/stats/top/genres", &token).await;
    assert_eq!(
        genres["items"],
        json!([
            { "genre": "pop", "rank": 1, "previous_rank": null, "plays": 3, "ms_played": 180_000 },
            { "genre": "rock", "rank": 2, "previous_rank": null, "plays": 3, "ms_played": 180_000 },
        ])
    );
}

#[tokio::test]
async fn backfill_looks_items_up_one_by_one_once_batches_are_refused() {
    let (state, server) = setup().await;
    let (u, _) = user(&state, "u").await;
    let t1 = imported(&state, u, "t1", "Artist One", 2).await;
    let t2 = imported(&state, u, "t2", "Gone", 1).await;
    let forbidden = ResponseTemplate::new(403)
        .set_body_json(json!({ "error": { "status": 403, "message": "Forbidden" } }));
    Mock::given(method("GET"))
        .and(path("/v1/tracks"))
        .respond_with(forbidden.clone())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/artists"))
        .respond_with(forbidden)
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/tracks/t1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(track_object(
            "t1",
            &[("a1", "Artist One"), ("a2", "Artist Two")],
            ("al1", "Album One"),
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/tracks/t2"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({ "error": { "status": 404, "message": "Not found" } })),
        )
        .expect(1)
        .mount(&server)
        .await;
    for (id, name) in [("a1", "Artist One"), ("a2", "Artist Two")] {
        Mock::given(method("GET"))
            .and(path(format!("/v1/artists/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(artist_object(
                id,
                name,
                &["pop"],
            )))
            .expect(1)
            .mount(&server)
            .await;
    }

    // Budget 4: the refused batch, t1, t2 (not found), then the first artist.
    let done = backfill_metadata(&state, NOW).await;
    assert_eq!(
        done,
        Backfill {
            calls: 4,
            tracks: 2,
            artists: 1,
            blocked_until: None,
        }
    );
    assert!(!state.spotify_batch_lookups.load(Ordering::Relaxed));
    assert!(metadata_at(&state, t1).await.is_some());
    assert!(metadata_at(&state, t2).await.is_some());
    assert_eq!(
        paths(&server).await,
        [
            "/v1/tracks",
            "/v1/tracks/t1",
            "/v1/tracks/t2",
            "/v1/artists/a1"
        ]
    );
    // The next round goes on one by one, without trying a batch again.
    let done = backfill_metadata(&state, NOW + 15_000).await;
    assert_eq!((done.calls, done.artists), (1, 1));
    assert_eq!(paths(&server).await.last().unwrap(), "/v1/artists/a2");
}

#[tokio::test]
async fn a_429_on_a_lookup_blocks_the_whole_app() {
    let (state, server) = setup().await;
    let (u, _) = user(&state, "u").await;
    let track = imported(&state, u, "t1", "Artist", 1).await;
    Mock::given(method("GET"))
        .and(path("/v1/tracks"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "120"))
        .expect(1)
        .mount(&server)
        .await;

    let done = backfill_metadata(&state, NOW).await;
    assert_eq!((done.calls, done.tracks), (1, 0));
    let until = app_block(&state).await.expect("app-wide block stored");
    assert!(
        (NOW + 120_000..=NOW + 120_000 + SLACK_MS).contains(&until),
        "{until}"
    );
    assert_eq!(metadata_at(&state, track).await, None, "retried later");
    // During the block: nothing more is sent.
    let done = backfill_metadata(&state, NOW + 60_000).await;
    assert_eq!(done.blocked_until, Some(until));
    assert_eq!(paths(&server).await.len(), 1);
}

#[tokio::test]
async fn a_poll_stores_the_artists_and_the_album_of_its_tracks() {
    let (state, server) = setup().await;
    let (u, token) = user(&state, "u").await;
    let played_at = chrono::DateTime::from_timestamp_millis(NOW - HOUR)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Mock::given(method("GET"))
        .and(path("/v1/me/player/recently-played"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [{
                "played_at": played_at,
                "track": track_object("t1", &[("a1", "Artist One"), ("a2", "Artist Two")], ("al1", "Album One")),
            }],
        })))
        .mount(&server)
        .await;
    assert_eq!(poll_user(&state, u, NOW).await.unwrap(), 1);

    let tracks = ok(&state, "/stats/top/tracks", &token).await;
    let first = &tracks["items"][0];
    assert_eq!(
        first["artists"],
        json!([
            { "spotify_id": "a1", "name": "Artist One" },
            { "spotify_id": "a2", "name": "Artist Two" },
        ])
    );
    assert_eq!(first["album"]["spotify_id"], "al1");
    assert_eq!(first["image_url"], "https://img/al1/640");
    let albums = ok(&state, "/stats/top/albums", &token).await;
    assert_eq!(
        albums["items"][0]["artists"],
        json!([{ "spotify_id": "a1", "name": "Artist One" }])
    );
    assert_eq!(albums["items"][0]["uri"], "spotify:album:al1");

    // The track needs no lookup; only its artists' images and genres do.
    Mock::given(method("GET"))
        .and(path("/v1/artists"))
        .and(query_param("ids", "a1,a2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "artists": [artist_object("a1", "Artist One", &[]), null],
        })))
        .expect(1)
        .mount(&server)
        .await;
    let done = backfill_metadata(&state, NOW).await;
    assert_eq!((done.calls, done.tracks, done.artists), (1, 0, 2));
}
