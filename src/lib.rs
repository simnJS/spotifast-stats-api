//! spotifast-stats-api: a multi-user stats.fm-like API on top of Spotify.

pub mod account;
pub mod auth;
pub mod clock;
pub mod config;
pub mod db;
pub mod error;
pub mod import;
pub mod spotify;
pub mod stats;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::{Level, Span};

use crate::auth::AuthUser;
use crate::clock::Clock;
use crate::config::Config;
use crate::error::AppResult;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    /// Time source: the system clock, simulated in tests.
    pub clock: Clock,
    /// Whether Spotify still serves the batch lookups (`/tracks?ids=`,
    /// `/artists?ids=`) to this app: cleared the first time it refuses one
    /// (Development Mode apps lost them in 2026), the metadata backfill then
    /// looks items up one by one.
    pub spotify_batch_lookups: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            http: reqwest::Client::new(),
            clock: Clock::system(),
            spotify_batch_lookups: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The same state on another clock (tests).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

/// Build the full router.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/me", get(me).delete(account::delete_me))
        .merge(account::router())
        .merge(spotify::router())
        .merge(stats::router())
        .merge(import::router())
        .with_state(state)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(request_span)
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
}

/// Span for one request: method and path only. The query string (OAuth
/// `code`/`state`), headers and bodies are deliberately never recorded.
pub fn request_span<B>(req: &Request<B>) -> Span {
    tracing::info_span!("request", method = %req.method(), path = %req.uri().path())
}

async fn health(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.pool)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(json!({ "status": "ok" }))),
        Err(e) => {
            tracing::error!(error = %e, "health check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "error" })),
            )
        }
    }
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

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }

    #[tokio::test]
    async fn trace_logs_path_only() {
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(Level::INFO)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);
        let state = AppState::new(
            crate::db::test_pool().await,
            crate::config::Config::for_tests(),
        );
        let req = Request::builder()
            .uri("/health?code=SECRETCODE&state=SECRETSTATE")
            .header("authorization", "Bearer SECRETTOKEN")
            .body(Body::empty())
            .unwrap();
        app(state).oneshot(req).await.unwrap();
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("path=/health"), "{out}");
        assert!(out.contains("method=GET"), "{out}");
        assert!(out.contains("status=200") || out.contains("200"), "{out}");
        for secret in ["SECRETCODE", "SECRETSTATE", "SECRETTOKEN", "code=", "?"] {
            assert!(!out.contains(secret), "leaked {secret}: {out}");
        }
    }
}
