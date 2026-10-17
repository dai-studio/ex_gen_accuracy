//! `/auth/login`, `/auth/callback` and `/auth/logout`.

use super::{ApiRequest, ApiResponse};
use crate::auth::{self, Role, oidc::OidcError};
use crate::error::MoxyError;
use crate::state::AppState;
use crate::util::safe_return_to;
use http::{Method, StatusCode};
use std::sync::Arc;

pub async fn handle(state: &Arc<AppState>, req: &ApiRequest) -> Result<ApiResponse, MoxyError> {
    match (req.path.as_str(), &req.method) {
        ("/auth/login", &Method::GET) => login(state, req).await,
        ("/auth/callback", &Method::GET) => callback(state, req).await,
        ("/auth/logout", &Method::POST) => logout(state, req).await,
        ("/auth/login" | "/auth/callback" | "/auth/logout", _) => {
            Err(MoxyError::method_not_allowed())
        }
        _ => Err(MoxyError::route_not_found()),
    }
}

fn not_configured() -> MoxyError {
    MoxyError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "oidc_not_configured",
        "sign-in is not available because moxy.xml has no <auth> element",
    )
}

/// The callback URL registered with the identity provider, derived from the request.
pub fn redirect_uri(state: &AppState, req: &ApiRequest) -> String {
    let scheme = req
        .header("x-forwarded-proto")
        .map(|s| s.split(',').next().unwrap_or("http").trim().to_string())
        .filter(|s| s == "http" || s == "https")
        .unwrap_or_else(|| "http".into());
    let host = req.header("host").map(str::to_string).unwrap_or_else(|| {
        let cfg = state.config();
        format!("{}:{}", cfg.server.host, cfg.server.port)
    });
    format!("{scheme}://{host}/auth/callback")
}

fn oidc_error(e: OidcError) -> MoxyError {
    match e {
        OidcError::InvalidState => {
            MoxyError::new(StatusCode::BAD_REQUEST, "invalid_state", e.to_string())
        }
        OidcError::ProviderDenied(_) => {
            MoxyError::new(StatusCode::BAD_REQUEST, "sign_in_denied", e.to_string())
        }
        OidcError::InvalidToken(_) => {
            MoxyError::new(StatusCode::BAD_REQUEST, "invalid_id_token", e.to_string())
        }
        OidcError::Provider(_) => MoxyError::new(
            StatusCode::BAD_GATEWAY,
            "identity_provider_error",
            e.to_string(),
        ),
        OidcError::Internal(e) => e.into(),
    }
}

async fn login(state: &Arc<AppState>, req: &ApiRequest) -> Result<ApiResponse, MoxyError> {
    let config = state.config();
    let auth = config.auth.as_ref().ok_or_else(not_configured)?;
    let return_to = safe_return_to(req.query_param("return_to").as_deref());
    let url = state
        .oidc
        .begin_login(
            state.kv.as_ref(),
            auth,
            &redirect_uri(state, req),
            return_to,
        )
        .await
        .map_err(oidc_error)?;
    Ok(ApiResponse::redirect(&url))
}

async fn callback(state: &Arc<AppState>, req: &ApiRequest) -> Result<ApiResponse, MoxyError> {
    let config = state.config();
    let auth = config.auth.as_ref().ok_or_else(not_configured)?;
    let code = req.query_param("code");
    let st = req.query_param("state");
    let err = req.query_param("error");
    let identity = state
        .oidc
        .complete_login(
            state.kv.as_ref(),
            auth,
            &redirect_uri(state, req),
            st.as_deref(),
            code.as_deref(),
            err.as_deref(),
        )
        .await
        .map_err(oidc_error)?;

    let role = Role::for_email(&config, &identity.email);
    auth::upsert_user(
        state.kv.as_ref(),
        &identity.subject,
        &identity.email,
        &identity.name,
        role,
    )?;
    let session = auth::create_session(state.kv.as_ref(), &identity.subject, config.session_ttl())?;
    log::info!("{} signed in as {}", identity.email, role.as_str());
    let target = safe_return_to(identity.return_to.as_deref()).unwrap_or_else(|| "/".into());
    Ok(ApiResponse::redirect(&target).header(
        "set-cookie",
        &auth::session_cookie(&session.id, config.session_ttl(), !config.server.is_local()),
    ))
}

async fn logout(state: &Arc<AppState>, req: &ApiRequest) -> Result<ApiResponse, MoxyError> {
    let config = state.config();
    if let Some(id) = auth::session_id_from_headers(&req.headers)
        && let Some(session) = auth::load_session(state.kv.as_ref(), &id, config.session_ttl())?
    {
        let presented = req
            .header("x-csrf-token")
            .map(str::to_string)
            .or_else(|| form_field(&req.body, "csrf_token"));
        if presented.as_deref() != Some(session.csrf_token.as_str()) {
            return Err(super::handlers::csrf_failed());
        }
        auth::delete_session(state.kv.as_ref(), &id)?;
    }
    Ok(ApiResponse::redirect("/").header(
        "set-cookie",
        &auth::clear_session_cookie(!config.server.is_local()),
    ))
}

fn form_field(body: &[u8], name: &str) -> Option<String> {
    url::form_urlencoded::parse(body)
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}
