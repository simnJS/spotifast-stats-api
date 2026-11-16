use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use spotifast_stats_api::config::Config;
use spotifast_stats_api::{AppState, app, auth, db};
use tower::ServiceExt;

async fn state() -> AppState {
    AppState::new(db::test_pool().await, Config::for_tests())
}

async fn get(state: &AppState, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder().uri(uri);
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

#[tokio::test]
async fn health_is_ok() {
    let s = state().await;
    let (status, body) = get(&s, "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn me_requires_valid_token() {
    let s = state().await;
    let (status, body) = get(&s, "/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());
    let (status, _) = get(&s, "/me", Some("nope")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let user = db::insert_test_user(&s.pool, "spot-1").await.unwrap();
    let token = auth::issue_token(&s.pool, user).await.unwrap();
    let (status, body) = get(&s, "/me", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], user);
    assert_eq!(body["spotify_id"], "spot-1");
    assert_eq!(body["display_name"], "User spot-1");
}

#[tokio::test]
async fn token_is_stored_hashed() {
    let s = state().await;
    let user = db::insert_test_user(&s.pool, "spot-2").await.unwrap();
    let token = auth::issue_token(&s.pool, user).await.unwrap();
    let (stored,): (String,) = sqlx::query_as("SELECT token_hash FROM api_tokens")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_ne!(stored, token);
    assert_eq!(stored, auth::hash_token(&token));
}

#[tokio::test]
async fn upsert_track_and_insert_play_helpers() {
    let s = state().await;
    let user = db::insert_test_user(&s.pool, "u").await.unwrap();
    let t1 = db::upsert_track(&s.pool, "t", "N", "A", "B", Some(1))
        .await
        .unwrap();
    let t2 = db::upsert_track(&s.pool, "t", "N", "A", "B", Some(1))
        .await
        .unwrap();
    assert_eq!(t1, t2);
    let first = db::insert_play(&s.pool, user, t1, 5, 5, db::PlaySource::Poll)
        .await
        .unwrap();
    let again = db::insert_play(&s.pool, user, t1, 5, 5, db::PlaySource::Poll)
        .await
        .unwrap();
    assert!(first);
    assert!(!again);
}

#[tokio::test]
async fn health_is_503_without_detail_when_db_is_down() {
    let s = state().await;
    s.pool.close().await;
    let (status, body) = get(&s, "/health", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, serde_json::json!({ "status": "error" }));
}
