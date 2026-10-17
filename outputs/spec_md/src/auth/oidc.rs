//! OpenID Connect sign-in: authorization code flow with PKCE.

use super::{LOGIN_STATE_TTL, LoginState, state_key};
use crate::config::AuthConfig;
use crate::kv::{KvStore, KvStoreExt};
use crate::util::{random_token, sha256_b64url};
use chrono::Utc;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;

const DISCOVERY_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

/// The identity established by a completed sign-in.
#[derive(Debug, Clone)]
pub struct Identity {
    pub subject: String,
    pub email: String,
    pub name: String,
    pub return_to: Option<String>,
}

#[derive(Debug)]
pub enum OidcError {
    /// Missing, unknown or expired `state`.
    InvalidState,
    /// The provider sent the browser back with an error.
    ProviderDenied(String),
    /// The ID token or its claims failed validation.
    InvalidToken(String),
    /// The provider could not be reached or answered unexpectedly.
    Provider(String),
    Internal(anyhow::Error),
}

impl std::fmt::Display for OidcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OidcError::InvalidState => write!(f, "the sign-in state is invalid or has expired"),
            OidcError::ProviderDenied(e) => {
                write!(f, "the identity provider returned an error: {e}")
            }
            OidcError::InvalidToken(e) => write!(f, "the ID token is invalid: {e}"),
            OidcError::Provider(e) => write!(f, "identity provider error: {e}"),
            OidcError::Internal(e) => write!(f, "internal error: {e}"),
        }
    }
}

