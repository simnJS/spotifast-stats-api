//! Streaming-history import (coming).
//!
//! Owned by a later task: only files inside this folder should change.

use axum::Router;

use crate::AppState;

/// Routes of this module; empty for now.
pub fn router() -> Router<AppState> {
    Router::new()
}
