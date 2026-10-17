//! Errors returned by Moxy itself (not by an upstream).
//!
//! Every such error is rendered as `{"error": {"type": "<snake_case_type>", "message": "..."}}`.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct MoxyError {
    pub status: u16,
    pub kind: &'static str,
    pub message: String,
    /// Extra response headers (e.g. Retry-After).
    pub headers: Vec<(String, String)>,
    /// Optional list of details (validation errors).
    pub details: Option<Vec<String>>,
}

impl MoxyError {
    pub fn new(status: u16, kind: &'static str, message: impl Into<String>) -> Self {
        MoxyError {
            status,
            kind,
            message: message.into(),
            headers: Vec::new(),
            details: None,
        }
    }

    pub fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn body(&self) -> Value {
        let mut err = json!({ "type": self.kind, "message": self.message });
        if let Some(details) = &self.details {
            err["details"] = json!(details);
        }
        json!({ "error": err })
    }

    pub fn body_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.body()).unwrap_or_default()
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(400, "bad_request", msg)
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(401, "unauthorized", msg)
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(403, "forbidden", msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(404, "not_found", msg)
    }
    pub fn route_not_found(path: &str) -> Self {
        Self::new(404, "route_not_found", format!("no route matches {path}"))
    }
    pub fn method_not_allowed() -> Self {
        Self::new(405, "method_not_allowed", "method not allowed")
    }
    pub fn payload_too_large(limit: u64) -> Self {
        Self::new(
            413,
            "payload_too_large",
            format!("request body exceeds {limit} bytes"),
        )
    }
    pub fn validation(errors: Vec<String>) -> Self {
        let mut e = Self::new(422, "validation_failed", errors.join("; "));
        e.details = Some(errors);
        e
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(500, "internal_error", msg)
    }
    pub fn upstream_unavailable(msg: impl Into<String>) -> Self {
        Self::new(502, "upstream_unavailable", msg)
    }
    pub fn upstream_timeout(msg: impl Into<String>) -> Self {
        Self::new(504, "upstream_timeout", msg)
    }
}

impl std::fmt::Display for MoxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.status, self.kind, self.message)
    }
}

impl std::error::Error for MoxyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_shape() {
        let e = MoxyError::route_not_found("/x");
        assert_eq!(e.body()["error"]["type"], "route_not_found");
        assert!(e.body()["error"]["message"].as_str().unwrap().contains("/x"));
        assert!(e.body()["error"].get("details").is_none());
        let v = MoxyError::validation(vec!["a".into(), "b".into()]);
        assert_eq!(v.status, 422);
        assert_eq!(v.body()["error"]["details"][1], "b");
    }
}
