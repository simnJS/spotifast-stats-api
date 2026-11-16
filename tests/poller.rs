//! Spotify poller tests, against a wiremock server and an in-memory DB.

use std::collections::HashMap;

use serde_json::{Value, json};
use spotifast_stats_api::config::Config;
use spotifast_stats_api::spotify::{PollError, poll_all_users, poll_user};
use spotifast_stats_api::{AppState, db};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn setup() -> (AppState, MockServer) {
    let server = MockServer::start().await;
    let mut config = Config::for_tests();
    config.spotify_api_base = format!("{}/v1", server.uri());
    config.spotify_accounts_base = server.uri();
    let state = AppState::new(db::test_pool().await, config);
    (state, server)
}

fn item(track_id: &str, played_at: &str, artists: &[&str]) -> Value {
    json!({
        "played_at": played_at,
        "track": {
            "id": track_id, "name": format!("Song {track_id}"), "duration_ms": 200_000,
            "artists": artists.iter().map(|a| json!({"name": a})).collect::<Vec<_>>(),
            "album": { "name": "Album" },
        }
    })
}

/// A user whose access token is valid for a long time.
async fn fresh_user(state: &AppState, spotify_id: &str, access: &str) -> i64 {
    let id = db::insert_test_user(&state.pool, spotify_id).await.unwrap();
    sqlx::query("UPDATE users SET access_token = ?1, token_expires_at = ?2 WHERE id = ?3")
        .bind(access)
        .bind(chrono::Utc::now().timestamp() + 3600)
        .bind(id)
        .execute(&state.pool)
        .await
        .unwrap();
    id
}

async fn mock_recent(server: &MockServer, bearer: &str, items: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path("/v1/me/player/recently-played"))
        .and(header("authorization", format!("Bearer {bearer}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": items })))
        .mount(server)
        .await;
}

async fn play_count(state: &AppState, user_id: i64) -> i64 {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plays WHERE user_id = ?1")
        .bind(user_id)
        .fetch_one(&state.pool)
        .await
        .unwrap();
    n
}

#[tokio::test]
async fn spotify_poll_inserts_plays_and_is_idempotent() {
    let (state, server) = setup().await;
    let user = fresh_user(&state, "u1", "tok").await;
    mock_recent(
        &server,
        "tok",
        vec![
            item("t2", "2024-01-01T10:05:00.500Z", &["Second", "Feat"]),
            item("t1", "2024-01-01T10:00:00.000Z", &["First"]),
        ],
    )
    .await;

    assert_eq!(poll_user(&state, user).await.unwrap(), 2);
    // Same data again: nothing new.
    assert_eq!(poll_user(&state, user).await.unwrap(), 0);
    assert_eq!(play_count(&state, user).await, 2);

    let row: (String, String, i64, i64, String) = sqlx::query_as(
        "SELECT t.artist_name, t.album_name, p.played_at, p.ms_played, p.source
         FROM plays p JOIN tracks t ON t.id = p.track_id WHERE t.spotify_id = 't2'",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Second");
    assert_eq!(row.1, "Album");
    assert_eq!(row.2, 1_704_103_500_500);
    assert_eq!(row.3, 200_000);
    assert_eq!(row.4, "poll");

    let (cursor,): (Option<i64>,) = sqlx::query_as("SELECT last_polled_at_ms FROM users")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(cursor, Some(1_704_103_500_500));

    // The second request carried the cursor as `after`.
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].url.query().unwrap().contains("limit=50"));
    assert!(!requests[0].url.query().unwrap().contains("after"));
    assert!(
        requests[1]
            .url
            .query_pairs()
            .any(|(k, v)| k == "after" && v == "1704103500500")
    );
}

