//! The HTML pages (`/`, `/chat`, `/admin`) and the embedded `/_app` assets.

use super::{ApiRequest, ApiResponse, Body};
use crate::auth::{self, Caller, Role};
use crate::error::MoxyError;
use crate::routing::Builtin;
use crate::state::AppState;
use crate::ui;
use bytes::Bytes;
use http::{Method, StatusCode};
use std::sync::Arc;

fn index_html() -> ApiResponse {
    ApiResponse::new(
        StatusCode::OK,
        Body::Full(Bytes::from_static(ui::INDEX_HTML)),
    )
    .header("content-type", "text/html; charset=utf-8")
    .header("cache-control", "no-store")
}

fn login_redirect(req: &ApiRequest) -> ApiResponse {
    let return_to: String =
        url::form_urlencoded::byte_serialize(req.path_and_query().as_bytes()).collect();
    ApiResponse::redirect(&format!("/auth/login?return_to={return_to}"))
}

pub async fn page(
    state: &Arc<AppState>,
    req: &ApiRequest,
    which: Builtin,
) -> Result<ApiResponse, MoxyError> {
    if req.method != Method::GET && req.method != Method::HEAD {
        return Err(MoxyError::method_not_allowed());
    }
    let needed = match which {
        Builtin::Root => {
            if req.path != "/" {
                return Err(MoxyError::route_not_found());
            }
            None
        }
        Builtin::Chat => Some(Role::User),
        _ => Some(Role::Admin),
    };
    let Some(needed) = needed else {
        return Ok(index_html());
    };
    let config = state.config();
    let caller = auth::authenticate_session(state.kv.as_ref(), &config, &req.headers)?;
    match caller {
        None => Ok(login_redirect(req)),
        Some(c @ Caller::Session { .. }) if c.role() >= Some(needed) => Ok(index_html()),
        Some(_) => Err(MoxyError::forbidden("the administrator role is required")),
    }
}

pub fn asset(req: &ApiRequest) -> Result<ApiResponse, MoxyError> {
    if req.method != Method::GET && req.method != Method::HEAD {
        return Err(MoxyError::method_not_allowed());
    }
    let Some(data) = ui::asset(&req.path) else {
        return Err(MoxyError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such asset",
        ));
    };
    let cache = if req.path.starts_with("/_app/immutable/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Ok(
        ApiResponse::new(StatusCode::OK, Body::Full(Bytes::from_static(data)))
            .header("content-type", ui::content_type(&req.path))
            .header("cache-control", cache),
    )
}
