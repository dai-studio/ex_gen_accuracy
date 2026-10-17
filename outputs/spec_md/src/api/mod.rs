//! Built-in routes: pages, sign-in, the JSON API and static UI assets.
//!
//! Handlers work on the small [`ApiRequest`]/[`ApiResponse`] types so they are
//! independent of the Pingora session and easy to test.

pub mod chat;
pub mod handlers;
pub mod pages;
pub mod signin;

use crate::error::MoxyError;
use crate::routing::{self, Builtin};
use crate::state::AppState;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use serde::Serialize;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Largest body accepted by built-in routes.
pub const MAX_API_BODY: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub client_ip: Option<IpAddr>,
}

impl ApiRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn query_param(&self, name: &str) -> Option<String> {
        crate::util::query_param(self.query.as_deref(), name)
    }

    pub fn path_and_query(&self) -> String {
        match &self.query {
            Some(q) if !q.is_empty() => format!("{}?{q}", self.path),
            _ => self.path.clone(),
        }
    }
}

pub enum Body {
    Full(Bytes),
    /// Chunks streamed to the caller as they arrive.
    Stream(mpsc::Receiver<Bytes>),
}

pub struct ApiResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Body,
}

impl std::fmt::Debug for ApiResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish()
    }
}

impl ApiResponse {
    pub fn new(status: StatusCode, body: Body) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body,
        }
    }

    pub fn json(status: StatusCode, value: &impl Serialize) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_default();
        Self::new(status, Body::Full(Bytes::from(body)))
            .header("content-type", "application/json")
            .header("cache-control", "no-store")
    }

    pub fn error(err: &MoxyError) -> Self {
        let mut r = Self::new(err.status, Body::Full(err.body_bytes()))
            .header("content-type", "application/json")
            .header("cache-control", "no-store");
        for (name, value) in &err.headers {
            r = r.header(name, value);
        }
        r
    }

    pub fn empty(status: StatusCode) -> Self {
        Self::new(status, Body::Full(Bytes::new()))
    }

    pub fn redirect(location: &str) -> Self {
        Self::empty(StatusCode::FOUND)
            .header("location", location)
            .header("cache-control", "no-store")
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            self.headers.append(n, v);
        }
        self
    }

    /// The full body, for tests and non-streaming callers.
    pub fn body_bytes(&self) -> Option<&Bytes> {
        match &self.body {
            Body::Full(b) => Some(b),
            Body::Stream(_) => None,
        }
    }
}

impl From<MoxyError> for ApiResponse {
    fn from(e: MoxyError) -> Self {
        ApiResponse::error(&e)
    }
}

/// Dispatch a request for a built-in route.
pub async fn handle(state: &Arc<AppState>, req: ApiRequest) -> ApiResponse {
    let Some(builtin) = routing::classify(&req.path) else {
        return MoxyError::route_not_found().into();
    };
    let result = match builtin {
        Builtin::Root | Builtin::Chat | Builtin::Admin => pages::page(state, &req, builtin).await,
        Builtin::App => pages::asset(&req),
        Builtin::Auth => signin::handle(state, &req).await,
        Builtin::Api => handlers::handle(state, req).await,
    };
    result.unwrap_or_else(|e| e.into())
}
