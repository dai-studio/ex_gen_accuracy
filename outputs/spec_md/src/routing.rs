//! Classification of request paths into built-in routes.
//!
//! Built-in routes are reserved and always considered before proxy routes.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `/` (exact): endpoint discovery page.
    Root,
    /// `/chat` and everything beneath it.
    Chat,
    /// `/admin` and everything beneath it.
    Admin,
    /// Everything beneath `/auth`.
    Auth,
    /// Everything beneath `/api`.
    Api,
    /// Everything beneath `/_app`: static user interface assets.
    App,
}

const RESERVED: [&str; 5] = ["chat", "admin", "auth", "api", "_app"];

/// Return the built-in route that owns `path`, if any.
pub fn classify(path: &str) -> Option<Builtin> {
    if path == "/" {
        return Some(Builtin::Root);
    }
    let rest = path.strip_prefix('/')?;
    let first = rest.split('/').next().unwrap_or("");
    match first {
        "chat" => Some(Builtin::Chat),
        "admin" => Some(Builtin::Admin),
        "auth" => Some(Builtin::Auth),
        "api" => Some(Builtin::Api),
        "_app" => Some(Builtin::App),
        _ => None,
    }
}

/// Whether a route's literal path falls under one of the reserved prefixes.
pub fn is_reserved(literal: &str) -> bool {
    let rest = literal.trim_start_matches('/');
    let first = rest.split('/').next().unwrap_or("");
    RESERVED.contains(&first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(classify("/"), Some(Builtin::Root));
        assert_eq!(classify("/chat"), Some(Builtin::Chat));
        assert_eq!(classify("/chat/x"), Some(Builtin::Chat));
        assert_eq!(classify("/admin/routes"), Some(Builtin::Admin));
        assert_eq!(classify("/auth/login"), Some(Builtin::Auth));
        assert_eq!(classify("/api/endpoints"), Some(Builtin::Api));
        assert_eq!(classify("/_app/immutable/x.js"), Some(Builtin::App));
        assert_eq!(classify("/chatty"), None);
        assert_eq!(classify("/openai/v1"), None);
        assert_eq!(classify(""), None);
    }

    #[test]
    fn reserved() {
        assert!(is_reserved("/chat/"));
        assert!(is_reserved("/api"));
        assert!(!is_reserved("/apis/"));
        assert!(!is_reserved("/openai/"));
    }
}
