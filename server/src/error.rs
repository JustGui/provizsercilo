use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub code: String,
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
            code: "bad_request".to_string(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
            code: "not_found".to_string(),
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid or missing admin token".to_string(),
            code: "unauthorized".to_string(),
        }
    }

    pub fn service_unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
            code: "no_providers_available".to_string(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
            code: "internal_error".to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": self.message,
            "code": self.code,
        });
        (self.status, Json(body)).into_response()
    }
}

impl From<storage_sqlite::StorageError> for AppError {
    fn from(e: storage_sqlite::StorageError) -> Self {
        match e {
            storage_sqlite::StorageError::NotFound(msg) => Self::not_found(msg),
            other => Self::internal(other.to_string()),
        }
    }
}
