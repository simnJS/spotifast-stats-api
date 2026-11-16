//! `/auth/login` and `/auth/callback`: the Spotify authorization code flow.
//!
//! The anti-CSRF `state` is a random nonce stored in a short-lived, HttpOnly
//! cookie signed with HMAC-SHA256 (keyed with the Spotify client secret), so the
//! callback only accepts a state issued to the same browser.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::client::{self, SCOPES};
use crate::AppState;
use crate::auth::issue_token;
use crate::config::Config;
use crate::error::{AppError, AppResult};

const COOKIE_NAME: &str = "spotify_oauth_state";
const STATE_TTL_SECS: i64 = 600;

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

fn sign(state: &AppState, nonce: &str, expires: i64) -> String {
    hex::encode(hmac_sha256(
        state.config.spotify_client_secret.as_bytes(),
        format!("{nonce}.{expires}").as_bytes(),
    ))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
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

/// `GET /auth/login`: redirect to Spotify's authorize page.
pub async fn login(State(state): State<AppState>, parts: Parts) -> AppResult<Response> {
    check_rate_limit(&state, &parts)?;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let nonce = hex::encode(bytes);
    let expires = now_secs() + STATE_TTL_SECS;
    let cookie = format!(
        "{COOKIE_NAME}={nonce}.{expires}.{}; Max-Age={STATE_TTL_SECS}{}",
        sign(&state, &nonce, expires),
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

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Verify the `state` query parameter against the signed cookie.
fn verify_state(app: &AppState, headers: &HeaderMap, state_param: &str) -> bool {
    let prefix = format!("{COOKIE_NAME}=");
    let cookie_value = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|p| p.trim().strip_prefix(prefix.as_str()));
    let Some(value) = cookie_value else {
        return false;
    };
    let mut parts = value.split('.');
    let (Some(nonce), Some(expires), Some(sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Ok(expires_n) = expires.parse::<i64>() else {
        return false;
    };
    constant_time_eq(sign(app, nonce, expires_n).as_bytes(), sig.as_bytes())
        && expires_n >= now_secs()
        && constant_time_eq(nonce.as_bytes(), state_param.as_bytes())
}

/// Log a callback failure once, at warn, with its stage and cause.
/// Never pass a code, state, token, secret, cookie or response body here.
fn log_failure(stage: &str, cause: &dyn std::fmt::Display) {
    tracing::warn!(stage, %cause, "spotify login failed");
}

fn fail(stage: &str, cause: &dyn std::fmt::Display, err: AppError) -> Response {
    log_failure(stage, cause);
    err.into_response()
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

/// `GET /auth/callback?code&state`: finish the login and return an API token.
// Failures are already-logged, ready-made responses; boxing them would only add noise.
#[allow(clippy::result_large_err)]
pub async fn callback(
    State(state): State<AppState>,
    parts: Parts,
    Query(params): Query<CallbackParams>,
) -> Result<Response, Response> {
    check_rate_limit(&state, &parts).map_err(IntoResponse::into_response)?;
    let headers = &parts.headers;

    let Some(state_param) = params.state.as_deref() else {
        return Err(fail(
            "state check",
            &"missing state parameter",
            AppError::BadRequest("missing state".into()),
        ));
    };
    if !verify_state(&state, headers, state_param) {
        return Err(fail(
            "state check",
            &"state does not match the cookie, or expired",
            AppError::BadRequest("invalid or expired state".into()),
        ));
    }
    if let Some(err) = params.error {
        // `error` is chosen by the caller: keep it short in the logs.
        let short: String = err.chars().take(64).collect();
        return Err(fail(
            "spotify denied",
            &short,
            AppError::BadRequest(format!("Spotify denied access: {short}")),
        ));
    }
    let Some(code) = params.code.filter(|c| !c.is_empty()) else {
        return Err(fail(
            "state check",
            &"missing code parameter",
            AppError::BadRequest("missing code".into()),
        ));
    };

    let tokens = client::exchange_code(&state, &code).await.map_err(|e| {
        fail(
            "token exchange",
            &e,
            AppError::BadGateway(format!(
                "Spotify refused the authorization code (token exchange failed: {e}). \
                 Try logging in again."
            )),
        )
    })?;
    let Some(refresh) = tokens.refresh_token else {
        return Err(fail(
            "token exchange",
            &"Spotify returned no refresh token",
            AppError::BadGateway("Spotify returned no refresh token".into()),
        ));
    };
    let profile = client::get_profile(&state, &tokens.access_token)
        .await
        .map_err(|e| {
            fail(
                "profile",
                &e,
                AppError::BadGateway(format!(
                    "Spotify refused the profile request ({e}). If your Spotify account \
                     is not the app owner's, it may need to be added to the app's user \
                     list in the Spotify developer dashboard."
                )),
            )
        })?;

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
    let expires_at = now_secs() + tokens.expires_in;

    let db_fail = |e: sqlx::Error| {
        let cause = e.to_string();
        fail(
            "database",
            &cause,
            AppError::BadGateway("could not save your login, please try again".into()),
        )
    };

    // A (re-)login also clears the poller backoff so polling resumes promptly;
    // `rate_limited_until_ms` is Spotify's quota and is left alone.
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

    let clear = format!("{COOKIE_NAME}=; Max-Age=0{}", cookie_attrs(&state));
    let mut resp = if wants_html(headers) {
        (
            [(CONTENT_TYPE, "text/html; charset=utf-8")],
            token_page(&display_name, &token),
        )
            .into_response()
    } else {
        Json(json!({
            "token": token,
            "user": { "id": user_id, "spotify_id": profile.id, "display_name": display_name },
        }))
        .into_response()
    };
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut().insert(
        SET_COOKIE,
        header(&clear).map_err(IntoResponse::into_response)?,
    );
    Ok(resp)
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
}
