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
    issue_token_with_id(pool, user_id).await.map(|(_, t)| t)
}

/// Like [`issue_token`], also returning the new token's row id.
pub async fn issue_token_with_id(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<(i64, String), sqlx::Error> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let res = sqlx::query("INSERT INTO api_tokens (user_id, token_hash) VALUES (?1, ?2)")
        .bind(user_id)
        .bind(hash_token(&token))
        .execute(pool)
        .await?;
    Ok((res.last_insert_rowid(), token))
}

/// Minimum delay between two `last_used_at_ms` writes for the same token.
const LAST_USED_REFRESH_MS: i64 = 60_000;

/// Authenticated user, extracted from `Authorization: Bearer <token>`.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub user_id: i64,
    /// Row id of the token that authenticated this request.
    pub token_id: i64,
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
        let row: Option<(i64, i64)> =
            sqlx::query_as("SELECT id, user_id FROM api_tokens WHERE token_hash = ?1")
                .bind(hash_token(token))
                .fetch_optional(&state.pool)
                .await?;
        let (token_id, user_id) = row.ok_or_else(unauthorized)?;
        // Conditional write: a no-op unless the stored value is older than a minute.
        let now = chrono::Utc::now().timestamp_millis();
        if let Err(e) = sqlx::query(
            "UPDATE api_tokens SET last_used_at_ms = ?1 \
             WHERE id = ?2 AND (last_used_at_ms IS NULL OR last_used_at_ms <= ?3)",
        )
        .bind(now)
        .bind(token_id)
        .bind(now - LAST_USED_REFRESH_MS)
        .execute(&state.pool)
        .await
        {
            tracing::warn!(error = %e, "could not update token last_used_at_ms");
        }
        Ok(AuthUser { user_id, token_id })
    }
}
