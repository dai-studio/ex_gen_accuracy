//! Built-in, reserved routes: `/`, `/chat`, `/admin`, `/auth/*`, `/api/*`, `/_app/*`.

use crate::auth;
use crate::error::MoxyError;
use crate::http::{Req, Resp};
use crate::state::AppState;
use crate::ui;
use std::sync::Arc;

fn under(path: &str, prefix: &str) -> bool {
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Is `path` handled by a built-in route? Built-ins are checked before proxy routes.
pub fn is_builtin(path: &str) -> bool {
    path == "/"
        || under(path, "/chat")
        || under(path, "/admin")
        || under(path, "/auth")
        || under(path, "/api")
        || under(path, "/_app")
}

/// Largest request body accepted by built-in routes.
pub const MAX_BUILTIN_BODY: usize = 4 * 1024 * 1024;

fn login_redirect(path: &str) -> Resp {
    let rt: String = url::form_urlencoded::byte_serialize(path.as_bytes()).collect();
    Resp::redirect(&format!("/auth/login?return_to={rt}"))
}

pub async fn handle(state: &Arc<AppState>, req: Req) -> Resp {
    let path = req.path.clone();
    let page_method = matches!(req.method.as_str(), "GET" | "HEAD");
    if under(&path, "/api") {
        return crate::api::handle(state, req).await;
    }
    if under(&path, "/_app") {
        if !page_method {
            return MoxyError::method_not_allowed().into();
        }
        return ui::asset(&path["/_app".len()..].trim_start_matches('/').to_string());
    }
    if under(&path, "/auth") {
        return match (req.method.as_str(), path.as_str()) {
            ("GET", "/auth/login") => auth::login(state, &req).await,
            ("GET", "/auth/callback") => auth::callback(state, &req).await,
            ("POST", "/auth/logout") => auth::logout(state, &req),
            (_, "/auth/login" | "/auth/callback" | "/auth/logout") => {
                MoxyError::method_not_allowed().into()
            }
            _ => MoxyError::route_not_found(&path).into(),
        };
    }
    if !page_method {
        return MoxyError::method_not_allowed().into();
    }
    if path == "/" {
        return ui::index();
    }
    // /chat and /admin need a signed-in browser session.
    let ident = match auth::authenticate_session(state, req.header("cookie")) {
        Ok(i) => i,
        Err(e) => return e.into(),
    };
    let Some(ident) = ident else {
        return login_redirect(&path);
    };
    if under(&path, "/admin") && !ident.is_admin() {
        return MoxyError::forbidden("admin role required").into();
    }
    ui::index()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_paths() {
        for p in ["/", "/chat", "/chat/x", "/admin", "/auth/login", "/api/me", "/_app/x.js"] {
            assert!(is_builtin(p), "{p}");
        }
        for p in ["/chatgpt", "/openai/x", "/apix", "/x"] {
            assert!(!is_builtin(p), "{p}");
        }
    }
}
