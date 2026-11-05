use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApiError {
    Unauthenticated,
    PermissionDenied,
    NotFound,
    InvalidArgument,
    Conflict,
    IdempotencyConflict,
    LastManager,
    InternalError,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::PermissionDenied => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::InvalidArgument => StatusCode::BAD_REQUEST,
            Self::Conflict | Self::IdempotencyConflict | Self::LastManager => StatusCode::CONFLICT,
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(serde_json::json!({"code": self}))).into_response()
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(_: rusqlite::Error) -> Self {
        Self::InternalError
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(_: serde_json::Error) -> Self {
        Self::InternalError
    }
}
