//! `/auth/login` and `/auth/callback`: the Spotify authorization code flow.
//!
//! The anti-CSRF `state` is a random nonce stored in a short-lived, HttpOnly
//! cookie signed with HMAC-SHA256 (keyed with the Spotify client secret), so the
//! callback only accepts a state issued to the same browser.
//!
//! Desktop apps can use the RFC 8252 loopback pattern: `/auth/login` accepts an
//! `app_redirect` (a loopback `http` URL with an explicit port) and an opaque
//! `app_state`. Both travel inside the signed cookie, and the callback then
//! redirects to `app_redirect` with `token` (or `error`) and `state` instead of
//! rendering the token page.
//!
//! While Spotify rate limits the app (see [`super::rate_limit`]), neither makes
//! any Spotify call: they answer 503 with `Retry-After` and « Spotify limite
//! l’app jusqu’à HH:MM UTC » (an HTML page for a browser, JSON otherwise), and
//! nothing sends the browser back to Spotify, so it cannot loop. A 429 during
//! the login itself (token exchange or profile) starts such a block.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::header::{
    ACCEPT, CACHE_CONTROL, CONTENT_TYPE, COOKIE, LOCATION, RETRY_AFTER, SET_COOKIE,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::client::{self, ClientError, SCOPES};
use super::rate_limit;
use crate::AppState;
use crate::auth::issue_token;
use crate::config::Config;
use crate::error::{AppError, AppResult};

const COOKIE_NAME: &str = "spotify_oauth_state";
const STATE_TTL_SECS: i64 = 600;
/// Longest accepted `app_redirect` once normalized (it is hex-encoded in the cookie).
const APP_REDIRECT_MAX_LEN: usize = 512;
const APP_STATE_MAX_LEN: usize = 128;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

/// HMAC of the cookie's message (everything before its last `.`).
fn sign(state: &AppState, msg: &str) -> String {
    hex::encode(hmac_sha256(
        state.config.spotify_client_secret.as_bytes(),
        msg.as_bytes(),
    ))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn now_secs(state: &AppState) -> i64 {
    state.clock.now_ms().div_euclid(1000)
}

fn cookie_attrs(state: &AppState) -> &'static str {
    if state.config.spotify_redirect_uri.starts_with("https://") {
        "; Path=/auth; HttpOnly; SameSite=Lax; Secure"
    } else {
        "; Path=/auth; HttpOnly; SameSite=Lax"
    }
}

fn header(v: &str) -> AppResult<HeaderValue> {
    HeaderValue::from_str(v).map_err(|e| AppError::Internal(format!("invalid header: {e}")))
}

// ---- desktop app return (RFC 8252 loopback redirect) -----------------------

/// Where a desktop app wants the login result, carried in the signed cookie.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AppTarget {
    /// A loopback URL, as validated and normalized by [`parse_app_redirect`].
    redirect: String,
    /// Echoed back as `state`; `None` when the app sent none.
    state: Option<String>,
}

/// Accept only `http://{127.0.0.1|[::1]|localhost}:<port>/<path>[?query]`,
/// without userinfo or fragment: any other target could hand the token to a
/// third party.
fn parse_app_redirect(raw: &str) -> AppResult<reqwest::Url> {
    let bad = || AppError::BadRequest("app_redirect must be a loopback http URL".into());
    let url = reqwest::Url::parse(raw).map_err(|_| bad())?;
    // The parser normalizes the host (lowercase, canonical IPv4 and IPv6) and
    // drops a default port, so `:80` counts as no port; loopback listeners use
    // an ephemeral one anyway.
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    let ok = url.scheme() == "http"
        && loopback
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.as_str().len() <= APP_REDIRECT_MAX_LEN;
    if ok { Ok(url) } else { Err(bad()) }
}

fn check_app_state(s: &str) -> AppResult<()> {
    let ok = s.len() <= APP_STATE_MAX_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "app_state must be at most {APP_STATE_MAX_LEN} characters of [A-Za-z0-9_-]"
        )))
    }
}

