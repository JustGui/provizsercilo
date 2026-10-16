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
    pub attempts: Option<Box<serde_json::Value>>,
    pub retry_after_secs: Option<u64>,
}

impl AppError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
            code: "not_found".to_string(),
            fallback_chain: None,
            attempts: None,
            retry_after_secs: None,
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid or missing admin token".to_string(),
            code: "unauthorized".to_string(),
            fallback_chain: None,
            attempts: None,
            retry_after_secs: None,
        }
    }

    pub fn service_unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
            code: "no_providers_available".to_string(),
            fallback_chain: None,
            attempts: None,
            retry_after_secs: Some(1),
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
            attempts: serde_json::to_value(attempts).ok().map(Box::new),
            retry_after_secs: Some(1),
        }
    }

    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_secs = Some(seconds.max(1));
        self
    }

    pub fn request_error(message: String, chain: String, attempts: &impl serde::Serialize) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "request_error".into(),
            message,
            fallback_chain: Some(chain),
            attempts: serde_json::to_value(attempts).ok().map(Box::new),
            retry_after_secs: None,
        }
    }

    pub fn probe_busy() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: "Probe already running or concurrency limit reached".into(),
            code: "probe_busy".into(),
            fallback_chain: None,
            attempts: None,
            retry_after_secs: None,
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
            code: "internal_error".to_string(),
            fallback_chain: None,
            attempts: None,
            retry_after_secs: None,
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
            body["attempts"] = *attempts;
        }
        let mut response = (self.status, Json(body)).into_response();
        if let Some(seconds) = self.retry_after_secs {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                seconds.max(1).to_string().parse().expect("integer header"),
            );
        }
        response
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_providers_retry_after_is_at_least_one_second() {
        assert_eq!(
            AppError::service_unavailable("none")
                .into_response()
                .headers()["Retry-After"],
            "1"
        );
        assert_eq!(
            AppError::service_unavailable("none")
                .with_retry_after(0)
                .into_response()
                .headers()["Retry-After"],
            "1"
        );
        assert_eq!(
            AppError::service_unavailable("none")
                .with_retry_after(37)
                .into_response()
                .headers()["Retry-After"],
            "37"
        );
        assert!(!AppError::unauthorized()
            .into_response()
            .headers()
            .contains_key("Retry-After"));
    }
}
