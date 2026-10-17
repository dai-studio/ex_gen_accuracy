//! Authentication: Moxy API keys, browser sessions, users and the OpenID Connect flow.

use crate::error::MoxyError;
use crate::http::{Req, Resp};
use crate::state::AppState;
use crate::store::{KvExt, KvStore};
use crate::util::{self, cookie_value, random_b64, random_hex, sha256_hex};
use chrono::{DateTime, Duration as CDuration, Utc};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const SESSION_COOKIE: &str = "moxy_session";
const LOGIN_STATE_TTL_MIN: i64 = 10;
const SESSION_REFRESH_AFTER: CDuration = CDuration::hours(1);
const KEY_LAST_USED_RESOLUTION: CDuration = CDuration::seconds(60);
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Via {
    Session { id: String, csrf: String },
    ApiKey { id: String },
}

/// An authenticated caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub sub: String,
    pub email: String,
    pub name: String,
    pub role: Role,
    pub via: Via,
}

impl Identity {
    /// Key used to separate cache entries between callers.
    pub fn caller_key(&self) -> String {
        match &self.via {
            Via::ApiKey { id } => format!("apikey:{id}"),
            Via::Session { .. } => format!("user:{}", self.sub),
        }
    }
    pub fn api_key_id(&self) -> Option<&str> {
        match &self.via {
            Via::ApiKey { id } => Some(id),
            Via::Session { .. } => None,
        }
    }
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRecord {
    pub sub: String,
    pub email: String,
    pub name: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    pub last_login_at: DateTime<Utc>,
    /// False only when the identity provider said the email is unverified.
    #[serde(default = "yes")]
    pub email_verified: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: String,
    pub user_sub: String,
    pub csrf_token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStateRecord {
    pub nonce: String,
    pub pkce_verifier: String,
    pub return_to: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyRecord {
    pub id: String,
    pub name: String,
    pub owner_sub: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub revoked: bool,
}

fn kv_err(e: String) -> MoxyError {
    log::error!("kv store error: {e}");
    MoxyError::internal("storage error")
}

/// Current role of a user, re-checked against the active config.
pub fn role_for(state: &AppState, email: &str, email_verified: bool) -> Role {
    if email_verified && state.active().resolved.is_admin_email(email) {
        Role::Admin
    } else {
        Role::User
    }
}

// ---------------------------------------------------------------- API keys

pub fn new_api_key(
    kv: &dyn KvStore,
    name: &str,
    owner_sub: &str,
) -> Result<(String, ApiKeyRecord), String> {
    let plaintext = format!("moxy_{}", random_b64(24));
    let rec = ApiKeyRecord {
        id: random_hex(8),
        name: name.to_string(),
        owner_sub: owner_sub.to_string(),
        created_at: Utc::now(),
        last_used_at: None,
        revoked: false,
    };
    kv.put(&format!("apikey#{}", sha256_hex(plaintext.as_bytes())), &rec)?;
    Ok((plaintext, rec))
}

pub fn list_api_keys(kv: &dyn KvStore) -> Result<Vec<ApiKeyRecord>, String> {
    let mut keys: Vec<ApiKeyRecord> = kv
        .scan_prefix("apikey#")?
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value(v).ok())
        .collect();
    keys.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(keys)
}

/// Mark a key revoked. Returns false if no key has this id.
pub fn revoke_api_key(kv: &dyn KvStore, id: &str) -> Result<bool, String> {
    for (k, v) in kv.scan_prefix("apikey#")? {
        if v.get("id").and_then(|i| i.as_str()) == Some(id) {
            kv.update(&k, &mut |cur| {
                cur.map(|mut v| {
                    v["revoked"] = serde_json::Value::Bool(true);
                    v
                })
            })?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn bearer(req_auth: Option<&str>) -> Option<&str> {
    let v = req_auth?.trim();
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Authenticate `Authorization: Bearer <moxy key>`.
/// Ok(None) when no bearer token is present; Err when it is present but invalid.
pub fn authenticate_api_key(
    state: &AppState,
    authorization: Option<&str>,
) -> Result<Option<Identity>, MoxyError> {
    let Some(token) = bearer(authorization) else {
        return Ok(None);
    };
    let key = format!("apikey#{}", sha256_hex(token.as_bytes()));
    let rec: ApiKeyRecord = match state.kv.get(&key).map_err(kv_err)? {
        Some(r) => r,
        None => return Err(MoxyError::unauthorized("invalid API key")),
    };
    if rec.revoked {
        return Err(MoxyError::unauthorized("API key has been revoked"));
    }
    let now = Utc::now();
    if rec
        .last_used_at
        .is_none_or(|t| now - t > KEY_LAST_USED_RESOLUTION)
    {
        let _ = state.kv.update(&key, &mut |cur| {
            cur.map(|mut v| {
                v["lastUsedAt"] = serde_json::json!(now);
                v
            })
        });
    }
    let owner: Option<UserRecord> = state
        .kv
        .get(&format!("user#{}", rec.owner_sub))
        .map_err(kv_err)?;
    let (email, name, verified) = owner
        .map(|u| (u.email.clone(), u.name.clone(), u.email_verified))
        .unwrap_or_else(|| (String::new(), rec.owner_sub.clone(), false));
    let role = role_for(state, &email, verified);
    Ok(Some(Identity {
        sub: rec.owner_sub,
        email,
        name,
        role,
        via: Via::ApiKey { id: rec.id },
    }))
}

// ---------------------------------------------------------------- sessions

pub fn authenticate_session(
    state: &AppState,
    cookie_header: Option<&str>,
) -> Result<Option<Identity>, MoxyError> {
    let Some(id) = cookie_header.and_then(|h| cookie_value(h, SESSION_COOKIE)) else {
        return Ok(None);
    };
    if id.is_empty() {
        return Ok(None);
    }
    let key = format!("session#{id}");
    let Some(sess): Option<SessionRecord> = state.kv.get(&key).map_err(kv_err)? else {
        return Ok(None);
    };
    let Some(user): Option<UserRecord> = state
        .kv
        .get(&format!("user#{}", sess.user_sub))
        .map_err(kv_err)?
    else {
        return Ok(None);
    };
    // Sliding expiry: extend when the last refresh is more than 1 h ago.
    let ttl = session_ttl(state);
    let now = Utc::now();
    let last_refresh = sess.expires_at - ttl;
    if now - last_refresh > SESSION_REFRESH_AFTER {
        let new_exp = now + ttl;
        let _ = state.kv.update(&key, &mut |cur| {
            cur.map(|mut v| {
                v["expiresAt"] = serde_json::json!(new_exp);
                v
            })
        });
    }
    let role = role_for(state, &user.email, user.email_verified);
    Ok(Some(Identity {
        sub: user.sub,
        email: user.email,
        name: user.name,
        role,
        via: Via::Session {
            id: sess.id,
            csrf: sess.csrf_token,
        },
    }))
}

fn session_ttl(state: &AppState) -> CDuration {
    let d = state
        .active()
        .resolved
        .auth
        .as_ref()
        .map(|a| a.session_ttl)
        .unwrap_or(crate::config::DEFAULT_SESSION_TTL);
    CDuration::from_std(d).unwrap_or(CDuration::hours(24))
}

/// Bearer API key first, then session cookie.
pub fn authenticate(state: &AppState, req: &Req) -> Result<Option<Identity>, MoxyError> {
    if let Some(id) = authenticate_api_key(state, req.header("authorization"))? {
        return Ok(Some(id));
    }
    authenticate_session(state, req.header("cookie"))
}

fn session_cookie(state: &AppState, value: &str, max_age: Option<i64>) -> String {
    let mut c = format!("{SESSION_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/");
    if let Some(m) = max_age {
        c.push_str(&format!("; Max-Age={m}"));
    }
    if state.secure_cookies() {
        c.push_str("; Secure");
    }
    c
}

/// Create a user record (or update it on re-login) and a new session.
/// Returns the Set-Cookie header value.
pub fn login_user(
    state: &AppState,
    sub: &str,
    email: &str,
    name: &str,
    email_verified: bool,
) -> Result<(String, SessionRecord), MoxyError> {
    let now = Utc::now();
    let role = role_for(state, email, email_verified);
    let ukey = format!("user#{sub}");
    let existing: Option<UserRecord> = state.kv.get(&ukey).map_err(kv_err)?;
    let user = UserRecord {
        sub: sub.to_string(),
        email: email.to_string(),
        name: name.to_string(),
        role,
        created_at: existing.map(|u| u.created_at).unwrap_or(now),
        last_login_at: now,
        email_verified,
    };
    state.kv.put(&ukey, &user).map_err(kv_err)?;
    let ttl = session_ttl(state);
    let sess = SessionRecord {
        id: random_b64(32),
        user_sub: sub.to_string(),
        csrf_token: random_b64(32),
        created_at: now,
        expires_at: now + ttl,
    };
    state
        .kv
        .put(&format!("session#{}", sess.id), &sess)
        .map_err(kv_err)?;
    let cookie = session_cookie(state, &sess.id, Some(ttl.num_seconds()));
    Ok((cookie, sess))
}

// ---------------------------------------------------------------- OIDC

#[derive(Default)]
pub struct OidcCache {
    meta: Mutex<Option<(String, Instant, CoreProviderMetadata)>>,
}

fn oidc_http() -> reqwest::Client {
    reqwest::Client::builder()
        // Following redirects opens the client up to SSRF.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .expect("oidc http client")
}

async fn provider_metadata(
    state: &AppState,
    issuer: &str,
) -> Result<CoreProviderMetadata, MoxyError> {
    if let Some((iss, at, meta)) = state.oidc.meta.lock().as_ref() {
        if iss == issuer && at.elapsed() < DISCOVERY_TTL {
            return Ok(meta.clone());
        }
    }
    let url = IssuerUrl::new(issuer.to_string())
        .map_err(|e| MoxyError::internal(format!("invalid issuer: {e}")))?;
    let meta = CoreProviderMetadata::discover_async(url, &oidc_http())
        .await
        .map_err(|e| {
            log::error!("OIDC discovery for {issuer} failed: {e}");
            MoxyError::new(
                502,
                "oidc_unavailable",
                "the identity provider could not be reached",
            )
        })?;
    *state.oidc.meta.lock() = Some((issuer.to_string(), Instant::now(), meta.clone()));
    Ok(meta)
}

fn redirect_uri(state: &AppState, req: &Req) -> String {
    let host = req.header("host").map(str::to_string).unwrap_or_else(|| {
        let r = &state.active().resolved;
        format!("{}:{}", r.host, r.port)
    });
    format!("http://{host}/auth/callback")
}

fn auth_settings(state: &AppState) -> Result<crate::config::AuthSettings, MoxyError> {
    state.active().resolved.auth.clone().ok_or_else(|| {
        MoxyError::new(
            503,
            "auth_not_configured",
            "sign-in is not configured (no <auth> in moxy.xml)",
        )
    })
}

/// GET /auth/login?return_to=<path>
pub async fn login(state: &Arc<AppState>, req: &Req) -> Resp {
    let result: Result<Resp, MoxyError> = async {
        let auth = auth_settings(state)?;
        let return_to = util::safe_return_to(util::query_param(&req.query, "return_to").as_deref());
        let meta = provider_metadata(state, &auth.issuer).await?;
        let client = CoreClient::from_provider_metadata(
            meta,
            ClientId::new(auth.client_id.clone()),
            auth.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri(state, req))
                .map_err(|e| MoxyError::bad_request(format!("bad Host header: {e}")))?,
        );
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, csrf, nonce) = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("email".into()))
            .add_scope(Scope::new("profile".into()))
            .set_pkce_challenge(challenge)
            .url();
        let rec = LoginStateRecord {
            nonce: nonce.secret().clone(),
            pkce_verifier: verifier.secret().clone(),
            return_to,
            expires_at: Utc::now() + CDuration::minutes(LOGIN_STATE_TTL_MIN),
        };
        state
            .kv
            .put(&format!("auth#state#{}", csrf.secret()), &rec)
            .map_err(kv_err)?;
        Ok(Resp::redirect(url.as_str()))
    }
    .await;
    result.unwrap_or_else(Resp::from)
}

/// GET /auth/callback?code=..&state=..
pub async fn callback(state: &Arc<AppState>, req: &Req) -> Resp {
    let result: Result<Resp, MoxyError> = async {
        let auth = auth_settings(state)?;
        let q = util::parse_query(&req.query);
        let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let invalid = || MoxyError::new(400, "invalid_state", "login state is invalid or expired");
        let st = get("state").ok_or_else(invalid)?;
        let skey = format!("auth#state#{st}");
        let rec: LoginStateRecord = state.kv.get(&skey).map_err(kv_err)?.ok_or_else(invalid)?;
        state.kv.delete(&skey).map_err(kv_err)?;
        if let Some(err) = get("error") {
            return Err(MoxyError::new(
                400,
                "oidc_error",
                format!("identity provider returned an error: {err}"),
            ));
        }
        let code = get("code").ok_or_else(|| MoxyError::bad_request("missing code"))?;

        let meta = provider_metadata(state, &auth.issuer).await?;
        let client = CoreClient::from_provider_metadata(
            meta,
            ClientId::new(auth.client_id.clone()),
            auth.client_secret.clone().map(ClientSecret::new),
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_uri(state, req))
                .map_err(|e| MoxyError::bad_request(format!("bad Host header: {e}")))?,
        );
        let oidc_fail = |what: &str, e: &dyn std::fmt::Display| {
            log::warn!("OIDC {what} failed: {e}");
            MoxyError::new(400, "oidc_error", format!("sign-in failed: {what}"))
        };
        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|e| oidc_fail("token endpoint missing", &e))?
            .set_pkce_verifier(PkceCodeVerifier::new(rec.pkce_verifier))
            .request_async(&oidc_http())
            .await
            .map_err(|e| oidc_fail("code exchange", &e))?;
        let id_token = token
            .id_token()
            .ok_or_else(|| MoxyError::new(400, "oidc_error", "no ID token returned"))?;
        let claims = id_token
            .claims(&client.id_token_verifier(), &Nonce::new(rec.nonce))
            .map_err(|e| oidc_fail("ID token verification", &e))?;
        let sub = claims.subject().as_str().to_string();
        let email = claims
            .email()
            .map(|e| e.as_str().to_string())
            .unwrap_or_default();
        // An explicitly unverified email never grants admin.
        let verified = claims.email_verified() != Some(false);
        let name = claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| email.clone());
        let (cookie, _) = login_user(state, &sub, &email, &name, verified)?;
        log::info!("user {email} ({sub}) signed in");
        Ok(Resp::redirect(&rec.return_to).header("set-cookie", cookie))
    }
    .await;
    result.unwrap_or_else(Resp::from)
}

/// POST /auth/logout
pub fn logout(state: &AppState, req: &Req) -> Resp {
    if let Some(id) = req.header("cookie").and_then(|h| cookie_value(h, SESSION_COOKIE)) {
        let _ = state.kv.delete(&format!("session#{id}"));
    }
    Resp::redirect("/").header("set-cookie", session_cookie(state, "", Some(0)))
}
