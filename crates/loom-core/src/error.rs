use serde::{Deserialize, Serialize};

/// Unified error codes (OpenAI-compatible `error.code` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidApiKey,
    PermissionDenied,
    ModelNotFound,
    RateLimited,
    ConcurrencyExceeded,
    PayloadTooLarge,
    Overloaded,
    RequestTimeout,
    UpstreamError,
    Cancelled,
    Unavailable,
    InvalidRequest,
    Internal,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::InvalidApiKey => "invalid_api_key",
            ErrorCode::PermissionDenied => "permission_denied",
            ErrorCode::ModelNotFound => "model_not_found",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::ConcurrencyExceeded => "concurrency_exceeded",
            ErrorCode::PayloadTooLarge => "payload_too_large",
            ErrorCode::Overloaded => "overloaded",
            ErrorCode::RequestTimeout => "request_timeout",
            ErrorCode::UpstreamError => "upstream_error",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::InvalidRequest => "invalid_request_error",
            ErrorCode::Internal => "internal_error",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            ErrorCode::InvalidApiKey => 401,
            ErrorCode::PermissionDenied => 403,
            ErrorCode::ModelNotFound => 404,
            ErrorCode::RateLimited | ErrorCode::ConcurrencyExceeded => 429,
            ErrorCode::PayloadTooLarge => 413,
            ErrorCode::Overloaded | ErrorCode::Unavailable => 503,
            ErrorCode::RequestTimeout => 408,
            ErrorCode::UpstreamError => 502,
            ErrorCode::Cancelled => 499,
            ErrorCode::InvalidRequest => 400,
            ErrorCode::Internal => 500,
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self {
            ErrorCode::InvalidApiKey | ErrorCode::PermissionDenied => "authentication_error",
            ErrorCode::RateLimited | ErrorCode::ConcurrencyExceeded => "rate_limit_error",
            ErrorCode::InvalidRequest | ErrorCode::PayloadTooLarge => "invalid_request_error",
            ErrorCode::UpstreamError | ErrorCode::Unavailable | ErrorCode::Overloaded => {
                "server_error"
            }
            _ => "server_error",
        }
    }
}

/// OpenAI-compatible error envelope: {"error": {...}}.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ApiError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error_type: code.error_type().to_string(),
            code: code.as_str().to_string(),
            message: message.into(),
            request_id: None,
            param: None,
        }
    }

    pub fn with_request_id(mut self, id: impl ToString) -> Self {
        self.request_id = Some(id.to_string());
        self
    }

    pub fn envelope(self) -> ErrorEnvelope {
        ErrorEnvelope { error: self }
    }
}

impl ErrorEnvelope {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ApiError::new(code, message).envelope()
    }

    /// HTTP status code matching the error code.
    pub fn http_status(&self) -> u16 {
        // Map back from the serialized code string to the enum's status.
        ErrorCode::from_code_str(&self.error.code)
            .map(|c| c.http_status())
            .unwrap_or(500)
    }
}

impl ErrorCode {
    /// Parse a serialized code string back into the enum (for status mapping).
    pub fn from_code_str(s: &str) -> Option<Self> {
        Some(match s {
            "invalid_api_key" => ErrorCode::InvalidApiKey,
            "permission_denied" => ErrorCode::PermissionDenied,
            "model_not_found" => ErrorCode::ModelNotFound,
            "rate_limited" => ErrorCode::RateLimited,
            "concurrency_exceeded" => ErrorCode::ConcurrencyExceeded,
            "payload_too_large" => ErrorCode::PayloadTooLarge,
            "overloaded" => ErrorCode::Overloaded,
            "request_timeout" => ErrorCode::RequestTimeout,
            "upstream_error" => ErrorCode::UpstreamError,
            "cancelled" => ErrorCode::Cancelled,
            "unavailable" => ErrorCode::Unavailable,
            "invalid_request_error" => ErrorCode::InvalidRequest,
            "internal_error" => ErrorCode::Internal,
            _ => return None,
        })
    }
}
