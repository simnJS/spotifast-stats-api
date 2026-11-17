//! Account endpoints: poller status, API token management and account deletion.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::AppState;
use crate::auth::{self, AuthUser};
use crate::error::{AppError, AppResult};
use crate::spotify::rate_limit;
use crate::spotify::round_tenth;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/me/status", get(status))
        .route("/auth/tokens", get(list_tokens))
        .route("/auth/token", post(create_token))
        .route("/auth/token/{id}", delete(revoke_token))
}

/// The poller columns of the user row that `/me/status` shows.
#[derive(sqlx::FromRow)]
struct StatusRow {
    last_poll_ok_at_ms: Option<i64>,
    next_poll_at_ms: Option<i64>,
    poll_interval_secs: Option<i64>,
    secs_per_play: Option<f64>,
    last_error: Option<String>,
    last_error_at_ms: Option<i64>,
    last_polled_at_ms: Option<i64>,
}

/// `GET /me/status`: why is (or is not) my listening being recorded.
///
/// Times are unix ms. `next_poll_at` is when the poller reads Spotify next for
/// this user: the scheduled poll, or the end of the app-wide Spotify rate limit
/// (`rate_limited_until`) when that is later (`null`: at once).
async fn status(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let row: StatusRow = sqlx::query_as(
        "SELECT last_poll_ok_at_ms, next_poll_at_ms, poll_interval_secs, secs_per_play, \
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
    let rate_limited_until = rate_limit::blocked_until(&state.pool, state.clock.now_ms()).await?;
    let next_poll_at = match (row.next_poll_at_ms, rate_limited_until) {
        (Some(next), Some(until)) => Some(next.max(until)),
        (next, until) => next.or(until),
    };
    Ok(Json(json!({
        "last_poll_ok_at": row.last_poll_ok_at_ms,
        "next_poll_at": next_poll_at,
        "poll_interval_secs": row.poll_interval_secs,
        "poll_min_interval_secs": state.config.poll_interval_secs,
        "poll_max_interval_secs": state.config.poll_max_interval_secs,
        "secs_per_play": row.secs_per_play.map(round_tenth),
        "rate_limited_until": rate_limited_until,
        "last_error": row.last_error,
        "last_error_at": row.last_error_at_ms,
        "last_played_at": row.last_polled_at_ms,
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
