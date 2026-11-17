//! Spotify login tests, against a wiremock server and an in-memory DB.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spotifast_stats_api::clock::Clock;
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
    login_with(state, "/auth/login").await
}

/// `/auth/login` with these query parameters, URL-encoded.
fn login_uri(params: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://x/auth/login").unwrap();
    url.query_pairs_mut().extend_pairs(params);
    format!("/auth/login?{}", url.query().unwrap())
}

/// Hit this login URI; returns (state parameter, cookie to send back).
async fn login_with(state: &AppState, uri: &str) -> (String, String) {
    let (status, headers, _) = call(state, get_req(uri, None, None)).await;
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
    raw(
        state,
        req_with(
            &format!("/auth/callback?code=the-code&state={st}"),
            Some(&cookie),
            headers,
        ),
    )
    .await
}

async fn raw(state: &AppState, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
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
async fn relogin_resets_poller_state() {
    let (state, server) = setup().await;
    mock_oauth(&server, "sp-user", json!("Spo"), "refresh-1").await;
    let (status, _) = do_callback(&state).await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query(
        "UPDATE users SET next_poll_at_ms = 111, last_error = 'boom', last_error_at_ms = 222",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let (status, _) = do_callback(&state).await;
    assert_eq!(status, StatusCode::OK);
    let row: (Option<i64>, Option<String>, Option<i64>) =
        sqlx::query_as("SELECT next_poll_at_ms, last_error, last_error_at_ms FROM users")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(row, (None, None, None));
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
    assert!(headers.get("location").is_none());
    let v: Value = serde_json::from_str(&json_body).unwrap();
    assert!(v["token"].is_string());
}

// ---- desktop apps: app_redirect / app_state (RFC 8252 loopback) ----------

const APP_REDIRECT_ERROR: &str = "app_redirect must be a loopback http URL";

fn location(headers: &axum::http::HeaderMap) -> (reqwest::Url, HashMap<String, String>) {
    let url = reqwest::Url::parse(headers["location"].to_str().unwrap()).unwrap();
    let q = url.query_pairs().into_owned().collect();
    (url, q)
}

fn assert_cookie_cleared(headers: &axum::http::HeaderMap) {
    let set = headers["set-cookie"].to_str().unwrap();
    assert!(set.starts_with("spotify_oauth_state=; Max-Age=0"), "{set}");
}

#[tokio::test]
async fn plain_login_keeps_the_original_cookie_format() {
    let (state, _server) = setup().await;
    let (_, cookie) = login(&state).await;
    // nonce.expires.signature
    assert_eq!(cookie.split('.').count(), 3, "{cookie}");
}

#[tokio::test]
async fn login_accepts_a_loopback_app_redirect() {
    let (state, _server) = setup().await;
    for redirect in [
        "http://127.0.0.1:4321/callback",
        "http://[::1]:4321/cb",
        "http://localhost:4321/cb?x=1",
    ] {
        // `login_with` checks the redirect to Spotify's authorize page.
        let uri = login_uri(&[("app_redirect", redirect), ("app_state", "abc_DEF-123")]);
        let (st, cookie) = login_with(&state, &uri).await;
        assert_eq!(st.len(), 32);
        // nonce.expires.redirect.app_state.signature
        assert_eq!(cookie.split('.').count(), 5, "{cookie}");
        assert!(cookie.contains(".abc_DEF-123."), "{cookie}");
    }
}

#[tokio::test]
async fn login_refuses_a_non_loopback_app_redirect_or_a_bad_app_state() {
    let (state, _server) = setup_with(|c| c.auth_rate_limit_per_min = 1000).await;
    for redirect in [
        "http://example.com:4321/cb",
        "http://127.0.0.2:4321/cb",
        "https://127.0.0.1:4321/cb",
        "https://localhost:4321/cb",
        "http://127.0.0.1/cb",
        "http://localhost/cb",
        "http://127.0.0.1:4321@example.com/cb",
        "http://user:pw@127.0.0.1:4321/cb",
        "http://127.0.0.1:4321/cb#frag",
        "not a url",
    ] {
        let uri = login_uri(&[("app_redirect", redirect), ("app_state", "s")]);
        let (status, headers, body) = call(&state, get_req(&uri, None, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{redirect}");
        assert_eq!(body, json!({ "error": APP_REDIRECT_ERROR }), "{redirect}");
        assert!(headers.get("set-cookie").is_none());
        assert!(headers.get("location").is_none());
    }
    for app_state in ["a".repeat(129), "with space".into(), "dot.dot".into()] {
        let uri = login_uri(&[
            ("app_redirect", "http://127.0.0.1:4321/cb"),
            ("app_state", app_state.as_str()),
        ]);
        let (status, _, body) = call(&state, get_req(&uri, None, None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{app_state}");
        assert!(
            body["error"].as_str().unwrap().starts_with("app_state"),
            "{body}"
        );
    }
}

#[tokio::test]
async fn callback_redirects_the_token_to_the_app() {
    let (state, server) = setup().await;
    mock_oauth(&server, "sp-user", json!("Spo"), "refresh-1").await;
    let uri = login_uri(&[
        ("app_redirect", "http://127.0.0.1:4321/done?x=1"),
        ("app_state", "xyz-1"),
    ]);
    let (st, cookie) = login_with(&state, &uri).await;

    // Even a browser asking for HTML gets the redirect, not the token page.
    let (status, headers, body) = raw(
        &state,
        req_with(
            &format!("/auth/callback?code=the-code&state={st}"),
            Some(&cookie),
            &[("accept", "text/html")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    assert!(!body.contains("only once"));
    assert_eq!(headers["cache-control"], "no-store");
    assert_cookie_cleared(&headers);

    let loc = headers["location"].to_str().unwrap();
    assert!(
        loc.starts_with("http://127.0.0.1:4321/done?x=1&token="),
        "{loc}"
    );
    assert!(loc.ends_with("&state=xyz-1"), "{loc}");
    let (url, q) = location(&headers);
    assert_eq!(url.path(), "/done");
    assert_eq!(q["x"], "1");
    assert_eq!(q["state"], "xyz-1");
    assert!(!q.contains_key("error"));

    let (status, _, me) = call(&state, get_req("/me", None, Some(q["token"].as_str()))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["spotify_id"], "sp-user");
}

#[tokio::test]
async fn callback_redirects_errors_to_the_app() {
    let (state, server) = setup().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":"invalid_grant"})))
        .mount(&server)
        .await;

    // The user refused on Spotify's consent page.
    let uri = login_uri(&[
        ("app_redirect", "http://[::1]:4321/cb"),
        ("app_state", "s1"),
    ]);
    let (st, cookie) = login_with(&state, &uri).await;
    let (status, headers, _) = raw(
        &state,
        get_req(
            &format!("/auth/callback?error=access_denied&state={st}"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    assert_cookie_cleared(&headers);
    let (url, q) = location(&headers);
    assert_eq!(url.host_str(), Some("[::1]"));
    assert_eq!(url.port(), Some(4321));
    assert_eq!(url.path(), "/cb");
    assert_eq!(q["error"], "Spotify denied access: access_denied");
    assert_eq!(q["state"], "s1");
    assert!(!q.contains_key("token"));

    // A bad code: Spotify refuses the exchange. No app_state, so no `state`.
    let uri = login_uri(&[("app_redirect", "http://localhost:4321/cb")]);
    let (st, cookie) = login_with(&state, &uri).await;
    let (status, headers, _) = raw(
        &state,
        get_req(
            &format!("/auth/callback?code=bad&state={st}"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    let (url, q) = location(&headers);
    assert_eq!(url.host_str(), Some("localhost"));
    assert!(q["error"].contains("token exchange failed"), "{q:?}");
    assert!(!q.contains_key("state"));
    assert!(!q.contains_key("token"));
    assert_eq!(user_count(&state).await, 0);
}

#[tokio::test]
async fn app_redirect_in_the_cookie_cannot_be_tampered_with() {
    let (state, server) = setup().await;
    mock_oauth(&server, "sp-user", json!("Spo"), "refresh-1").await;
    let uri = login_uri(&[
        ("app_redirect", "http://127.0.0.1:4321/cb"),
        ("app_state", "s1"),
    ]);
    let (st, cookie) = login_with(&state, &uri).await;
    let f: Vec<&str> = cookie.split('.').collect();
    let callback = format!("/auth/callback?code=the-code&state={st}");

    let other = hex::encode("http://127.0.0.1:9999/cb");
    let swapped = format!("{}.{}.{other}.{}.{}", f[0], f[1], f[3], f[4]);
    let restated = format!("{}.{}.{}.evil.{}", f[0], f[1], f[2], f[4]);
    // Dropping the app fields does not turn it into a valid plain cookie either.
    let downgraded = format!("{}.{}.{}", f[0], f[1], f[4]);
    for forged in [swapped.as_str(), restated.as_str(), downgraded.as_str()] {
        let (status, headers, _) = call(&state, get_req(&callback, Some(forged), None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{forged}");
        assert!(headers.get("location").is_none());
    }

    // A wrong state with a genuine cookie is a plain error, not a redirect.
    let (status, headers, _) = call(
        &state,
        get_req(
            "/auth/callback?code=the-code&state=nope",
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(headers.get("location").is_none());
    assert_eq!(user_count(&state).await, 0);
}

// ---- while Spotify rate limits the app (HTTP 429) ---------------------------

/// Simulated "now": 2027-01-15T08:00:00Z, in unix ms.
const NOW: i64 = 1_800_000_000_000;
const HTML: (&str, &str) = ("accept", "text/html,application/xhtml+xml,*/*;q=0.8");
const JSON: (&str, &str) = ("accept", "application/json");

/// [`setup`] on a clock stopped at [`NOW`].
async fn setup_at_now() -> (AppState, MockServer) {
    let (state, server) = setup().await;
    (state.with_clock(Clock::simulated(NOW)), server)
}

/// Store an app-wide block until `until_ms`, as a 429 anywhere would.
async fn block_app(state: &AppState, until_ms: i64) {
    sqlx::query("UPDATE app_state SET rate_limited_until_ms = ?1, rate_limited_at_ms = ?2")
        .bind(until_ms)
        .bind(NOW)
        .execute(&state.pool)
        .await
        .unwrap();
}

async fn rate_limited_until(state: &AppState) -> Option<i64> {
    sqlx::query_scalar("SELECT rate_limited_until_ms FROM app_state WHERE id = 1")
        .fetch_one(&state.pool)
        .await
        .unwrap()
}

/// The rate limit page: 503, `Retry-After`, the message, and nothing that
/// sends the browser anywhere (so it cannot loop).
fn assert_rate_limit_page(
    status: StatusCode,
    headers: &axum::http::HeaderMap,
    html: &str,
    message: &str,
    retry_after: &str,
) {
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{html}");
    assert_eq!(headers["retry-after"], retry_after);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(html.contains(&format!("<h1>{message}</h1>")), "{html}");
    assert!(headers.get("location").is_none());
    assert!(headers.get("refresh").is_none());
    assert!(headers.get("set-cookie").is_none());
    for way_back in ["http-equiv", "<script", "href", "/auth/"] {
        assert!(!html.contains(way_back), "{way_back} in {html}");
    }
}

/// The same as JSON: 503, `Retry-After` and `{"error": message}`.
fn assert_rate_limit_json(
    status: StatusCode,
    headers: &axum::http::HeaderMap,
    body: &str,
    message: &str,
    retry_after: &str,
) {
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(headers["retry-after"], retry_after);
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body, json!({ "error": message }));
    assert!(headers.get("location").is_none());
}

#[tokio::test]
async fn login_shows_until_when_spotify_limits_the_app_without_calling_it() {
    let (state, server) = setup_at_now().await;
    // 08:10:30 UTC: shown as 08:11, never earlier than the real end.
    block_app(&state, NOW + 630_000).await;
    let message = "Spotify limite l’app jusqu’à 08:11 UTC";

    let (status, headers, html) = raw(&state, req_with("/auth/login", None, &[HTML])).await;
    assert_rate_limit_page(status, &headers, &html, message, "630");
    let (status, headers, body) = raw(&state, req_with("/auth/login", None, &[JSON])).await;
    assert_rate_limit_json(status, &headers, &body, message, "630");
    // A desktop app's login is refused the same way.
    let uri = login_uri(&[("app_redirect", "http://127.0.0.1:4321/cb")]);
    let (status, headers, html) = raw(&state, req_with(&uri, None, &[HTML])).await;
    assert_rate_limit_page(status, &headers, &html, message, "630");

    state.clock.set_ms(NOW + 629_001);
    let (status, headers, _) = raw(&state, req_with("/auth/login", None, &[JSON])).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "1");
    assert!(server.received_requests().await.unwrap().is_empty());

    // Once the block is over, the login goes to Spotify again.
    state.clock.set_ms(NOW + 630_000);
    login(&state).await;
}

#[tokio::test]
async fn profile_429_blocks_the_app_and_the_callback_shows_until_when() {
    let (state, server) = setup_at_now().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-1", "token_type": "Bearer", "expires_in": 3600,
            "refresh_token": "r",
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/me"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "120"))
        .expect(1)
        .mount(&server)
        .await;
    let message = "Spotify limite l’app jusqu’à 08:02 UTC";
    let (st, cookie) = login(&state).await;
    let callback = format!("/auth/callback?code=the-code&state={st}");

    let (status, headers, html) = raw(&state, req_with(&callback, Some(&cookie), &[HTML])).await;
    assert_rate_limit_page(status, &headers, &html, message, "120");
    assert_eq!(rate_limited_until(&state).await, Some(NOW + 120_000));
    assert_eq!(user_count(&state).await, 0);

    // During the block, neither the same callback again nor a new login calls
    // Spotify (expect(1) on both mocks, verified on drop).
    let (status, headers, body) = raw(&state, req_with(&callback, Some(&cookie), &[JSON])).await;
    assert_rate_limit_json(status, &headers, &body, message, "120");
    let (status, headers, html) = raw(&state, req_with("/auth/login", None, &[HTML])).await;
    assert_rate_limit_page(status, &headers, &html, message, "120");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn token_exchange_429_is_sent_to_the_desktop_app() {
    let (state, server) = setup_at_now().await;
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "60"))
        .expect(1)
        .mount(&server)
        .await;
    let uri = login_uri(&[
        ("app_redirect", "http://127.0.0.1:4321/cb"),
        ("app_state", "s1"),
    ]);
    let (st, cookie) = login_with(&state, &uri).await;

    let (status, headers, _) = raw(
        &state,
        req_with(
            &format!("/auth/callback?code=the-code&state={st}"),
            Some(&cookie),
            &[HTML],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FOUND);
    assert_cookie_cleared(&headers);
    let (url, q) = location(&headers);
    assert_eq!(url.port(), Some(4321));
    assert_eq!(q["error"], "Spotify limite l’app jusqu’à 08:01 UTC");
    assert_eq!(q["state"], "s1");
    assert!(!q.contains_key("token"));
    assert_eq!(rate_limited_until(&state).await, Some(NOW + 60_000));
}
