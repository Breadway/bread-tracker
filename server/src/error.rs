//! API error type with JSON bodies.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("too many attempts, try again later")]
    TooManyRequests,
    #[error("{0}")]
    BadRequest(String),
    /// No such object for this caller. Used where confirming an id would leak another account.
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    /// A per-user or server-wide limit was reached (rows, devices, users).
    #[error("{0}")]
    LimitReached(String),
    #[error("this account is disabled")]
    Disabled,
    #[error("{0}")]
    Forbidden(String),
    #[error("server is busy, try again")]
    Unavailable,
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, self.to_string()),
            ApiError::TooManyRequests => (StatusCode::TOO_MANY_REQUESTS, self.to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            ApiError::LimitReached(m) => (StatusCode::UNPROCESSABLE_ENTITY, m.clone()),
            ApiError::Disabled => (StatusCode::FORBIDDEN, self.to_string()),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            ApiError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
            ApiError::Internal(e) => {
                tracing::error!(error = ?e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
