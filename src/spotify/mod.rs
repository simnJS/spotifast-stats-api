//! Spotify OAuth, token refresh and the play poller (coming).
//!
//! Owned by a later task: only files inside this folder should change.

use axum::Router;

use crate::AppState;

/// Routes of this module; empty for now.
pub fn router() -> Router<AppState> {
    Router::new()
}

/// Start the background poller that records plays for every user.
/// No-op for now; called once from `main`.
pub fn spawn_poller(_state: AppState) {}
