//! Application error type, rendered as a JSON `{"error": "..."}` body.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    /// Too many requests from this client; the value is the `Retry-After` in seconds.
    #[error("too many requests, retry in {0} s")]
    TooManyRequests(u64),
    /// An upstream service (e.g. Spotify) failed.
    #[error("{0}")]
    BadGateway(String),
    /// Spotify rate limits the whole app (HTTP 429): 503, with the message
    /// saying until when and the `Retry-After` in seconds.
    #[error("{message}")]
    SpotifyRateLimited {
        message: String,
        retry_after_secs: u64,
    },
    #[error("{0}")]
    Internal(String),
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::SpotifyRateLimited { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The `Retry-After` of this error, in seconds, when it has one.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::TooManyRequests(secs) => Some(*secs),
            Self::SpotifyRateLimited {
                retry_after_secs, ..
            } => Some(*retry_after_secs),
            _ => None,
        }
    }

    /// The message shown to clients: internal details are not leaked.
    pub fn public_message(&self) -> String {
        match self {
            Self::Internal(_) => "internal server error".to_string(),
            other => other.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if let Self::Internal(m) = &self {
            tracing::error!(error = %m, "internal error");
        }
        let message = self.public_message();
        let mut resp = (status, Json(json!({ "error": message }))).into_response();
        if let Some(secs) = self.retry_after_secs() {
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(secs),
            );
        }
        resp
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        Self::BadGateway(e.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
