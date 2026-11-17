use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spotifast_stats_api::clock::Clock;
use spotifast_stats_api::config::Config;
use spotifast_stats_api::spotify::poll_user;
use spotifast_stats_api::{AppState, app, auth, db};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Simulated "now": 2027-01-15T08:00:00Z, in unix ms.
const NOW: i64 = 1_800_000_000_000;

async fn state() -> AppState {
    AppState::new(db::test_pool().await, Config::for_tests())
}

async fn call(
    state: &AppState,
    method: &str,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app(state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn user_with_token(s: &AppState, spotify_id: &str) -> (i64, String) {
    let user = db::insert_test_user(&s.pool, spotify_id).await.unwrap();
    let token = auth::issue_token(&s.pool, user).await.unwrap();
    (user, token)
}

#[tokio::test]
async fn endpoints_require_a_token() {
    let s = state().await;
    for (m, uri) in [
        ("GET", "/me/status"),
        ("GET", "/auth/tokens"),
        ("POST", "/auth/token"),
        ("DELETE", "/auth/token/1"),
        ("DELETE", "/me"),
    ] {
        let (status, _) = call(&s, m, uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{m} {uri}");
    }
}

#[tokio::test]
async fn status_defaults_to_nulls() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "a").await;
    let (status, body) = call(&s, "GET", "/me/status", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    for k in [
        "last_poll_ok_at",
        "next_poll_at",
        "poll_interval_secs",
        "secs_per_play",
        "rate_limited_until",
        "last_error",
        "last_error_at",
        "last_played_at",
    ] {
        assert!(body[k].is_null(), "{k}");
    }
    assert_eq!(body["plays_count"], 0);
    // Config::for_tests(): POLL_INTERVAL_SECS and POLL_MAX_INTERVAL_SECS.
    assert_eq!(body["poll_min_interval_secs"], 1200);
    assert_eq!(body["poll_max_interval_secs"], 2400);
}

#[tokio::test]
async fn status_reads_seeded_columns() {
    let s = state().await;
    let (user, token) = user_with_token(&s, "a").await;
    sqlx::query(
        "UPDATE users SET last_poll_ok_at_ms = 1000, next_poll_at_ms = 2000, \
         poll_interval_secs = 1500, secs_per_play = 61.04, last_error = 'boom', \
         last_error_at_ms = 4000, last_polled_at_ms = 5000 WHERE id = ?1",
    )
    .bind(user)
    .execute(&s.pool)
    .await
    .unwrap();
    let t = db::upsert_track(&s.pool, "t", "N", "A", "B", Some(1))
        .await
        .unwrap();
    db::insert_play(&s.pool, user, t, 5000, 10, db::PlaySource::Poll)
        .await
        .unwrap();
    let (status, body) = call(&s, "GET", "/me/status", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["last_poll_ok_at"], 1000);
    assert_eq!(body["next_poll_at"], 2000);
    assert_eq!(body["poll_interval_secs"], 1500);
    assert_eq!(body["secs_per_play"], 61.0);
    assert!(body["rate_limited_until"].is_null());
    assert_eq!(body["last_error"], "boom");
    assert_eq!(body["last_error_at"], 4000);
    assert_eq!(body["last_played_at"], 5000);
    assert_eq!(body["plays_count"], 1);
}

#[tokio::test]
async fn status_shows_the_app_wide_rate_limit_and_the_poll_it_delays() {
    let s = state().await.with_clock(Clock::simulated(NOW));
    let (user, token) = user_with_token(&s, "a").await;
    sqlx::query("UPDATE users SET next_poll_at_ms = ?1 WHERE id = ?2")
        .bind(NOW + 60_000)
        .bind(user)
        .execute(&s.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE app_state SET rate_limited_until_ms = ?1, rate_limited_at_ms = ?2")
        .bind(NOW + 600_000)
        .bind(NOW)
        .execute(&s.pool)
        .await
        .unwrap();

    let (_, body) = call(&s, "GET", "/me/status", Some(&token)).await;
    assert_eq!(body["rate_limited_until"], NOW + 600_000);
    assert_eq!(
        body["next_poll_at"],
        NOW + 600_000,
        "not before the block ends"
    );

    // Over: no block, the scheduled poll again.
    s.clock.set_ms(NOW + 600_000);
    let (_, body) = call(&s, "GET", "/me/status", Some(&token)).await;
    assert!(body["rate_limited_until"].is_null());
    assert_eq!(body["next_poll_at"], NOW + 60_000);
}

#[tokio::test]
async fn status_shows_the_pace_and_the_interval_measured_by_a_poll() {
    let server = MockServer::start().await;
    let mut config = Config::for_tests();
    config.spotify_api_base = format!("{}/v1", server.uri());
    config.spotify_accounts_base = server.uri();
    let s = AppState::new(db::test_pool().await, config).with_clock(Clock::simulated(NOW));
    let (user, token) = user_with_token(&s, "a").await;
    sqlx::query(
        "UPDATE users SET access_token = 'tok', token_expires_at = ?1, \
         last_polled_at_ms = ?2 WHERE id = ?3",
    )
    .bind(NOW / 1000 + 86_400)
    .bind(NOW - 3_600_000)
    .bind(user)
    .execute(&s.pool)
    .await
    .unwrap();
    // 10 plays 80 s apart: 9 × 80 s / 10 = 72 s per play, so 25 plays take 1800 s.
    let items: Vec<Value> = (0..10)
        .map(|i| {
            let played_at = chrono::DateTime::from_timestamp_millis(NOW - 5_000 - i * 80_000)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            json!({
                "played_at": played_at,
                "track": {
                    "id": format!("t{i}"), "name": "Song", "duration_ms": 200_000,
                    "artists": [{"name": "A"}], "album": {"name": "B"},
                }
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/v1/me/player/recently-played"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": items })))
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(poll_user(&s, user, NOW).await.unwrap(), 10);
    let (status, body) = call(&s, "GET", "/me/status", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["secs_per_play"], 72.0);
    assert_eq!(body["poll_interval_secs"], 1800);
    assert!(body["rate_limited_until"].is_null());
    let next = body["next_poll_at"].as_i64().unwrap();
    // 1800 s ± 10 % jitter, plus the real duration of the poll.
    assert!(
        (NOW + 1_620_000..=NOW + 1_985_000).contains(&next),
        "{next}"
    );
    assert_eq!(body["last_played_at"], NOW - 5_000);
    assert_eq!(body["plays_count"], 10);
}

#[tokio::test]
async fn token_lifecycle() {
    let s = state().await;
    let (_, first) = user_with_token(&s, "a").await;

    let (status, body) = call(&s, "POST", "/auth/token", Some(&first)).await;
    assert_eq!(status, StatusCode::CREATED);
    let second = body["token"].as_str().unwrap().to_string();
    let second_id = body["id"].as_i64().unwrap();

    // List: two tokens, `current` follows the caller, nothing secret is exposed.
    let (status, body) = call(&s, "GET", "/auth/tokens", Some(&second)).await;
    assert_eq!(status, StatusCode::OK);
    let tokens = body["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2);
    for t in tokens {
        assert_eq!(t["current"], t["id"] == second_id);
        assert!(t.get("token").is_none() && t.get("token_hash").is_none());
        assert!(t["created_at"].as_i64().unwrap() > 1_000_000_000_000);
    }
    assert!(
        tokens
            .iter()
            .all(|t| t["last_used_at"].is_i64() || t["id"] != second_id)
    );
    let raw = body.to_string();
    assert!(!raw.contains(&second) && !raw.contains(&auth::hash_token(&second)));

    // Revoke the first token with the second one.
    let first_id = tokens.iter().find(|t| t["id"] != second_id).unwrap()["id"]
        .as_i64()
        .unwrap();
    let (status, _) = call(
        &s,
        "DELETE",
        &format!("/auth/token/{first_id}"),
        Some(&second),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&s, "GET", "/me", Some(&first)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &s,
        "DELETE",
        &format!("/auth/token/{first_id}"),
        Some(&second),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Revoking the current token is allowed and takes effect at once.
    let (status, _) = call(
        &s,
        "DELETE",
        &format!("/auth/token/{second_id}"),
        Some(&second),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&s, "GET", "/me", Some(&second)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tokens_are_isolated_between_users() {
    let s = state().await;
    let (_, a) = user_with_token(&s, "a").await;
    let (_, b) = user_with_token(&s, "b").await;
    let (_, body) = call(&s, "GET", "/auth/tokens", Some(&b)).await;
    let b_id = body["tokens"][0]["id"].as_i64().unwrap();

    let (_, body) = call(&s, "GET", "/auth/tokens", Some(&a)).await;
    let list = body["tokens"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_ne!(list[0]["id"].as_i64().unwrap(), b_id);

    let (status, _) = call(&s, "DELETE", &format!("/auth/token/{b_id}"), Some(&a)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&s, "GET", "/me", Some(&b)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&s, "DELETE", "/auth/token/99999", Some(&a)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn last_used_is_written_at_most_once_a_minute() {
    let s = state().await;
    let (_, token) = user_with_token(&s, "a").await;
    let read = || async {
        let (v,): (Option<i64>,) = sqlx::query_as("SELECT last_used_at_ms FROM api_tokens")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        v
    };
    assert_eq!(read().await, None);
    call(&s, "GET", "/me", Some(&token)).await;
    let first = read().await.expect("set on first use");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    call(&s, "GET", "/me", Some(&token)).await;
    assert_eq!(read().await, Some(first), "no write within a minute");

    // An old value is refreshed.
    sqlx::query("UPDATE api_tokens SET last_used_at_ms = ?1")
        .bind(first - 120_000)
        .execute(&s.pool)
        .await
        .unwrap();
    call(&s, "GET", "/me", Some(&token)).await;
    assert!(read().await.unwrap() >= first);
}

#[tokio::test]
async fn delete_account_removes_user_data_only() {
    let s = state().await;
    let (a, a_token) = user_with_token(&s, "a").await;
    let (b, b_token) = user_with_token(&s, "b").await;
    let t = db::upsert_track(&s.pool, "t", "N", "A", "B", Some(1))
        .await
        .unwrap();
    db::insert_play(&s.pool, a, t, 1, 1, db::PlaySource::Poll)
        .await
        .unwrap();
    db::insert_play(&s.pool, b, t, 2, 1, db::PlaySource::Poll)
        .await
        .unwrap();

    let (status, _) = call(&s, "DELETE", "/me", Some(&a_token)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = call(&s, "GET", "/me", Some(&a_token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let count = |sql: &'static str, id: i64| {
        let pool = s.pool.clone();
        async move {
            let (n,): (i64,) = sqlx::query_as(sql).bind(id).fetch_one(&pool).await.unwrap();
            n
        }
    };
    assert_eq!(
        count("SELECT COUNT(*) FROM plays WHERE user_id = ?1", a).await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM api_tokens WHERE user_id = ?1", a).await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM users WHERE id = ?1", a).await,
        0
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM plays WHERE user_id = ?1", b).await,
        1
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM tracks WHERE id = ?1", t).await,
        1
    );
    let (status, _) = call(&s, "GET", "/me", Some(&b_token)).await;
    assert_eq!(status, StatusCode::OK);
}
