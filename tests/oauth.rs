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
    setup_with(|_| {}).await
}

async fn setup_with(tweak: impl FnOnce(&mut Config)) -> (AppState, MockServer) {
    let server = MockServer::start().await;
    let mut config = Config::for_tests();
    config.spotify_api_base = format!("{}/v1", server.uri());
    config.spotify_accounts_base = server.uri();
    tweak(&mut config);
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
    assert_eq!(
        q["scope"],
        "user-read-recently-played user-read-private user-read-currently-playing"
    );
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

fn req_with(uri: &str, cookie: Option<&str>, headers: &[(&str, &str)]) -> Request<Body> {
    let mut req = Request::builder().uri(uri);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(Body::empty()).unwrap()
}

async fn callback_raw(
    state: &AppState,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, String) {
    let (st, cookie) = login(state).await;
    let resp = app(state.clone())
        .oneshot(req_with(
            &format!("/auth/callback?code=the-code&state={st}"),
            Some(&cookie),
            headers,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let h = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, h, String::from_utf8_lossy(&bytes).into_owned())
}

async fn user_count(state: &AppState) -> i64 {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    n
}

#[tokio::test]
async fn allow_list_refuses_unknown_account() {
    let (state, server) =
        setup_with(|c| c.allowed_spotify_ids = Some(vec!["someone".into()])).await;
    mock_oauth(&server, "sp-user", json!("Spo"), "refresh-1").await;
    let (status, body) = do_callback(&state).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(user_count(&state).await, 0);
    let (tokens,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM api_tokens")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(tokens, 0);
}

#[tokio::test]
async fn allow_list_accepts_listed_account() {
    let (state, server) =
        setup_with(|c| c.allowed_spotify_ids = Some(vec!["x".into(), "sp-user".into()])).await;
    mock_oauth(&server, "sp-user", json!("Spo"), "refresh-1").await;
    let (status, body) = do_callback(&state).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(user_count(&state).await, 1);
}

#[tokio::test]
async fn profile_refused_hints_at_dashboard_user_list() {
    let (state, server) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1", "token_type": "Bearer", "expires_in": 3600,
            "refresh_token": "r",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/me"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let (status, body) = do_callback(&state).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["error"].as_str().unwrap().contains("dashboard"));
    assert_eq!(user_count(&state).await, 0);
}

#[tokio::test]
async fn relogin_resets_poller_state_but_keeps_rate_limit() {
    let (state, server) = setup().await;
    mock_oauth(&server, "sp-user", json!("Spo"), "refresh-1").await;
    let (status, _) = do_callback(&state).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query(
        "UPDATE users SET next_poll_at_ms = 111, last_error = 'boom', \
         last_error_at_ms = 222, rate_limited_until_ms = 333",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let (status, _) = do_callback(&state).await;
    assert_eq!(status, StatusCode::OK);
    let row: (Option<i64>, Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT next_poll_at_ms, last_error, last_error_at_ms, rate_limited_until_ms FROM users",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    assert_eq!(row, (None, None, None, Some(333)));
}

#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    let (state, _server) = setup_with(|c| c.auth_rate_limit_per_min = 3).await;
    for _ in 0..3 {
        let (status, _, _) = call(&state, get_req("/auth/login", None, None)).await;
        assert_eq!(status, StatusCode::FOUND);
    }
    let (status, headers, _) = call(&state, get_req("/auth/login", None, None)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let secs: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&secs));
    // The callback shares the same budget.
    let (status, _, _) = call(&state, get_req("/auth/callback?code=c&state=s", None, None)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn rate_limit_is_per_forwarded_ip_when_trusted() {
    let (state, _s) = setup_with(|c| {
        c.auth_rate_limit_per_min = 1;
        c.trust_x_forwarded_for = true;
    })
    .await;
    let hit = |xff: &'static str| {
        let state = state.clone();
        async move {
            app(state)
                .oneshot(req_with("/auth/login", None, &[("x-forwarded-for", xff)]))
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(hit("1.1.1.1").await, StatusCode::FOUND);
    assert_eq!(hit("1.1.1.1").await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(hit("2.2.2.2").await, StatusCode::FOUND);
    // Only the right-most address counts: a spoofed left part changes nothing.
    assert_eq!(hit("9.9.9.9, 1.1.1.1").await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn rate_limit_ignores_forwarded_for_when_not_trusted() {
    let (state, _s) = setup_with(|c| c.auth_rate_limit_per_min = 1).await;
    let first = app(state.clone())
        .oneshot(req_with(
            "/auth/login",
            None,
            &[("x-forwarded-for", "1.1.1.1")],
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::FOUND);
    let second = app(state.clone())
        .oneshot(req_with(
            "/auth/login",
            None,
            &[("x-forwarded-for", "2.2.2.2")],
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn callback_answers_html_or_json_by_accept_header() {
    let (state, server) = setup().await;
    mock_oauth(
        &server,
        "sp-user",
        json!("<b>\"Eve\" & co</b>"),
        "refresh-1",
    )
    .await;

    let (status, headers, html) = callback_raw(&state, &[("accept", "text/html,*/*;q=0.8")]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(headers["cache-control"], "no-store");
    assert!(html.contains("&lt;b&gt;&quot;Eve&quot; &amp; co&lt;/b&gt;"));
    assert!(!html.contains("<b>"));
    assert!(html.contains("only once"));
    assert!(html.contains("Authorization: Bearer "));
    assert!(html.contains("/stats/summary"));
    assert!(!html.contains("http://") && !html.contains("src="));

    let (status, headers, json_body) =
        callback_raw(&state, &[("accept", "application/json")]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(headers["cache-control"], "no-store");
    let v: Value = serde_json::from_str(&json_body).unwrap();
    assert!(v["token"].is_string());
}
