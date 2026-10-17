//! A small request/response model used by the built-in routes, independent of Pingora
//! so the handlers can be unit-tested.

use crate::error::MoxyError;
use bytes::Bytes;
use http::HeaderMap;
use serde_json::Value;
use std::net::IpAddr;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub client_ip: Option<IpAddr>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn json(&self) -> Result<Value, MoxyError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| MoxyError::bad_request(format!("invalid JSON body: {e}")))
    }
}

pub enum Body {
    Full(Bytes),
    /// Streamed body (e.g. server-sent events); ends when the sender is dropped.
    Stream(mpsc::Receiver<Bytes>),
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Resp {
    pub fn new(status: u16) -> Self {
        Resp {
            status,
            headers: Vec::new(),
            body: Body::Full(Bytes::new()),
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn bytes(status: u16, content_type: &str, body: impl Into<Bytes>) -> Self {
        Resp {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            body: Body::Full(body.into()),
        }
    }

    pub fn json(status: u16, v: &Value) -> Self {
        Self::bytes(
            status,
            "application/json",
            serde_json::to_vec(v).unwrap_or_default(),
        )
    }

    pub fn error(e: &MoxyError) -> Self {
        let mut r = Self::bytes(e.status, "application/json", e.body_bytes());
        for (k, v) in &e.headers {
            r.headers.push((k.clone(), v.clone()));
        }
        r
    }

    pub fn redirect(location: &str) -> Self {
        Resp::new(302).header("location", location)
    }

    pub fn sse(rx: mpsc::Receiver<Bytes>) -> Self {
        Resp {
            status: 200,
            headers: vec![
                ("content-type".into(), "text/event-stream".into()),
                ("cache-control".into(), "no-cache".into()),
            ],
            body: Body::Stream(rx),
        }
    }

    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Collect the body (tests).
    pub async fn body_bytes(self) -> Bytes {
        match self.body {
            Body::Full(b) => b,
            Body::Stream(mut rx) => {
                let mut out = Vec::new();
                while let Some(c) = rx.recv().await {
                    out.extend_from_slice(&c);
                }
                out.into()
            }
        }
    }
}

impl From<MoxyError> for Resp {
    fn from(e: MoxyError) -> Self {
        Resp::error(&e)
    }
}
