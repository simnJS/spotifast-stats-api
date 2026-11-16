//! Account endpoints: poller status, API token management and account deletion.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::{self, AuthUser};
use crate::error::{AppError, AppResult};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/me/status", get(status))
        .route("/auth/tokens", get(list_tokens))
        .route("/auth/token", post(create_token))
        .route("/auth/token/{id}", delete(revoke_token))
}

type StatusRow = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

/// `GET /me/status`: why is (or is not) my listening being recorded.
async fn status(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let row: StatusRow = sqlx::query_as(
        "SELECT last_poll_ok_at_ms, next_poll_at_ms, poll_interval_secs, rate_limited_until_ms, \
         last_error, last_error_at_ms, last_polled_at_ms FROM users WHERE id = ?1",
    )
    .bind(user.user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::Unauthorized("missing or invalid bearer token".into()))?;
    let (plays_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plays WHERE user_id = ?1")
        .bind(user.user_id)
        .fetch_one(&state.pool)
        .await?;
    Ok(Json(json!({
        "last_poll_ok_at": row.0,
        "next_poll_at": row.1,
        "poll_interval_secs": row.2,
        "rate_limited_until": row.3,
        "last_error": row.4,
        "last_error_at": row.5,
        "last_played_at": row.6,
        "plays_count": plays_count,
    })))
}

/// `GET /auth/tokens`: the caller's tokens (never the token or its hash).
async fn list_tokens(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let rows: Vec<(i64, i64, Option<i64>)> = sqlx::query_as(
        "SELECT id, created_at, last_used_at_ms FROM api_tokens WHERE user_id = ?1 ORDER BY id",
    )
    .bind(user.user_id)
    .fetch_all(&state.pool)
    .await?;
    let tokens: Vec<Value> = rows
        .into_iter()
        .map(|(id, created_at, last_used)| {
            json!({
                "id": id,
                "created_at": created_at * 1000,
                "last_used_at": last_used,
                "current": id == user.token_id,
            })
        })
        .collect();
    Ok(Json(json!({ "tokens": tokens })))
}

/// `POST /auth/token`: create a new token, returned once.
async fn create_token(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<(StatusCode, Json<Value>)> {
    let (id, token) = auth::issue_token_with_id(&state.pool, user.user_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "token": token })),
    ))
}

/// `DELETE /auth/token/{id}`: revoke one of the caller's tokens.
async fn revoke_token(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let res = sqlx::query("DELETE FROM api_tokens WHERE id = ?1 AND user_id = ?2")
        .bind(id)
        .bind(user.user_id)
        .execute(&state.pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound("token not found".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /me`: delete the account; plays and tokens cascade, shared tracks stay.
pub async fn delete_me(State(state): State<AppState>, user: AuthUser) -> AppResult<StatusCode> {
    sqlx::query("DELETE FROM users WHERE id = ?1")
        .bind(user.user_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
