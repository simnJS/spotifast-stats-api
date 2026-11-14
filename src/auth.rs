//! Bearer-token authentication.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::AppState;
use crate::error::AppError;

/// SHA-256 hex digest of a raw token.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Create a new API token for `user_id`. Returns the raw token (hex, 64 chars);
/// only its SHA-256 is stored.
pub async fn issue_token(pool: &SqlitePool, user_id: i64) -> Result<String, sqlx::Error> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    sqlx::query("INSERT INTO api_tokens (user_id, token_hash) VALUES (?1, ?2)")
        .bind(user_id)
        .bind(hash_token(&token))
        .execute(pool)
        .await?;
    Ok(token)
}

/// Authenticated user, extracted from `Authorization: Bearer <token>`.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub user_id: i64,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let unauthorized = || AppError::Unauthorized("missing or invalid bearer token".into());
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(unauthorized)?;
        let (scheme, token) = header.split_once(' ').ok_or_else(unauthorized)?;
        let token = token.trim();
        if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
            return Err(unauthorized());
        }
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT user_id FROM api_tokens WHERE token_hash = ?1")
                .bind(hash_token(token))
                .fetch_optional(&state.pool)
                .await?;
        row.map(|(user_id,)| AuthUser { user_id })
            .ok_or_else(unauthorized)
    }
}
