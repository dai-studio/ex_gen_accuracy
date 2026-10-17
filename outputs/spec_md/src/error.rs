//! Moxy's own error responses.
//!
//! Every error Moxy produces itself (as opposed to an error passed through
//! from an upstream) is rendered as `{"error": {"type": "...", "message": "..."}}`.

use http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};

/// An error that Moxy reports to a caller.
#[derive(Debug, Clone)]
pub struct MoxyError {
    pub status: StatusCode,
    /// Short machine-readable identifier in snake case.
    pub kind: &'static str,
    /// Human-readable explanation.
    pub message: String,
    /// Optional structured details (for example validation errors).
    pub details: Option<Value>,
    /// Extra response headers such as `Retry-After`.
    pub headers: Vec<(&'static str, String)>,
}

impl MoxyError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            details: None,
            headers: Vec::new(),
        }
    }

    pub fn with_details(mut self, details: impl Serialize) -> Self {
        self.details = serde_json::to_value(details).ok();
        self
    }

    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn route_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "route_not_found",
            "no route matches this path",
        )
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method not allowed on this path",
        )
    }

    pub fn payload_too_large(limit: u64) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("request body exceeds the limit of {limit} bytes"),
        )
    }

    pub fn upstream_unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "upstream_unavailable", message)
    }

    pub fn upstream_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "the upstream did not respond in time",
        )
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
    }

    /// The JSON body for this error.
    pub fn body_json(&self) -> Value {
        let mut err = json!({ "type": self.kind, "message": self.message });
        if let Some(details) = &self.details {
            err["details"] = details.clone();
        }
        json!({ "error": err })
    }

    pub fn body_bytes(&self) -> bytes::Bytes {
        bytes::Bytes::from(serde_json::to_vec(&self.body_json()).unwrap_or_default())
    }
}

impl std::fmt::Display for MoxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.kind,
            self.status.as_u16(),
            self.message
        )
    }
}

impl std::error::Error for MoxyError {}

impl From<anyhow::Error> for MoxyError {
    fn from(e: anyhow::Error) -> Self {
        log::error!("internal error: {e:#}");
        MoxyError::internal("internal server error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_shape() {
        let e = MoxyError::route_not_found();
        let v = e.body_json();
        assert_eq!(v["error"]["type"], "route_not_found");
        assert!(v["error"]["message"].is_string());
        assert_eq!(v.as_object().unwrap().len(), 1);
    }
}
