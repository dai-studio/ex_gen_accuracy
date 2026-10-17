//! Small helpers shared across modules.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Parse a duration such as `500ms`, `30s`, `5m`, `24h` or `30d`.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    let secs = match unit {
        "ms" => return Some(Duration::from_millis(n)),
        "s" => n,
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3600)?,
        "d" => n.checked_mul(86400)?,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// `n` random bytes, base64url-encoded without padding.
pub fn random_b64(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Accept only same-origin relative paths for redirects (`/foo`, not `//evil` or `https://..`).
pub fn safe_return_to(p: Option<&str>) -> String {
    match p {
        Some(p)
            if p.starts_with('/')
                && !p.starts_with("//")
                && !p.starts_with("/\\")
                && !p.chars().any(|c| c.is_control()) =>
        {
            p.to_string()
        }
        _ => "/".to_string(),
    }
}

/// Parse a query string into decoded (key, value) pairs.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(q.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

pub fn query_param(q: &str, name: &str) -> Option<String> {
    parse_query(q).into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// Read a cookie value from a Cookie header.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// Remove one cookie from a Cookie header; `None` when nothing is left.
pub fn strip_cookie(header: &str, name: &str) -> Option<String> {
    let rest: Vec<&str> = header
        .split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| p.split_once('=').map(|(k, _)| k.trim()) != Some(name))
        .collect();
    (!rest.is_empty()).then(|| rest.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("24h"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_duration("30d"), Some(Duration::from_secs(30 * 86400)));
        assert_eq!(parse_duration("5"), None);
        assert_eq!(parse_duration("m"), None);
        assert_eq!(parse_duration("5y"), None);
    }

    #[test]
    fn return_to() {
        assert_eq!(safe_return_to(Some("/chat")), "/chat");
        assert_eq!(safe_return_to(Some("//evil.com")), "/");
        assert_eq!(safe_return_to(Some("https://evil.com")), "/");
        assert_eq!(safe_return_to(Some("/\\evil.com")), "/");
        assert_eq!(safe_return_to(None), "/");
    }

    #[test]
    fn cookies() {
        let h = "a=1; moxy_session=abc; b=2";
        assert_eq!(cookie_value(h, "moxy_session").as_deref(), Some("abc"));
        assert_eq!(strip_cookie(h, "moxy_session").as_deref(), Some("a=1; b=2"));
        assert_eq!(strip_cookie("moxy_session=x", "moxy_session"), None);
    }

    #[test]
    fn randoms() {
        assert_eq!(random_b64(32).len(), 43);
        assert_eq!(random_b64(24).len(), 32);
        assert_eq!(random_hex(8).len(), 16);
    }
}
