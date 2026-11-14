//! Spotify OAuth login, token refresh and the background play poller.

mod client;
mod oauth;
mod poller;

use axum::Router;
use axum::routing::get;

use crate::AppState;

pub use poller::{PollError, poll_all_users, poll_user, spawn_poller};

/// Routes of this module: `/auth/login` and `/auth/callback`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(oauth::login))
        .route("/auth/callback", get(oauth::callback))
}
