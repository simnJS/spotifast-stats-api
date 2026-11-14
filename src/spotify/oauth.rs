//! `/auth/login` and `/auth/callback`: the Spotify authorization code flow.
//!
//! The anti-CSRF `state` is a random nonce stored in a short-lived, HttpOnly
//! cookie signed with HMAC-SHA256 (keyed with the Spotify client secret), so the
//! callback only accepts a state issued to the same browser.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::header::{COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::client::{self, ClientError, SCOPES};
use crate::AppState;
use crate::auth::issue_token;
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
pub async fn login(State(state): State<AppState>) -> AppResult<Response> {
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

fn bad_gateway(e: ClientError) -> AppError {
    AppError::BadGateway(format!("Spotify error: {e}"))
}

/// `GET /auth/callback?code&state`: finish the login and return an API token.
pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<CallbackParams>,
) -> AppResult<Response> {
    let Some(state_param) = params.state.as_deref() else {
        return Err(AppError::BadRequest("missing state".into()));
    };
    if !verify_state(&state, &headers, state_param) {
        return Err(AppError::BadRequest("invalid or expired state".into()));
    }
    if let Some(err) = params.error {
        return Err(AppError::BadRequest(format!(
            "Spotify denied access: {err}"
        )));
    }
    let code = params
        .code
        .filter(|c| !c.is_empty())
        .ok_or_else(|| AppError::BadRequest("missing code".into()))?;

    let tokens = client::exchange_code(&state, &code)
        .await
        .map_err(bad_gateway)?;
    let refresh = tokens
        .refresh_token
        .ok_or_else(|| AppError::BadGateway("Spotify returned no refresh token".into()))?;
    let profile = client::get_profile(&state, &tokens.access_token)
        .await
        .map_err(bad_gateway)?;
    let display_name = profile
        .display_name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| profile.id.clone());
    let expires_at = now_secs() + tokens.expires_in;

    let (user_id,): (i64,) = sqlx::query_as(
        "INSERT INTO users (spotify_id, display_name, access_token, refresh_token, token_expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(spotify_id) DO UPDATE SET
             display_name = excluded.display_name,
             access_token = excluded.access_token,
             refresh_token = excluded.refresh_token,
             token_expires_at = excluded.token_expires_at
         RETURNING id",
    )
    .bind(&profile.id)
    .bind(&display_name)
    .bind(&tokens.access_token)
    .bind(&refresh)
    .bind(expires_at)
    .fetch_one(&state.pool)
    .await?;

    let token = issue_token(&state.pool, user_id).await?;

    let clear = format!("{COOKIE_NAME}=; Max-Age=0{}", cookie_attrs(&state));
    let mut resp = Json(json!({
        "token": token,
        "user": { "id": user_id, "spotify_id": profile.id, "display_name": display_name },
    }))
    .into_response();
    resp.headers_mut().insert(SET_COOKIE, header(&clear)?);
    Ok(resp)
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