/// Validate the optional desktop app parameters (empty values count as absent).
/// `app_state` is only kept along with an `app_redirect`.
fn app_target(params: LoginParams) -> AppResult<Option<AppTarget>> {
    let redirect = params
        .app_redirect
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(parse_app_redirect)
        .transpose()?;
    let state = params.app_state.filter(|s| !s.is_empty());
    if let Some(s) = &state {
        check_app_state(s)?;
    }
    Ok(redirect.map(|url| AppTarget {
        redirect: url.as_str().to_owned(),
        state,
    }))
}

/// The cookie's signed message: `nonce.expires` (the original format, still
/// used for browser logins), or `nonce.expires.<hex app_redirect>.<app_state>`.
fn state_message(nonce: &str, expires: i64, app: Option<&AppTarget>) -> String {
    match app {
        None => format!("{nonce}.{expires}"),
        Some(t) => format!(
            "{nonce}.{expires}.{}.{}",
            hex::encode(&t.redirect),
            t.state.as_deref().unwrap_or("")
        ),
    }
}

/// `302` to the desktop app's loopback URL, with `key=value` then `state`
/// appended to its query.
fn redirect_to_app(app: &AppTarget, key: &str, value: &str) -> AppResult<Response> {
    // Checked again even though the cookie is signed: this is where the token goes.
    let mut url = parse_app_redirect(&app.redirect)?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair(key, value);
        if let Some(s) = &app.state {
            query.append_pair("state", s);
        }
    }
    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut().insert(LOCATION, header(url.as_str())?);
    Ok(resp)
}

// ---- login ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginParams {
    app_redirect: Option<String>,
    app_state: Option<String>,
}

/// `GET /auth/login[?app_redirect&app_state]`: redirect to Spotify's authorize page.
pub async fn login(
    State(state): State<AppState>,
    parts: Parts,
    query: Result<Query<LoginParams>, QueryRejection>,
) -> AppResult<Response> {
    check_rate_limit(&state, &parts)?;
    let Query(params) = query.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let app = app_target(params)?;

    // The login would end in a Spotify call: not before the block ends.
    let now_ms = state.clock.now_ms();
    if let Some(until_ms) = rate_limit::blocked_until(&state.pool, now_ms).await? {
        tracing::info!(
            stage = "login",
            "spotify login refused: Spotify limits the app"
        );
        return Ok(error_response(
            &parts.headers,
            rate_limit::blocked_error(until_ms, now_ms),
        ));
    }

    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let nonce = hex::encode(bytes);
    let expires = now_secs(&state) + STATE_TTL_SECS;
    let msg = state_message(&nonce, expires, app.as_ref());
    let cookie = format!(
        "{COOKIE_NAME}={msg}.{}; Max-Age={STATE_TTL_SECS}{}",
        sign(&state, &msg),
        cookie_attrs(&state)
    );

    let base = format!("{}/authorize", state.config.spotify_accounts_base);
    let url = reqwest::Url::parse_with_params(
        &base,
        &[
            ("response_type", "code"),
            ("client_id", state.config.spotify_client_id.as_str()),
            ("redirect_uri", state.config.spotify_redirect_uri.as_str()),
            ("scope", SCOPES),
            ("state", nonce.as_str()),
        ],
    )
    .map_err(|e| AppError::Internal(format!("invalid Spotify accounts base: {e}")))?;

    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut().insert(LOCATION, header(url.as_str())?);
    resp.headers_mut().insert(SET_COOKIE, header(&cookie)?);
    Ok(resp)
}

// ---- callback -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// What a verified state cookie carries.
struct VerifiedState {
    app: Option<AppTarget>,
}

