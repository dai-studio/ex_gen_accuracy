//! The optional per-route response cache, stored under `cache#<sha256>`.

use crate::kv::{KvStore, KvStoreExt};
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Largest response body that will be cached.
pub const MAX_CACHED_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Path of the route the entry belongs to, used to discard entries when it changes.
    pub route: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Base64-encoded body.
    pub body: String,
    pub stored_at: DateTime<Utc>,
}

impl CacheEntry {
    pub fn body_bytes(&self) -> Bytes {
        Bytes::from(STANDARD.decode(&self.body).unwrap_or_default())
    }
}

/// Serialise JSON with object keys sorted, so equivalent bodies hash equally.
fn canonical_json(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                canonical_json(&map[k.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// JSON bodies are canonicalised; anything else is used as is.
pub fn normalise_body(body: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<Value>(body) {
        Ok(v) => {
            let mut s = String::new();
            canonical_json(&v, &mut s);
            s.into_bytes()
        }
        Err(_) => body.to_vec(),
    }
}

/// `cache#` + SHA-256 over route path, method, upstream URL, normalised body and caller.
pub fn cache_key(
    route: &str,
    method: &str,
    upstream_url: &str,
    body: &[u8],
    caller: &str,
) -> String {
    let mut h = Sha256::new();
    for part in [
        route.as_bytes(),
        method.as_bytes(),
        upstream_url.as_bytes(),
        &normalise_body(body),
        caller.as_bytes(),
    ] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part);
    }
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("cache#{hex}")
}

fn cache_control_has(headers: &HeaderMap, directives: &[&str]) -> bool {
    headers
        .get_all(http::header::CACHE_CONTROL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|d| d.trim().to_ascii_lowercase())
        .any(|d| {
            directives
                .iter()
                .any(|x| d == *x || d.starts_with(&format!("{x}=")))
        })
}

/// A request carrying `Cache-Control: no-cache` bypasses the cache.
pub fn request_bypasses(headers: &HeaderMap) -> bool {
    cache_control_has(headers, &["no-cache"])
}

/// Whether the response content type indicates a streamed response.
pub fn is_streamed_content_type(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.contains("event-stream") || ct.contains("ndjson") || ct.contains("jsonl")
}

/// Only non-streamed 200 responses without `no-store`/`private` are stored.
pub fn response_storable(status: u16, headers: &HeaderMap, streamed: bool) -> bool {
    status == 200 && !streamed && !cache_control_has(headers, &["no-store", "private"])
}

pub fn lookup(kv: &dyn KvStore, key: &str) -> Result<Option<CacheEntry>> {
    kv.get_json(key)
}

pub fn store(
    kv: &dyn KvStore,
    key: &str,
    route: &str,
    status: u16,
    headers: Vec<(String, String)>,
    body: &[u8],
    ttl: Duration,
) -> Result<()> {
    let now = Utc::now();
    let entry = CacheEntry {
        route: route.to_string(),
        status,
        headers,
        body: STANDARD.encode(body),
        stored_at: now,
    };
    let expires = now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::minutes(5));
    kv.put_json(key, &entry, Some(expires))
}

/// Discard every entry belonging to `route`.
pub fn purge_route(kv: &dyn KvStore, route: &str) -> Result<usize> {
    kv.delete_where("cache#", &|v| {
        v.get("route").and_then(Value::as_str) == Some(route)
    })
}

/// Response headers worth storing (hop-by-hop and length headers are recomputed on replay).
pub fn storable_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    const SKIP: [&str; 6] = [
        "connection",
        "keep-alive",
        "transfer-encoding",
        "content-length",
        "proxy-connection",
        "upgrade",
    ];
    headers
        .iter()
        .filter(|(n, _)| !SKIP.contains(&n.as_str()))
        .filter_map(|(n, v)| Some((n.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::MemoryStore;

    #[test]
    fn key_depends_on_every_part() {
        let base = cache_key("/a/*", "GET", "http://u/x", b"", "apikey:1");
        assert!(base.starts_with("cache#"));
        assert_eq!(base.len(), 6 + 64);
        assert_eq!(
            base,
            cache_key("/a/*", "GET", "http://u/x", b"", "apikey:1")
        );
        assert_ne!(
            base,
            cache_key("/b/*", "GET", "http://u/x", b"", "apikey:1")
        );
        assert_ne!(
            base,
            cache_key("/a/*", "POST", "http://u/x", b"", "apikey:1")
        );
        assert_ne!(
            base,
            cache_key("/a/*", "GET", "http://u/y", b"", "apikey:1")
        );
        assert_ne!(
            base,
            cache_key("/a/*", "GET", "http://u/x", b"x", "apikey:1")
        );
        assert_ne!(
            base,
            cache_key("/a/*", "GET", "http://u/x", b"", "apikey:2")
        );
    }

    #[test]
    fn json_bodies_are_normalised() {
        let a = cache_key(
            "/a/*",
            "POST",
            "u",
            br#"{"b":1,"a":[1,{"y":2,"x":1}]}"#,
            "c",
        );
        let b = cache_key(
            "/a/*",
            "POST",
            "u",
            br#"{ "a": [1, {"x":1, "y":2}], "b": 1 }"#,
            "c",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn cache_control_rules() {
        let mut h = HeaderMap::new();
        assert!(!request_bypasses(&h));
        h.insert("cache-control", "max-age=0, no-cache".parse().unwrap());
        assert!(request_bypasses(&h));

        let mut r = HeaderMap::new();
        assert!(response_storable(200, &r, false));
        assert!(!response_storable(201, &r, false));
        assert!(!response_storable(200, &r, true));
        r.insert("cache-control", "private, max-age=10".parse().unwrap());
        assert!(!response_storable(200, &r, false));
        r.insert("cache-control", "no-store".parse().unwrap());
        assert!(!response_storable(200, &r, false));
        assert!(is_streamed_content_type("text/event-stream"));
        assert!(!is_streamed_content_type("application/json"));
    }

    #[test]
    fn store_lookup_purge() {
        let kv = MemoryStore::new();
        let k1 = cache_key("/a/*", "GET", "u1", b"", "c");
        let k2 = cache_key("/b/*", "GET", "u2", b"", "c");
        store(
            &kv,
            &k1,
            "/a/*",
            200,
            vec![("content-type".into(), "text/plain".into())],
            b"hello",
            Duration::from_secs(60),
        )
        .unwrap();
        store(
            &kv,
            &k2,
            "/b/*",
            200,
            vec![],
            b"world",
            Duration::from_secs(60),
        )
        .unwrap();
        let e = lookup(&kv, &k1).unwrap().unwrap();
        assert_eq!(&e.body_bytes()[..], b"hello");
        assert_eq!(e.headers[0].0, "content-type");
        assert_eq!(purge_route(&kv, "/a/*").unwrap(), 1);
        assert!(lookup(&kv, &k1).unwrap().is_none());
        assert!(lookup(&kv, &k2).unwrap().is_some());
    }
}
