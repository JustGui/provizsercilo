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
    pub fallback_chain: Option<String>,
    pub attempts: Option<serde_json::Value>,
}

impl AppError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
            code: "not_found".to_string(),
            fallback_chain: None,
            attempts: None,
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid or missing admin token".to_string(),
            code: "unauthorized".to_string(),
            fallback_chain: None,
            attempts: None,
        }
    }

    pub fn service_unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
            code: "no_providers_available".to_string(),
            fallback_chain: None,
            attempts: None,
        }
    }

    /// Same as `service_unavailable` but attaches the full attempt/skip chain
    /// so the caller can see exactly which backends were tried (and why the
    /// rest were skipped) instead of a bare, unexplained 503.
    pub fn service_unavailable_with_attempts(
        msg: impl Into<String>,
        fallback_chain: impl Into<String>,
        attempts: &impl serde::Serialize,
    ) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
            code: "no_providers_available".to_string(),
            fallback_chain: Some(fallback_chain.into()),
            attempts: serde_json::to_value(attempts).ok(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
            code: "internal_error".to_string(),
            fallback_chain: None,
            attempts: None,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "error": self.message,
            "code": self.code,
        });
        if let Some(chain) = self.fallback_chain {
            body["fallback_chain"] = json!(chain);
        }
        if let Some(attempts) = self.attempts {
            body["attempts"] = attempts;
        }
        (self.status, Json(body)).into_response()
    }
}

impl From<proviz_core::storage::StorageError> for AppError {
    fn from(e: proviz_core::storage::StorageError) -> Self {
        match e {
            proviz_core::storage::StorageError::NotFound(msg) => Self::not_found(msg),
            other => Self::internal(other.to_string()),
        }
    }
}
