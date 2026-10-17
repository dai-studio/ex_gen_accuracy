//! Small helpers shared across modules.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// `n` random bytes encoded as URL-safe base64 without padding.
pub fn random_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn sha256_b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(data))
}

/// Value of the cookie `name` in a `Cookie` header.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// A `Cookie` header with the cookie `name` removed; `None` when nothing remains.
pub fn strip_cookie(header: &str, name: &str) -> Option<String> {
    let kept: Vec<&str> = header
        .split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| p.split_once('=').map(|(k, _)| k.trim()) != Some(name))
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

/// Parse an `application/x-www-form-urlencoded` query string into pairs.
pub fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    match query {
        Some(q) => url::form_urlencoded::parse(q.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
        None => Vec::new(),
    }
}

pub fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    query_pairs(query)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

/// Accept only same-origin relative paths for `return_to`.
pub fn safe_return_to(value: Option<&str>) -> Option<String> {
    let v = value?.trim();
    if !v.starts_with('/') || v.starts_with("//") || v.starts_with("/\\") {
        return None;
    }
    if v.contains('\\') || v.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(v.to_string())
}

/// The `Bearer` token of an `Authorization` header.
pub fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_url_safe() {
        let t = random_token(32);
        assert_eq!(t.len(), 43);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(random_token(32), t);
    }

    #[test]
    fn hashes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // RFC 7636 appendix B
        assert_eq!(
            sha256_b64url(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn cookies() {
        let h = "a=1; moxy_session=abc; b=2";
        assert_eq!(cookie_value(h, "moxy_session"), Some("abc"));
        assert_eq!(cookie_value(h, "c"), None);
        assert_eq!(strip_cookie(h, "moxy_session").as_deref(), Some("a=1; b=2"));
        assert_eq!(strip_cookie("moxy_session=abc", "moxy_session"), None);
    }

    #[test]
    fn return_to() {
        assert_eq!(safe_return_to(Some("/chat")).as_deref(), Some("/chat"));
        assert_eq!(
            safe_return_to(Some("/admin?x=1")).as_deref(),
            Some("/admin?x=1")
        );
        assert_eq!(safe_return_to(Some("//evil.com")), None);
        assert_eq!(safe_return_to(Some("https://evil.com")), None);
        assert_eq!(safe_return_to(Some("/\\evil.com")), None);
        assert_eq!(safe_return_to(Some("chat")), None);
        assert_eq!(safe_return_to(None), None);
    }

    #[test]
    fn bearer() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
    }
}