/// Verify the `state` query parameter against the signed cookie.
fn verify_state(app: &AppState, headers: &HeaderMap, state_param: &str) -> Option<VerifiedState> {
    let prefix = format!("{COOKIE_NAME}=");
    let value = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|p| p.trim().strip_prefix(prefix.as_str()))?;
    let (msg, sig) = value.rsplit_once('.')?;
    if !constant_time_eq(sign(app, msg).as_bytes(), sig.as_bytes()) {
        return None;
    }
    let mut fields = msg.split('.');
    let nonce = fields.next()?;
    let expires: i64 = fields.next()?.parse().ok()?;
    let target = match (fields.next(), fields.next(), fields.next()) {
        (None, None, None) => None,
        (Some(redirect_hex), Some(app_state), None) => Some(AppTarget {
            redirect: String::from_utf8(hex::decode(redirect_hex).ok()?).ok()?,
            state: (!app_state.is_empty()).then(|| app_state.to_owned()),
        }),
        _ => return None,
    };
    (expires >= now_secs(app) && constant_time_eq(nonce.as_bytes(), state_param.as_bytes()))
        .then_some(VerifiedState { app: target })
}

/// Log a callback failure once, at warn, with its stage and cause.
/// Never pass a code, state, token, secret, cookie or response body here.
fn log_failure(stage: &str, cause: &dyn std::fmt::Display) {
    tracing::warn!(stage, %cause, "spotify login failed");
}

/// Log a callback failure and return the error to show for it.
fn fail(stage: &str, cause: &dyn std::fmt::Display, err: AppError) -> AppError {
    log_failure(stage, cause);
    err
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.to_ascii_lowercase().contains("text/html"))
}

