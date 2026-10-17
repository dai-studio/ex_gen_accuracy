//! The JSON API under `/api`.

use super::{ApiRequest, ApiResponse};
use crate::auth::{self, Caller, Role};
use crate::config::doc::{AuthDoc, MiddlewareDoc, RouteDoc, ServerDoc};
use crate::config::{Config, ValidationError};
use crate::error::MoxyError;
use crate::state::AppState;
use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

pub fn csrf_failed() -> MoxyError {
    MoxyError::new(
        StatusCode::FORBIDDEN,
        "csrf_token_invalid",
        "requests authenticated by a session cookie must send a valid X-CSRF-Token header",
    )
}

/// Require at least `role`: 401 without credentials, 403 when the role is insufficient.
pub fn require(caller: &Caller, role: Role) -> Result<(), MoxyError> {
    match caller.role() {
        None => Err(MoxyError::unauthorized("authentication required")),
        Some(r) if r >= role => Ok(()),
        Some(_) => Err(MoxyError::forbidden("the administrator role is required")),
    }
}

/// State-changing requests authenticated by a session cookie must carry the CSRF token.
pub fn check_csrf(req: &ApiRequest, caller: &Caller) -> Result<(), MoxyError> {
    let safe = matches!(req.method, Method::GET | Method::HEAD | Method::OPTIONS);
    if let Caller::Session { session, .. } = caller
        && !safe
        && req.header("x-csrf-token") != Some(session.csrf_token.as_str())
    {
        return Err(csrf_failed());
    }
    Ok(())
}

fn parse_json<T: for<'de> Deserialize<'de>>(req: &ApiRequest) -> Result<T, MoxyError> {
    serde_json::from_slice(&req.body).map_err(|e| {
        MoxyError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            format!("invalid JSON body: {e}"),
        )
    })
}

fn validation_failed(errors: Vec<ValidationError>) -> MoxyError {
    MoxyError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_failed",
        crate::config::format_errors(&errors),
    )
    .with_details(errors)
}

pub async fn handle(state: &Arc<AppState>, req: ApiRequest) -> Result<ApiResponse, MoxyError> {
    let config = state.config();
    let path = req.path.trim_end_matches('/').to_string();
    let method = req.method.clone();

    // The endpoints resource is anonymous and needs no credential lookup.
    if path == "/api/endpoints" {
        return match method {
            Method::GET | Method::HEAD => {
                Ok(ApiResponse::json(StatusCode::OK, &endpoints(&config)))
            }
            _ => Err(MoxyError::method_not_allowed()),
        };
    }

    let caller = auth::authenticate_any(state.kv.as_ref(), &config, &req.headers)?;

    let key_id = path.strip_prefix("/api/keys/").map(str::to_string);
    match (path.as_str(), &method) {
        ("/api/me", &Method::GET) => {
            require(&caller, Role::User)?;
            Ok(ApiResponse::json(StatusCode::OK, &me(&caller)))
        }
        ("/api/chat", &Method::POST) => {
            require(&caller, Role::User)?;
            check_csrf(&req, &caller)?;
            super::chat::chat(state, &config, &caller, &req).await
        }
        ("/api/config", &Method::GET) => {
            require(&caller, Role::Admin)?;
            Ok(config_response(state))
        }
        ("/api/config/routes", &Method::PUT) => {
            require(&caller, Role::Admin)?;
            check_csrf(&req, &caller)?;
            let routes: RoutesBody = parse_json(&req)?;
            let mut doc = config.doc.clone();
            doc.routes = routes.into_vec();
            write(state, doc)
        }
        ("/api/config/system", &Method::PUT) => {
            require(&caller, Role::Admin)?;
            check_csrf(&req, &caller)?;
            let body: SystemBody = parse_json(&req)?;
            let mut doc = config.doc.clone();
            doc.server = body.server;
            doc.auth = body.auth;
            write(state, doc)
        }
        ("/api/config/middleware", &Method::PUT) => {
            require(&caller, Role::Admin)?;
            check_csrf(&req, &caller)?;
            let route_path = req.query_param("path").ok_or_else(|| {
                MoxyError::bad_request("the 'path' query parameter names the route")
            })?;
            let middleware: MiddlewareBody = parse_json(&req)?;
            let mut doc = config.doc.clone();
            let route = doc
                .routes
                .iter_mut()
                .find(|r| r.path == route_path)
                .ok_or_else(MoxyError::route_not_found)?;
            route.middleware = middleware.into_vec();
            write(state, doc)
        }
        ("/api/keys", &Method::GET) => {
            require(&caller, Role::Admin)?;
            let keys = auth::list_api_keys(state.kv.as_ref())?;
            Ok(ApiResponse::json(StatusCode::OK, &json!({ "keys": keys })))
        }
        ("/api/keys", &Method::POST) => {
            require(&caller, Role::Admin)?;
            check_csrf(&req, &caller)?;
            let body: NewKey = parse_json(&req)?;
            let name = body.name.trim();
            if name.is_empty() {
                return Err(MoxyError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_request",
                    "a key needs a name",
                ));
            }
            let Caller::Session { user, .. } = &caller else {
                return Err(MoxyError::forbidden(
                    "API keys are created by signed-in administrators",
                ));
            };
            let (plaintext, record) = auth::create_api_key(state.kv.as_ref(), name, &user.subject)?;
            Ok(ApiResponse::json(
                StatusCode::CREATED,
                &CreatedKey {
                    key: plaintext,
                    record,
                },
            ))
        }
        (_, &Method::DELETE)
            if key_id
                .as_deref()
                .is_some_and(|id| !id.is_empty() && !id.contains('/')) =>
        {
            require(&caller, Role::Admin)?;
            check_csrf(&req, &caller)?;
            let id = key_id.unwrap();
            if auth::revoke_api_key(state.kv.as_ref(), &id)? {
                Ok(ApiResponse::empty(StatusCode::NO_CONTENT))
            } else {
                Err(MoxyError::new(
                    StatusCode::NOT_FOUND,
                    "key_not_found",
                    "no API key with this id",
                ))
            }
        }
        (
            "/api/me"
            | "/api/chat"
            | "/api/config"
            | "/api/config/routes"
            | "/api/config/system"
            | "/api/config/middleware"
            | "/api/keys",
            _,
        ) => Err(MoxyError::method_not_allowed()),
        _ if key_id.is_some() => Err(MoxyError::method_not_allowed()),
        _ => Err(MoxyError::route_not_found()),
    }
}

