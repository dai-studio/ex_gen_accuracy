//! `/api/*`: Moxy's own JSON API (used by the web UI and by scripts).

use crate::auth::{self, Identity, Via};
use crate::config::{ActiveConfig, Config, Middleware, Route};
use crate::error::MoxyError;
use crate::http::{Req, Resp};
use crate::state::AppState;
use serde_json::{Value, json};
use std::sync::Arc;

/// Authenticate and authorise an /api call. State-changing calls made with a session
/// cookie must carry the session's CSRF token in `X-CSRF-Token`.
pub fn require(state: &AppState, req: &Req, admin: bool) -> Result<Identity, MoxyError> {
    let ident = auth::authenticate(state, req)?
        .ok_or_else(|| MoxyError::unauthorized("sign in or send an API key"))?;
    if admin && !ident.is_admin() {
        return Err(MoxyError::forbidden("admin role required"));
    }
    if let Via::Session { csrf, .. } = &ident.via {
        let safe = matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS");
        if !safe && req.header("x-csrf-token") != Some(csrf.as_str()) {
            return Err(MoxyError::new(
                403,
                "csrf_failed",
                "missing or invalid X-CSRF-Token header",
            ));
        }
    }
    Ok(ident)
}

pub fn endpoints_json(active: &ActiveConfig) -> Value {
    let list: Vec<Value> = active
        .resolved
        .routes
        .iter()
        .map(|r| {
            json!({
                "path": r.path,
                "description": r.description,
                "auth": r.auth.as_str(),
                "chat": r.ai.is_some(),
            })
        })
        .collect();
    json!({ "endpoints": list })
}

fn config_json(state: &AppState) -> Value {
    let active = state.active();
    json!({
        "version": active.version,
        "loadedAt": active.loaded_at,
        "path": state.env.config_path.display().to_string(),
        "lastError": *state.last_error.lock(),
        "config": active.raw,
    })
}

pub async fn handle(state: &Arc<AppState>, req: Req) -> Resp {
    match dispatch(state, &req).await {
        Ok(r) => r,
        Err(e) => Resp::error(&e),
    }
}

async fn dispatch(state: &Arc<AppState>, req: &Req) -> Result<Resp, MoxyError> {
    let path = req.path.trim_end_matches('/');
    let m = req.method.as_str();
    match path {
        "/api/endpoints" => {
            only(m, &["GET", "HEAD"])?;
            Ok(Resp::json(200, &endpoints_json(&state.active())))
        }
        "/api/me" => {
            only(m, &["GET", "HEAD"])?;
            let id = require(state, req, false)?;
            let mut v = json!({
                "sub": id.sub,
                "email": id.email,
                "name": id.name,
                "role": id.role.as_str(),
            });
            match &id.via {
                Via::Session { csrf, .. } => {
                    v["authMethod"] = json!("session");
                    v["csrfToken"] = json!(csrf);
                }
                Via::ApiKey { id } => {
                    v["authMethod"] = json!("api-key");
                    v["apiKeyId"] = json!(id);
                }
            }
            Ok(Resp::json(200, &v))
        }
        "/api/chat" => {
            only(m, &["POST"])?;
            let id = require(state, req, false)?;
            Ok(crate::chat::chat(state, req, id).await)
        }
        "/api/config" => {
            only(m, &["GET", "HEAD"])?;
            require(state, req, true)?;
            Ok(Resp::json(200, &config_json(state)))
        }
        "/api/config/routes" => {
            only(m, &["PUT"])?;
            require(state, req, true)?;
            let body = req.json()?;
            let routes: Vec<Route> = from_value(unwrap_key(body, "routes"))?;
            let mut raw = state.active().raw.clone();
            raw.routes = routes;
            save(state, raw).await
        }
        "/api/config/settings" => {
            only(m, &["PUT"])?;
            require(state, req, true)?;
            let body = req.json()?;
            let obj = body
                .as_object()
                .ok_or_else(|| MoxyError::validation(vec!["body must be an object".into()]))?;
            let mut raw = state.active().raw.clone();
            if let Some(s) = obj.get("server") {
                raw.server = from_value(s.clone())?;
            }
            if let Some(a) = obj.get("auth") {
                raw.auth = from_value(a.clone())?;
            }
            save(state, raw).await
        }
        "/api/keys" => match m {
            "GET" | "HEAD" => {
                require(state, req, true)?;
                let keys = auth::list_api_keys(&*state.kv).map_err(MoxyError::internal)?;
                Ok(Resp::json(200, &json!({ "keys": keys })))
            }
            "POST" => {
                let id = require(state, req, true)?;
                let body = req.json()?;
                let name = body
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| MoxyError::validation(vec!["name is required".into()]))?;
                let (plaintext, rec) =
                    auth::new_api_key(&*state.kv, name, &id.sub).map_err(MoxyError::internal)?;
                let mut v = serde_json::to_value(&rec).unwrap_or_default();
                v["key"] = json!(plaintext);
                Ok(Resp::json(201, &v))
            }
            _ => Err(MoxyError::method_not_allowed()),
        },
        _ => {
            if let Some(id) = path.strip_prefix("/api/keys/") {
                only(m, &["DELETE"])?;
                require(state, req, true)?;
                return if auth::revoke_api_key(&*state.kv, id).map_err(MoxyError::internal)? {
                    Ok(Resp::json(200, &json!({ "id": id, "revoked": true })))
                } else {
                    Err(MoxyError::not_found(format!("no API key with id {id}")))
                };
            }
            if let Some(enc) = path
                .strip_prefix("/api/config/routes/")
                .and_then(|p| p.strip_suffix("/middleware"))
            {
                only(m, &["PUT"])?;
                require(state, req, true)?;
                let route_path = percent_encoding::percent_decode_str(enc)
                    .decode_utf8_lossy()
                    .into_owned();
                let body = req.json()?;
                let mw: Vec<Middleware> = from_value(unwrap_key(body, "middleware"))?;
                let mut raw = state.active().raw.clone();
                let r = raw
                    .routes
                    .iter_mut()
                    .find(|r| r.path == route_path)
                    .ok_or_else(|| MoxyError::not_found(format!("no route {route_path}")))?;
                r.middleware = mw;
                return save(state, raw).await;
            }
            Err(MoxyError::not_found(format!("unknown API endpoint {}", req.path)))
        }
    }
}

fn only(method: &str, allowed: &[&str]) -> Result<(), MoxyError> {
    if allowed.contains(&method) {
        Ok(())
    } else {
        Err(MoxyError::method_not_allowed())
    }
}

/// Accept both `[...]` and `{"<key>": [...]}`.
fn unwrap_key(v: Value, key: &str) -> Value {
    match v {
        Value::Object(mut m) if m.contains_key(key) => m.remove(key).unwrap_or(Value::Null),
        other => other,
    }
}

fn from_value<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, MoxyError> {
    serde_json::from_value(v).map_err(|e| MoxyError::validation(vec![e.to_string()]))
}

async fn save(state: &Arc<AppState>, raw: Config) -> Result<Resp, MoxyError> {
    match state.write_config(&raw).await {
        Ok(_) => Ok(Resp::json(200, &config_json(state))),
        Err(errs) => Err(MoxyError::validation(errs)),
    }
}