#[tokio::test]
async fn spotify_expired_token_is_refreshed_before_polling() {
    let (state, server) = setup().await;
    // token_expires_at = 0: expired.
    let user = db::insert_test_user(&state.pool, "u1").await.unwrap();
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access", "token_type": "Bearer", "expires_in": 3600,
        })))
        .expect(1)
        .mount(&server)
        .await;
    mock_recent(
        &server,
        "new-access",
        vec![item("t1", "2024-01-01T10:00:00Z", &["A"])],
    )
    .await;

    assert_eq!(poll_user(&state, user).await.unwrap(), 1);

    let (access, refresh, exp): (String, String, i64) =
        sqlx::query_as("SELECT access_token, refresh_token, token_expires_at FROM users")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(access, "new-access");
    assert_eq!(refresh, "refresh", "old refresh token is kept when omitted");
    assert!(exp > chrono::Utc::now().timestamp() + 3000);

    // Now valid: a second poll must not refresh again (expect(1) verified on drop).
    poll_user(&state, user).await.unwrap();
}

#[tokio::test]
async fn spotify_two_users_keep_separate_plays() {
    let (state, server) = setup().await;
    let a = fresh_user(&state, "ua", "tok-a").await;
    let b = fresh_user(&state, "ub", "tok-b").await;
    mock_recent(
        &server,
        "tok-a",
        vec![item("shared", "2024-01-01T10:00:00Z", &["X"])],
    )
    .await;
    mock_recent(
        &server,
        "tok-b",
        vec![
            item("shared", "2024-01-01T10:00:00Z", &["X"]),
            item("only-b", "2024-01-01T11:00:00Z", &["Y"]),
        ],
    )
    .await;

    let mut blocked = HashMap::new();
    poll_all_users(&state, &mut blocked).await;

    assert_eq!(play_count(&state, a).await, 1);
    assert_eq!(play_count(&state, b).await, 2);
    let (tracks,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tracks")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(tracks, 2);
}

#[tokio::test]
async fn spotify_one_users_failure_does_not_stop_others_and_429_is_honoured() {
    let (state, server) = setup().await;
    let bad = fresh_user(&state, "bad", "tok-bad").await;
    let good = fresh_user(&state, "good", "tok-good").await;
    Mock::given(method("GET"))
        .and(path("/v1/me/player/recently-played"))
        .and(header("authorization", "Bearer tok-bad"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "7"))
        .mount(&server)
        .await;
    mock_recent(
        &server,
        "tok-good",
        vec![item("t1", "2024-01-01T10:00:00Z", &["A"])],
    )
    .await;

    match poll_user(&state, bad).await {
        Err(PollError::RateLimited(d)) => assert_eq!(d.as_secs(), 7),
        other => panic!("expected RateLimited, got {other:?}"),
    }

    let mut blocked = HashMap::new();
    poll_all_users(&state, &mut blocked).await;
    assert_eq!(play_count(&state, good).await, 1);
    assert!(blocked.contains_key(&bad));

    // While blocked, the rate-limited user is not called again.
    let before = server.received_requests().await.unwrap().len();
    poll_all_users(&state, &mut blocked).await;
    let calls = &server.received_requests().await.unwrap()[before..];
    assert!(
        calls
            .iter()
            .all(|r| r.headers["authorization"] != "Bearer tok-bad")
    );
}

#[tokio::test]
async fn spotify_revoked_token_fails_only_that_user() {
    let (state, server) = setup().await;
    let bad = db::insert_test_user(&state.pool, "bad").await.unwrap(); // expired
    let good = fresh_user(&state, "good", "tok-good").await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":"invalid_grant"})))
        .mount(&server)
        .await;
    mock_recent(
        &server,
        "tok-good",
        vec![item("t1", "2024-01-01T10:00:00Z", &["A"])],
    )
    .await;

    assert!(matches!(
        poll_user(&state, bad).await,
        Err(PollError::Other(_))
    ));
    let mut blocked = HashMap::new();
    poll_all_users(&state, &mut blocked).await;
    assert_eq!(play_count(&state, good).await, 1);
    assert_eq!(play_count(&state, bad).await, 0);
}
