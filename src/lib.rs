//! spotifast-stats-api: a multi-user stats.fm-like API on top of Spotify.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod import;
pub mod spotify;
pub mod stats;

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::auth::AuthUser;
use crate::config::Config;
use crate::error::AppResult;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            http: reqwest::Client::new(),
        }
    }
}

/// Build the full router.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/me", get(me))
        .merge(spotify::router())
        .merge(stats::router())
        .merge(import::router())
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn me(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let row: (i64, String, String) =
        sqlx::query_as("SELECT id, spotify_id, display_name FROM users WHERE id = ?1")
            .bind(user.user_id)
            .fetch_one(&state.pool)
            .await?;
    Ok(Json(json!({
        "id": row.0,
        "spotify_id": row.1,
        "display_name": row.2,
    })))
}