/// Public description of a proxy route: never upstreams, headers or credentials.
#[derive(Debug, Serialize)]
pub struct EndpointInfo {
    pub path: String,
    pub description: String,
    pub auth: &'static str,
    /// Whether the route has an `<ai>` element (and so appears in the chat interface).
    pub chat: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

pub fn endpoints(config: &Config) -> serde_json::Value {
    let list: Vec<EndpointInfo> = config
        .routes
        .iter()
        .map(|r| EndpointInfo {
            path: r.path().to_string(),
            description: r.description.clone(),
            auth: r.auth.as_str(),
            chat: r.ai().is_some(),
            provider: r.ai().map(|a| a.provider.as_str()),
            model: r.ai().and_then(|a| a.model.clone()),
        })
        .collect();
    json!({ "endpoints": list })
}

fn me(caller: &Caller) -> serde_json::Value {
    match caller {
        Caller::Session {
            user,
            session,
            role,
        } => json!({
            "subject": user.subject,
            "email": user.email,
            "name": user.name,
            "role": role.as_str(),
            "auth": "session",
            "csrf_token": session.csrf_token,
            "session_expires_at": session.expires_at,
        }),
        Caller::ApiKey(key) => json!({
            "subject": key.owner,
            "role": Role::User.as_str(),
            "auth": "api-key",
            "key": { "id": key.id, "name": key.name },
        }),
        Caller::Anonymous => json!(null),
    }
}

fn config_response(state: &AppState) -> ApiResponse {
    let config = state.config();
    ApiResponse::json(
        StatusCode::OK,
        &json!({ "status": state.status(), "config": config.doc }),
    )
}

fn write(state: &Arc<AppState>, doc: crate::config::ConfigDoc) -> Result<ApiResponse, MoxyError> {
    match state.write_config(doc)? {
        Ok(()) => Ok(config_response(state)),
        Err(errors) => Err(validation_failed(errors)),
    }
}

/// Accepts either a bare array or `{"routes": [...]}`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RoutesBody {
    List(Vec<RouteDoc>),
    Wrapped { routes: Vec<RouteDoc> },
}

impl RoutesBody {
    fn into_vec(self) -> Vec<RouteDoc> {
        match self {
            RoutesBody::List(v) | RoutesBody::Wrapped { routes: v } => v,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MiddlewareBody {
    List(Vec<MiddlewareDoc>),
    Wrapped { middleware: Vec<MiddlewareDoc> },
}

impl MiddlewareBody {
    fn into_vec(self) -> Vec<MiddlewareDoc> {
        match self {
            MiddlewareBody::List(v) | MiddlewareBody::Wrapped { middleware: v } => v,
        }
    }
}

#[derive(Deserialize)]
struct SystemBody {
    #[serde(default)]
    server: Option<ServerDoc>,
    #[serde(default)]
    auth: Option<AuthDoc>,
}

#[derive(Deserialize)]
struct NewKey {
    name: String,
}

#[derive(Serialize)]
struct CreatedKey {
    /// The plaintext key, returned only in this response.
    key: String,
    #[serde(flatten)]
    record: auth::ApiKeyRecord,
}
