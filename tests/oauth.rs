//! Spotify login tests, against a wiremock server and an in-memory DB.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spotifast_stats_api::config::Config;
use spotifast_stats_api::{AppState, app, db};
use tower::ServiceExt;
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

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Value) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn get_req(uri: &str, cookie: Option<&str>, bearer: Option<&str>) -> Request<Body> {
    let mut req = Request::builder().uri(uri);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    req.body(Body::empty()).unwrap()
}

/// Hit /auth/login; returns (state parameter, cookie to send back).
async fn login(state: &AppState) -> (String, String) {
    let (status, headers, _) = call(state, get_req("/auth/login", None, None)).await;
    assert!(status == StatusCode::FOUND || status == StatusCode::SEE_OTHER);
    let location = headers["location"].to_str().unwrap();
    let url = reqwest::Url::parse(location).unwrap();
    assert_eq!(url.path(), "/authorize");
    let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "test-client");
    assert_eq!(q["redirect_uri"], "http://127.0.0.1/callback");
    assert_eq!(q["scope"], "user-read-recently-played user-read-private");
    let cookie = headers["set-cookie"].to_str().unwrap();
    let cookie = cookie.split(';').next().unwrap().to_string();
    (q["state"].clone(), cookie)
}

async fn mock_oauth(server: &MockServer, spotify_id: &str, display: Value, refresh: &str) {
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(header(
            "authorization",
            "Basic dGVzdC1jbGllbnQ6dGVzdC1zZWNyZXQ=",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1", "token_type": "Bearer", "expires_in": 3600,
            "refresh_token": refresh,
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/me"))
        .and(header("authorization", "Bearer access-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "id": spotify_id, "display_name": display })),
        )
        .mount(server)
        .await;
}

async fn do_callback(state: &AppState) -> (StatusCode, Value) {
    let (st, cookie) = login(state).await;
    let (status, _, body) = call(
        state,
        get_req(
            &format!("/auth/callback?code=the-code&state={st}"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    (status, body)
}

#[tokio::test]
async fn spotify_callback_creates_user_and_working_token() {
    let (state, server) = setup().await;
    mock_oauth(&server, "sp-user", json!("Spo User"), "refresh-1").await;

    let (status, body) = do_callback(&state).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["spotify_id"], "sp-user");
    assert_eq!(body["user"]["display_name"], "Spo User");
    let token = body["token"].as_str().unwrap();

    let (status, _, me) = call(&state, get_req("/me", None, Some(token))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["id"], body["user"]["id"]);
    assert_eq!(me["spotify_id"], "sp-user");

    let (refresh,): (String,) = sqlx::query_as("SELECT refresh_token FROM users")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(refresh, "refresh-1");
}

#[tokio::test]
async fn spotify_second_login_keeps_the_same_user() {
    let (state, server) = setup().await;
    mock_oauth(&server, "sp-user", Value::Null, "refresh-1").await;

    let (_, first) = do_callback(&state).await;
    server.reset().await;
    mock_oauth(&server, "sp-user", json!("Renamed"), "refresh-2").await;
    let (status, second) = do_callback(&state).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(first["user"]["id"], second["user"]["id"]);
    // A null display_name falls back to the Spotify id on first login.
    assert_eq!(first["user"]["display_name"], "sp-user");
    assert_eq!(second["user"]["display_name"], "Renamed");
    assert_ne!(first["token"], second["token"]);
    let (users, refresh): (i64, String) =
        sqlx::query_as("SELECT COUNT(*), MAX(refresh_token) FROM users")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(users, 1);
    assert_eq!(refresh, "refresh-2");
}

#[tokio::test]
async fn spotify_callback_rejects_missing_or_wrong_state() {
    let (state, _server) = setup().await;
    let (st, cookie) = login(&state).await;

    // No state parameter.
    let (status, _, _) = call(
        &state,
        get_req("/auth/callback?code=c", Some(&cookie), None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // No cookie.
    let (status, _, _) = call(
        &state,
        get_req(&format!("/auth/callback?code=c&state={st}"), None, None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Wrong state.
    let (status, _, _) = call(
        &state,
        get_req("/auth/callback?code=c&state=nope", Some(&cookie), None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Forged cookie.
    let forged = "spotify_oauth_state=abc.99999999999.deadbeef";
    let (status, _, _) = call(
        &state,
        get_req("/auth/callback?code=c&state=abc", Some(forged), None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn callback_spotify_error_is_502() {
    let (state, server) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":"invalid_grant"})))
        .mount(&server)
        .await;
    let (status, body) = do_callback(&state).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].is_string());
}
