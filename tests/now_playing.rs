//! `GET /now-playing` tests, against a wiremock server and an in-memory DB.
//! Each test has its own mock server, so the per-user in-memory cache (keyed by
//! the Spotify API base) is never shared between tests.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spotifast_stats_api::config::Config;
use spotifast_stats_api::{AppState, app, auth, db};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CURRENT: &str = "/v1/me/player/currently-playing";

struct Env {
    state: AppState,
    server: MockServer,
    user_id: i64,
    /// API token for `/now-playing`.
    token: String,
}

/// A user whose Spotify access token (`spotify-access`) is valid for a day.
async fn setup() -> Env {
    let server = MockServer::start().await;
    let mut config = Config::for_tests();
    config.spotify_api_base = format!("{}/v1", server.uri());
    config.spotify_accounts_base = server.uri();
    let state = AppState::new(db::test_pool().await, config);
    let user_id = db::insert_test_user(&state.pool, "listener").await.unwrap();
    sqlx::query(
        "UPDATE users SET access_token = 'spotify-access', token_expires_at = ?1 WHERE id = ?2",
    )
    .bind(chrono::Utc::now().timestamp() + 86_400)
    .bind(user_id)
    .execute(&state.pool)
    .await
    .unwrap();
    let token = auth::issue_token(&state.pool, user_id).await.unwrap();
    Env {
        state,
        server,
        user_id,
        token,
    }
}

async fn get(env: &Env, token: Option<&str>) -> (StatusCode, HeaderMap, Value) {
    let mut req = Request::builder().uri("/now-playing");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app(env.state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

fn playing_body() -> Value {
    json!({
        "is_playing": true,
        "progress_ms": 61_000,
        "currently_playing_type": "track",
        "item": {
            "id": "track1", "name": "Song", "duration_ms": 200_000,
            "artists": [{"name": "Artist"}, {"name": "Guest"}],
            "album": {"name": "Album"},
        }
    })
}

/// Mount a mock answering `template` that must be called exactly `calls` times.
async fn mock_current(server: &MockServer, template: ResponseTemplate, calls: u64) {
    Mock::given(method("GET"))
        .and(path(CURRENT))
        .and(header("authorization", "Bearer spotify-access"))
        .respond_with(template)
        .expect(calls)
        .mount(server)
        .await;
}

async fn rate_limited_until(env: &Env) -> Option<i64> {
    sqlx::query_scalar("SELECT rate_limited_until_ms FROM users WHERE id = ?1")
        .bind(env.user_id)
        .fetch_one(&env.state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn playing_track_is_returned() {
    let env = setup().await;
    mock_current(
        &env.server,
        ResponseTemplate::new(200).set_body_json(playing_body()),
        1,
    )
    .await;

    let (status, _, body) = get(&env, Some(&env.token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["is_playing"], true);
    assert_eq!(body["progress_ms"], 61_000);
    assert_eq!(
        body["track"],
        json!({
            "spotify_id": "track1", "name": "Song", "artist_name": "Artist",
            "album_name": "Album", "duration_ms": 200_000,
        })
    );
    let fetched_at = body["fetched_at"].as_str().unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(fetched_at).is_ok());
}

#[tokio::test]
async fn nothing_playing_is_204() {
    let env = setup().await;
    mock_current(&env.server, ResponseTemplate::new(204), 1).await;

    let (status, _, body) = get(&env, Some(&env.token)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);
}

#[tokio::test]
async fn cache_hit_makes_no_second_call() {
    let env = setup().await;
    // `expect(1)` is checked when the server drops: a second call fails the test.
    mock_current(
        &env.server,
        ResponseTemplate::new(200).set_body_json(playing_body()),
        1,
    )
    .await;

    let (_, _, first) = get(&env, Some(&env.token)).await;
    let (status, _, second) = get(&env, Some(&env.token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, second, "same cached answer, same fetched_at");
    assert_eq!(env.server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn nothing_playing_is_cached_too() {
    let env = setup().await;
    mock_current(&env.server, ResponseTemplate::new(204), 1).await;

    assert_eq!(get(&env, Some(&env.token)).await.0, StatusCode::NO_CONTENT);
    assert_eq!(get(&env, Some(&env.token)).await.0, StatusCode::NO_CONTENT);
    assert_eq!(env.server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn rate_limited_user_makes_no_call() {
    let env = setup().await;
    // Not even a token refresh: expire the token, nothing must be sent.
    sqlx::query("UPDATE users SET rate_limited_until_ms = ?1, token_expires_at = 0 WHERE id = ?2")
        .bind(chrono::Utc::now().timestamp_millis() + 120_000)
        .bind(env.user_id)
        .execute(&env.state.pool)
        .await
        .unwrap();

    let (status, headers, body) = get(&env, Some(&env.token)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let retry_after: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((100..=120).contains(&retry_after), "{retry_after}");
    assert!(body["error"].is_string());
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_429_is_stored_and_stops_further_calls() {
    let env = setup().await;
    mock_current(
        &env.server,
        ResponseTemplate::new(429).insert_header("Retry-After", "3600"),
        1,
    )
    .await;

    let before = chrono::Utc::now().timestamp_millis();
    let (status, headers, _) = get(&env, Some(&env.token)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let retry_after: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((3_500..=3_600).contains(&retry_after), "{retry_after}");
    let until = rate_limited_until(&env).await.unwrap();
    assert!(until >= before + 3_600_000, "{until}");

    // The deadline is stored: the next request makes no call (the mock allows one).
    let (status, _, _) = get(&env, Some(&env.token)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(env.server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn missing_scope_asks_to_log_in_again_without_retrying() {
    let env = setup().await;
    mock_current(
        &env.server,
        ResponseTemplate::new(403).set_body_json(
            json!({"error": {"status": 403, "message": "Insufficient client scope"}}),
        ),
        1,
    )
    .await;

    for _ in 0..2 {
        let (status, _, body) = get(&env, Some(&env.token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let message = body["error"].as_str().unwrap();
        assert!(message.contains("log in again"), "{message}");
        assert!(message.contains("/auth/login"), "{message}");
    }
    assert_eq!(
        env.server.received_requests().await.unwrap().len(),
        1,
        "the second request must not ask Spotify again"
    );
}

#[tokio::test]
async fn token_is_refreshed_only_when_expired() {
    let env = setup().await;
    sqlx::query("UPDATE users SET token_expires_at = 0 WHERE id = ?1")
        .bind(env.user_id)
        .execute(&env.state.pool)
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": "spotify-new", "expires_in": 3600, "token_type": "Bearer"}),
        ))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path(CURRENT))
        .and(header("authorization", "Bearer spotify-new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(playing_body()))
        .expect(1)
        .mount(&env.server)
        .await;

    let (status, _, _) = get(&env, Some(&env.token)).await;
    assert_eq!(status, StatusCode::OK);
    let stored: String = sqlx::query_scalar("SELECT access_token FROM users WHERE id = ?1")
        .bind(env.user_id)
        .fetch_one(&env.state.pool)
        .await
        .unwrap();
    assert_eq!(stored, "spotify-new");
}

#[tokio::test]
async fn bearer_token_is_required() {
    let env = setup().await;
    let (status, _, body) = get(&env, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());
    let (status, _, _) = get(&env, Some("not-a-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(env.server.received_requests().await.unwrap().is_empty());
}