fn token_page(display_name: &str, token: &str) -> String {
    let name = html_escape(display_name);
    let token = html_escape(token);
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Your API token</title>\
<style>body{{font-family:system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem}}\
code,pre{{background:#eee;padding:.2rem .4rem;border-radius:4px;word-break:break-all;white-space:pre-wrap}}\
.warn{{border:2px solid #c00;padding:.75rem;border-radius:6px;background:#fee}}</style></head>\
<body><h1>Welcome, {name}</h1>\
<p>Your API token:</p><p><code id=\"token\">{token}</code></p>\
<p class=\"warn\"><strong>This token is shown only once.</strong> Copy it now: it cannot be displayed again.</p>\
<p>Try it:</p><pre>curl -H \"Authorization: Bearer {token}\" https://YOUR-SERVER/stats/summary</pre>\
</body></html>\n"
    )
}

/// The page shown while Spotify rate limits the app. Nothing on it sends the
/// browser anywhere (no redirect, no refresh, no script): it cannot loop.
fn rate_limited_page(message: &str) -> String {
    let message = html_escape(message);
    format!(
        "<!doctype html>\n<html lang=\"fr\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Spotify limite l’app</title>\
<style>body{{font-family:system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem}}</style>\
</head><body><h1>{message}</h1>\
<p>Spotify refuse pour le moment les appels de cette app (HTTP 429), pour tous ses \
utilisateurs : la connexion ne peut pas aboutir avant cette heure. Recommencez-la ensuite.</p>\
</body></html>\n"
    )
}

/// An error answer of the login: [`rate_limited_page`] for a browser while
/// Spotify rate limits the app, the JSON error otherwise.
fn error_response(headers: &HeaderMap, err: AppError) -> Response {
    match err {
        AppError::SpotifyRateLimited {
            message,
            retry_after_secs,
        } if wants_html(headers) => {
            let mut resp = (
                StatusCode::SERVICE_UNAVAILABLE,
                [(CONTENT_TYPE, "text/html; charset=utf-8")],
                rate_limited_page(&message),
            )
                .into_response();
            resp.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(retry_after_secs));
            resp
        }
        other => other.into_response(),
    }
}

/// A finished login.
struct LoggedIn {
    token: String,
    user_id: i64,
    spotify_id: String,
    display_name: String,
}

/// The token page (HTML for a browser, JSON otherwise).
fn token_response(headers: &HeaderMap, login: &LoggedIn) -> Response {
    if wants_html(headers) {
        (
            [(CONTENT_TYPE, "text/html; charset=utf-8")],
            token_page(&login.display_name, &login.token),
        )
            .into_response()
    } else {
        Json(json!({
            "token": login.token,
            "user": {
                "id": login.user_id,
                "spotify_id": login.spotify_id,
                "display_name": login.display_name,
            },
        }))
        .into_response()
    }
}

/// `GET /auth/callback?code&state`: finish the login and return an API token,
/// or hand it (or the error) to the desktop app's `app_redirect`.
pub async fn callback(
    State(state): State<AppState>,
    parts: Parts,
    Query(params): Query<CallbackParams>,
) -> Response {
    if let Err(e) = check_rate_limit(&state, &parts) {
        return e.into_response();
    }
    let headers = &parts.headers;

    let Some(state_param) = params.state.as_deref() else {
        return fail(
            "state check",
            &"missing state parameter",
            AppError::BadRequest("missing state".into()),
        )
        .into_response();
    };
    // Without a verified cookie there is no trusted `app_redirect` either, so
    // these failures always get the plain error response.
    let Some(verified) = verify_state(&state, headers, state_param) else {
        return fail(
            "state check",
            &"state does not match the cookie, or expired",
            AppError::BadRequest("invalid or expired state".into()),
        )
        .into_response();
    };

    let outcome = finish_login(&state, params.error, params.code).await;
    let resp = match (verified.app, outcome) {
        (Some(app), Ok(login)) => redirect_to_app(&app, "token", &login.token),
        (Some(app), Err(e)) => redirect_to_app(&app, "error", &e.public_message()),
        (None, Ok(login)) => Ok(token_response(headers, &login)),
        (None, Err(e)) => return error_response(headers, e),
    };
    resp.and_then(|r| no_store_and_clear_cookie(&state, r))
        .unwrap_or_else(IntoResponse::into_response)
}

/// The responses that end a login carry the token: never cache them, and
/// retire the state cookie.
fn no_store_and_clear_cookie(state: &AppState, mut resp: Response) -> AppResult<Response> {
    let clear = format!("{COOKIE_NAME}=; Max-Age=0{}", cookie_attrs(state));
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut().insert(SET_COOKIE, header(&clear)?);
    Ok(resp)
}

/// Exchange the code, save the user and issue an API token. Failures are
/// logged here and come back as the error to show.
async fn finish_login(
    state: &AppState,
    error: Option<String>,
    code: Option<String>,
) -> AppResult<LoggedIn> {
    if let Some(err) = error {
        // `error` is chosen by the caller: keep it short in the logs.
        let short: String = err.chars().take(64).collect();
        return Err(fail(
            "spotify denied",
            &short,
            AppError::BadRequest(format!("Spotify denied access: {short}")),
        ));
    }
    let Some(code) = code.filter(|c| !c.is_empty()) else {
        return Err(fail(
            "state check",
            &"missing code parameter",
            AppError::BadRequest("missing code".into()),
        ));
    };

    let db_fail = |e: sqlx::Error| {
        let cause = e.to_string();
        fail(
            "database",
            &cause,
            AppError::BadGateway("could not save your login, please try again".into()),
        )
    };

    // No Spotify call at all while Spotify rate limits the app.
    let now_ms = state.clock.now_ms();
    if let Some(until_ms) = rate_limit::blocked_until(&state.pool, now_ms)
        .await
        .map_err(db_fail)?
    {
        return Err(fail(
            "rate limit",
            &"Spotify rate limits the app, no call made",
            rate_limit::blocked_error(until_ms, now_ms),
        ));
    }

    let tokens = match client::exchange_code(state, &code).await {
        Ok(tokens) => tokens,
        Err(e) => {
            let other = AppError::BadGateway(format!(
                "Spotify refused the authorization code (token exchange failed: {e}). \
                 Try logging in again."
            ));
            let shown = call_error(state, &e, other).await;
            return Err(fail("token exchange", &e, shown));
        }
    };
    let Some(refresh) = tokens.refresh_token else {
        return Err(fail(
            "token exchange",
            &"Spotify returned no refresh token",
            AppError::BadGateway("Spotify returned no refresh token".into()),
        ));
    };
    let profile = match client::get_profile(state, &tokens.access_token).await {
        Ok(profile) => profile,
        Err(e) => {
            let other = AppError::BadGateway(format!(
                "Spotify refused the profile request ({e}). If your Spotify account \
                 is not the app owner's, it may need to be added to the app's user \
                 list in the Spotify developer dashboard."
            ));
            let shown = call_error(state, &e, other).await;
            return Err(fail("profile", &e, shown));
        }
    };

    if let Some(allowed) = &state.config.allowed_spotify_ids
        && !allowed.iter().any(|id| id == &profile.id)
    {
        return Err(fail(
            "not allowed",
            &format!("spotify id {} is not in ALLOWED_SPOTIFY_IDS", profile.id),
            AppError::Forbidden("this Spotify account is not allowed on this server".into()),
        ));
    }

    let display_name = profile
        .display_name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| profile.id.clone());
    let expires_at = now_secs(state) + tokens.expires_in;

    // A (re-)login also clears the poller backoff so polling resumes promptly.
    let (user_id,): (i64,) = sqlx::query_as(
        "INSERT INTO users (spotify_id, display_name, access_token, refresh_token, token_expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(spotify_id) DO UPDATE SET
             display_name = excluded.display_name,
             access_token = excluded.access_token,
             refresh_token = excluded.refresh_token,
             token_expires_at = excluded.token_expires_at,
             next_poll_at_ms = NULL,
             last_error = NULL,
             last_error_at_ms = NULL
         RETURNING id",
    )
    .bind(&profile.id)
    .bind(&display_name)
    .bind(&tokens.access_token)
    .bind(&refresh)
    .bind(expires_at)
    .fetch_one(&state.pool)
    .await
    .map_err(db_fail)?;

    let token = issue_token(&state.pool, user_id).await.map_err(db_fail)?;
    Ok(LoggedIn {
        token,
        user_id,
        spotify_id: profile.id,
        display_name,
    })
}

/// The error to show for a failed Spotify call of the login: a 429 blocks the
/// whole app (stored, see [`rate_limit::record_429`]) and says until when;
/// anything else is `other`.
async fn call_error(state: &AppState, e: &ClientError, other: AppError) -> AppError {
    match e {
        ClientError::RateLimited { retry_after } => {
            let at_ms = state.clock.now_ms();
            let until_ms = rate_limit::record_429(&state.pool, *retry_after, at_ms).await;
            rate_limit::blocked_error(until_ms, at_ms)
        }
        _ => other,
    }
}

// ---- per-IP rate limit ----------------------------------------------------

const WINDOW: Duration = Duration::from_secs(60);
/// Hard cap on tracked IPs; beyond it the table is reset (fails open, stays bounded).
const MAX_TRACKED: usize = 10_000;

struct Limiter {
    config: Weak<Config>,
    hits: HashMap<String, (Instant, u32)>,
}

/// One limiter per `Config` instance (i.e. per app state). `AppState` is not
/// ours to extend, so the table lives here, keyed by the config's address; the
/// `Weak` keeps that address from being reused while the entry exists.
fn limiters() -> &'static Mutex<HashMap<usize, Limiter>> {
    static L: OnceLock<Mutex<HashMap<usize, Limiter>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

fn client_key(state: &AppState, parts: &Parts) -> String {
    if state.config.trust_x_forwarded_for
        && let Some(ip) = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
    {
        return ip.to_string();
    }
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "shared".to_string())
}