impl From<anyhow::Error> for OidcError {
    fn from(e: anyhow::Error) -> Self {
        OidcError::Internal(e)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
}

#[derive(Deserialize)]
struct UserInfo {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

pub struct OidcClient {
    http: reqwest::Client,
    discovery: Mutex<HashMap<String, (Discovery, Instant)>>,
    jwks: Mutex<HashMap<String, (JwkSet, Instant)>>,
}

impl OidcClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            discovery: Mutex::new(HashMap::new()),
            jwks: Mutex::new(HashMap::new()),
        }
    }

    pub async fn discover(&self, issuer: &str) -> Result<Discovery, OidcError> {
        if let Some((d, at)) = self.discovery.lock().unwrap().get(issuer)
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(d.clone());
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let d: Discovery = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| OidcError::Provider(format!("discovery failed: {e}")))?
            .json()
            .await
            .map_err(|e| OidcError::Provider(format!("invalid discovery document: {e}")))?;
        self.discovery
            .lock()
            .unwrap()
            .insert(issuer.to_string(), (d.clone(), Instant::now()));
        Ok(d)
    }

    async fn jwks(&self, uri: &str, force: bool) -> Result<JwkSet, OidcError> {
        if !force
            && let Some((set, at)) = self.jwks.lock().unwrap().get(uri)
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(set.clone());
        }
        let set: JwkSet = self
            .http
            .get(uri)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| OidcError::Provider(format!("fetching JWKS failed: {e}")))?
            .json()
            .await
            .map_err(|e| OidcError::Provider(format!("invalid JWKS: {e}")))?;
        self.jwks
            .lock()
            .unwrap()
            .insert(uri.to_string(), (set.clone(), Instant::now()));
        Ok(set)
    }

    /// Create a login state record and return the provider URL to redirect to.
    pub async fn begin_login(
        &self,
        kv: &dyn KvStore,
        auth: &AuthConfig,
        redirect_uri: &str,
        return_to: Option<String>,
    ) -> Result<String, OidcError> {
        let discovery = self.discover(&auth.issuer).await?;
        let state = random_token(24);
        let nonce = random_token(24);
        let verifier = random_token(48);
        let record = LoginState {
            nonce: nonce.clone(),
            pkce_verifier: verifier.clone(),
            return_to,
            expires_at: Utc::now() + LOGIN_STATE_TTL,
        };
        kv.put_json(&state_key(&state), &record, Some(record.expires_at))?;

        let mut url = Url::parse(&discovery.authorization_endpoint)
            .map_err(|e| OidcError::Provider(format!("invalid authorization endpoint: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &auth.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid email profile")
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &sha256_b64url(verifier.as_bytes()))
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    /// Validate the callback, exchange the code and verify the ID token.
    pub async fn complete_login(
        &self,
        kv: &dyn KvStore,
        auth: &AuthConfig,
        redirect_uri: &str,
        state: Option<&str>,
        code: Option<&str>,
        provider_error: Option<&str>,
    ) -> Result<Identity, OidcError> {
        let state = state
            .filter(|s| !s.is_empty())
            .ok_or(OidcError::InvalidState)?;
        // The state record is single use: it is removed whether or not the rest succeeds.
        let record: LoginState = match kv.take(&state_key(state))? {
            Some(v) => serde_json::from_value(v).map_err(|e| OidcError::Internal(e.into()))?,
            None => return Err(OidcError::InvalidState),
        };
        if record.expires_at <= Utc::now() {
            return Err(OidcError::InvalidState);
        }
        if let Some(err) = provider_error {
            return Err(OidcError::ProviderDenied(err.to_string()));
        }
        let code = code
            .filter(|c| !c.is_empty())
            .ok_or_else(|| OidcError::ProviderDenied("no authorization code".into()))?;

        let discovery = self.discover(&auth.issuer).await?;
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", redirect_uri.to_string()),
            ("code_verifier", record.pkce_verifier.clone()),
            ("client_id", auth.client_id.clone()),
        ];
        let mut req = self.http.post(&discovery.token_endpoint);
        if let Some(secret) = &auth.client_secret {
            let methods = discovery.token_endpoint_auth_methods_supported.as_deref();
            let basic = methods.is_none_or(|m| m.iter().any(|x| x == "client_secret_basic"));
            if basic {
                req = req.basic_auth(enc(&auth.client_id), Some(enc(secret)));
            } else {
                form.push(("client_secret", secret.clone()));
            }
        }
        let resp = req
            .form(&form)
            .send()
            .await
            .map_err(|e| OidcError::Provider(format!("token request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OidcError::Provider(format!(
                "token endpoint returned {status}: {}",
                body.chars().take(300).collect::<String>()
            )));
        }
        let tokens: TokenResponse = resp
            .json()
            .await
            .map_err(|e| OidcError::Provider(format!("invalid token response: {e}")))?;
        let id_token = tokens
            .id_token
            .ok_or_else(|| OidcError::InvalidToken("no id_token in token response".into()))?;

        let claims = self.verify_id_token(&discovery, auth, &id_token).await?;
        if claims.nonce.as_deref() != Some(record.nonce.as_str()) {
            return Err(OidcError::InvalidToken("nonce mismatch".into()));
        }

        let mut email = claims.email.clone();
        let mut name = claims.name.clone().or(claims.preferred_username.clone());
        if email.is_none()
            && let (Some(endpoint), Some(token)) =
                (&discovery.userinfo_endpoint, &tokens.access_token)
            && let Ok(resp) = self.http.get(endpoint).bearer_auth(token).send().await
            && let Ok(info) = resp.json::<UserInfo>().await
        {
            email = info.email;
            name = name.or(info.name);
        }
        let email = email.ok_or_else(|| {
            OidcError::InvalidToken("the identity provider did not supply an email address".into())
        })?;
        Ok(Identity {
            subject: claims.sub,
            name: name.unwrap_or_else(|| email.clone()),
            email,
            return_to: record.return_to,
        })
    }

    async fn verify_id_token(
        &self,
        discovery: &Discovery,
        auth: &AuthConfig,
        token: &str,
    ) -> Result<IdClaims, OidcError> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| OidcError::InvalidToken(e.to_string()))?;
        let key = match header.alg {
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512 => {
                let secret = auth.client_secret.as_ref().ok_or_else(|| {
                    OidcError::InvalidToken("HMAC-signed ID token but no client secret".into())
                })?;
                DecodingKey::from_secret(secret.as_bytes())
            }
            _ => {
                let mut set = self.jwks(&discovery.jwks_uri, false).await?;
                let find = |set: &JwkSet| match &header.kid {
                    Some(kid) => set.find(kid).cloned(),
                    None => set.keys.first().cloned(),
                };
                let jwk = match find(&set) {
                    Some(j) => j,
                    None => {
                        // the provider may have rotated its keys
                        set = self.jwks(&discovery.jwks_uri, true).await?;
                        find(&set).ok_or_else(|| {
                            OidcError::InvalidToken("no matching signing key".into())
                        })?
                    }
                };
                DecodingKey::from_jwk(&jwk).map_err(|e| OidcError::InvalidToken(e.to_string()))?
            }
        };
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&discovery.issuer]);
        validation.set_audience(&[&auth.client_id]);
        validation.leeway = 60;
        let data = jsonwebtoken::decode::<IdClaims>(token, &key, &validation)
            .map_err(|e| OidcError::InvalidToken(e.to_string()))?;
        Ok(data.claims)
    }
}

/// Form-urlencode a client credential for HTTP Basic, as RFC 6749 section 2.3.1 requires.
fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