fn check_rate_limit(state: &AppState, parts: &Parts) -> AppResult<()> {
    let key = client_key(state, parts);
    let now = Instant::now();
    let mut all = limiters().lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|_, l| l.config.strong_count() > 0);
    let id = Arc::as_ptr(&state.config) as usize;
    let lim = all.entry(id).or_insert_with(|| Limiter {
        config: Arc::downgrade(&state.config),
        hits: HashMap::new(),
    });
    lim.hits
        .retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
    if lim.hits.len() >= MAX_TRACKED && !lim.hits.contains_key(&key) {
        lim.hits.clear();
    }
    let entry = lim.hits.entry(key).or_insert((now, 0));
    if entry.1 >= state.config.auth_rate_limit_per_min {
        let left = WINDOW.saturating_sub(now.duration_since(entry.0));
        let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
        tracing::warn!(stage = "rate limit", "auth rate limit exceeded");
        return Err(AppError::TooManyRequests(secs.max(1)));
    }
    entry.1 += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn app_redirect_accepts_loopback_http_with_a_port() {
        for (raw, normalized) in [
            (
                "http://127.0.0.1:4321/callback",
                "http://127.0.0.1:4321/callback",
            ),
            ("http://[::1]:4321/cb", "http://[::1]:4321/cb"),
            ("http://[0:0:0:0:0:0:0:1]:4321/cb", "http://[::1]:4321/cb"),
            (
                "http://localhost:4321/cb?x=1",
                "http://localhost:4321/cb?x=1",
            ),
            ("http://LocalHost:4321", "http://localhost:4321/"),
        ] {
            let url = parse_app_redirect(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(url.as_str(), normalized);
        }
    }

    #[test]
    fn app_redirect_refuses_everything_else() {
        let long = format!("http://127.0.0.1:4321/{}", "a".repeat(APP_REDIRECT_MAX_LEN));
        for raw in [
            "https://127.0.0.1:4321/cb",
            "ftp://127.0.0.1:4321/cb",
            "http://127.0.0.1/cb",
            // A default port is indistinguishable from none once parsed.
            "http://127.0.0.1:80/cb",
            "http://127.0.0.2:4321/cb",
            "http://example.com:4321/cb",
            "http://localhost.example.com:4321/cb",
            "http://127.0.0.1:4321@example.com/cb",
            "http://user@127.0.0.1:4321/cb",
            "http://user:pw@127.0.0.1:4321/cb",
            "http://127.0.0.1:4321/cb#frag",
            "http://127.0.0.1:4321/cb#",
            "javascript:alert(1)",
            "/auth/callback",
            "",
            long.as_str(),
        ] {
            let err = parse_app_redirect(raw).expect_err(raw);
            assert_eq!(err.to_string(), "app_redirect must be a loopback http URL");
        }
    }

    #[test]
    fn app_state_is_short_and_url_safe() {
        assert!(check_app_state("abc_DEF-0123").is_ok());
        assert!(check_app_state(&"a".repeat(APP_STATE_MAX_LEN)).is_ok());
        for bad in [
            "a".repeat(APP_STATE_MAX_LEN + 1),
            "with space".into(),
            "dot.dot".into(),
            "semi;colon".into(),
            "é".into(),
        ] {
            assert!(check_app_state(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn app_redirect_appends_to_the_existing_query() {
        let app = AppTarget {
            redirect: "http://127.0.0.1:4321/cb?x=1".into(),
            state: Some("s-1".into()),
        };
        let resp = redirect_to_app(&app, "error", "a b&c").unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers()[LOCATION],
            "http://127.0.0.1:4321/cb?x=1&error=a+b%26c&state=s-1"
        );

        let app = AppTarget {
            redirect: "http://[::1]:4321/cb".into(),
            state: None,
        };
        let resp = redirect_to_app(&app, "token", "t").unwrap();
        assert_eq!(resp.headers()[LOCATION], "http://[::1]:4321/cb?token=t");
    }

    #[test]
    fn cookie_message_keeps_the_original_format_without_an_app() {
        assert_eq!(state_message("ab", 7, None), "ab.7");
        let app = AppTarget {
            redirect: "http://localhost:1/".into(),
            state: None,
        };
        assert_eq!(
            state_message("ab", 7, Some(&app)),
            format!("ab.7.{}.", hex::encode("http://localhost:1/"))
        );
    }
}
