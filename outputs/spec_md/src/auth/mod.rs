//! Users, sessions, login state, API keys and caller identity.

pub mod oidc;

use crate::config::{Config, CountKey};
use crate::kv::{KvStore, KvStoreExt};
use crate::util::{bearer_token, cookie_value, random_token, sha256_hex};
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::Duration;

pub const SESSION_COOKIE: &str = "moxy_session";
pub const API_KEY_PREFIX: &str = "moxy_";
/// Sessions are refreshed (sliding expiry) at most this often.
pub const SESSION_REFRESH_INTERVAL: ChronoDuration = ChronoDuration::hours(1);
/// Login state records expire after this long.
pub const LOGIN_STATE_TTL: ChronoDuration = ChronoDuration::minutes(10);
/// `last_used_at` of an API key is written at most this often.
const KEY_TOUCH_INTERVAL: ChronoDuration = ChronoDuration::seconds(60);

pub fn user_key(subject: &str) -> String {
    format!("user#{subject}")
}
pub fn session_key(id: &str) -> String {
    format!("session#{id}")
}
pub fn state_key(state: &str) -> String {
    format!("auth#state#{state}")
}
pub fn apikey_key(hash: &str) -> String {
    format!("apikey#{hash}")
}

fn chrono_dur(d: Duration) -> ChronoDuration {
    ChronoDuration::from_std(d).unwrap_or(ChronoDuration::hours(24))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Admin,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Admin => "admin",
        }
    }

    pub fn for_email(config: &Config, email: &str) -> Role {
        if config.is_admin_email(email) {
            Role::Admin
        } else {
            Role::User
        }
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub subject: String,
    pub email: String,
    pub name: String,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub last_login_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub subject: String,
    pub csrf_token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// When the sliding expiry was last extended.
    #[serde(default)]
    pub refreshed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginState {
    pub nonce: String,
    pub pkce_verifier: String,
    pub return_to: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: String,
    pub name: String,
    /// Subject identifier of the administrator who created the key.
    pub owner: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked: bool,
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

/// Create the user's record on first sign-in, or update it on later ones.
pub fn upsert_user(
    kv: &dyn KvStore,
    subject: &str,
    email: &str,
    name: &str,
    role: Role,
) -> Result<User> {
    let now = Utc::now();
    let existing: Option<User> = kv.get_json(&user_key(subject))?;
    let user = User {
        subject: subject.to_string(),
        email: email.to_string(),
        name: name.to_string(),
        role: role.as_str().to_string(),
        created_at: existing.map(|u| u.created_at).unwrap_or(now),
        last_login_at: now,
    };
    kv.put_json(&user_key(subject), &user, None)?;
    Ok(user)
}

pub fn get_user(kv: &dyn KvStore, subject: &str) -> Result<Option<User>> {
    kv.get_json(&user_key(subject))
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

pub fn create_session(kv: &dyn KvStore, subject: &str, ttl: Duration) -> Result<SessionRecord> {
    let now = Utc::now();
    let session = SessionRecord {
        id: random_token(32),
        subject: subject.to_string(),
        csrf_token: random_token(32),
        created_at: now,
        expires_at: now + chrono_dur(ttl),
        refreshed_at: Some(now),
    };
    kv.put_json(
        &session_key(&session.id),
        &session,
        Some(session.expires_at),
    )?;
    Ok(session)
}

/// Load a live session, extending its expiry when it was last refreshed more than an hour ago.
pub fn load_session(kv: &dyn KvStore, id: &str, ttl: Duration) -> Result<Option<SessionRecord>> {
    let Some(mut session) = kv.get_json::<SessionRecord>(&session_key(id))? else {
        return Ok(None);
    };
    let now = Utc::now();
    if session.expires_at <= now {
        return Ok(None);
    }
    let last = session.refreshed_at.unwrap_or(session.created_at);
    if now - last > SESSION_REFRESH_INTERVAL {
        session.expires_at = now + chrono_dur(ttl);
        session.refreshed_at = Some(now);
        kv.put_json(&session_key(id), &session, Some(session.expires_at))?;
    }
    Ok(Some(session))
}

pub fn delete_session(kv: &dyn KvStore, id: &str) -> Result<()> {
    kv.delete(&session_key(id))?;
    Ok(())
}

/// `Set-Cookie` value for a new session.
pub fn session_cookie(id: &str, ttl: Duration, secure: bool) -> String {
    let mut c = format!(
        "{SESSION_COOKIE}={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        ttl.as_secs()
    );
    if secure {
        c.push_str("; Secure");
    }
    c
}

/// `Set-Cookie` value that clears the session cookie.
pub fn clear_session_cookie(secure: bool) -> String {
    let mut c = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        c.push_str("; Secure");
    }
    c
}

pub fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| cookie_value(h, SESSION_COOKIE).map(str::to_string))
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

/// A new plaintext key: `moxy_` followed by 32 URL-safe base64 characters.
pub fn generate_api_key() -> String {
    // 24 random bytes encode to exactly 32 base64 characters.
    format!("{API_KEY_PREFIX}{}", random_token(24))
}

pub fn hash_api_key(plaintext: &str) -> String {
    sha256_hex(plaintext.as_bytes())
}

/// Create and store a key. The plaintext is returned once and never stored.
pub fn create_api_key(kv: &dyn KvStore, name: &str, owner: &str) -> Result<(String, ApiKeyRecord)> {
    let plaintext = generate_api_key();
    let record = ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        owner: owner.to_string(),
        created_at: Utc::now(),
        last_used_at: None,
        revoked: false,
    };
    kv.put_json(&apikey_key(&hash_api_key(&plaintext)), &record, None)?;
    Ok((plaintext, record))
}

/// Look up a presented key; returns it if it exists and is not revoked.
pub fn verify_api_key(kv: &dyn KvStore, plaintext: &str) -> Result<Option<ApiKeyRecord>> {
    if !plaintext.starts_with(API_KEY_PREFIX) {
        return Ok(None);
    }
    let key = apikey_key(&hash_api_key(plaintext));
    let Some(mut record) = kv.get_json::<ApiKeyRecord>(&key)? else {
        return Ok(None);
    };
    if record.revoked {
        return Ok(None);
    }
    let now = Utc::now();
    if record
        .last_used_at
        .is_none_or(|t| now - t > KEY_TOUCH_INTERVAL)
    {
        record.last_used_at = Some(now);
        kv.put_json(&key, &record, None)?;
    }
    Ok(Some(record))
}

pub fn list_api_keys(kv: &dyn KvStore) -> Result<Vec<ApiKeyRecord>> {
    let mut keys: Vec<ApiKeyRecord> = kv
        .scan_prefix("apikey#")?
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value(v).ok())
        .collect();
    keys.sort_by_key(|k| k.created_at);
    Ok(keys)
}

/// Mark the key with the given id as revoked. Returns false if no such key exists.
pub fn revoke_api_key(kv: &dyn KvStore, id: &str) -> Result<bool> {
    for (k, v) in kv.scan_prefix("apikey#")? {
        let Ok(mut record) = serde_json::from_value::<ApiKeyRecord>(v) else {
            continue;
        };
        if record.id == id {
            record.revoked = true;
            kv.put_json(&k, &record, None)?;
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Caller identity
// ---------------------------------------------------------------------------

/// Who is making a request.
#[derive(Debug, Clone)]
pub enum Caller {
    Anonymous,
    ApiKey(ApiKeyRecord),
    Session {
        user: User,
        session: SessionRecord,
        role: Role,
    },
}

impl Caller {
    /// The caller's role, if authenticated. API keys act with the user role.
    pub fn role(&self) -> Option<Role> {
        match self {
            Caller::Anonymous => None,
            Caller::ApiKey(_) => Some(Role::User),
            Caller::Session { role, .. } => Some(*role),
        }
    }

    pub fn is_session(&self) -> bool {
        matches!(self, Caller::Session { .. })
    }

    /// The identity used for rate limit and token budget counting.
    pub fn count_identity(&self, key: CountKey, ip: Option<IpAddr>) -> String {
        let ip = || {
            format!(
                "ip:{}",
                ip.map(|i| i.to_string())
                    .unwrap_or_else(|| "unknown".into())
            )
        };
        match (key, self) {
            (CountKey::Ip, _) | (_, Caller::Anonymous) => ip(),
            (CountKey::ApiKey, Caller::ApiKey(k)) => k.id.clone(),
            (CountKey::User, Caller::ApiKey(k)) => k.owner.clone(),
            (_, Caller::Session { user, .. }) => user.subject.clone(),
        }
    }

    /// The caller component of a cache key.
    pub fn cache_identity(&self) -> String {
        match self {
            Caller::Anonymous => "anonymous".into(),
            Caller::ApiKey(k) => format!("apikey:{}", k.id),
            Caller::Session { user, .. } => format!("user:{}", user.subject),
        }
    }
}

/// Authenticate a bearer API key from the `Authorization` header.
pub fn authenticate_api_key(kv: &dyn KvStore, headers: &HeaderMap) -> Result<Option<Caller>> {
    let Some(token) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token)
    else {
        return Ok(None);
    };
    Ok(verify_api_key(kv, token)?.map(Caller::ApiKey))
}

/// Authenticate the session cookie; the role is recomputed from the current configuration.
pub fn authenticate_session(
    kv: &dyn KvStore,
    config: &Config,
    headers: &HeaderMap,
) -> Result<Option<Caller>> {
    let Some(id) = session_id_from_headers(headers) else {
        return Ok(None);
    };
    let Some(session) = load_session(kv, &id, config.session_ttl())? else {
        return Ok(None);
    };
    let Some(user) = get_user(kv, &session.subject)? else {
        return Ok(None);
    };
    let role = Role::for_email(config, &user.email);
    Ok(Some(Caller::Session {
        user,
        session,
        role,
    }))
}

/// Session cookie first, then bearer API key (used by the JSON API).
pub fn authenticate_any(kv: &dyn KvStore, config: &Config, headers: &HeaderMap) -> Result<Caller> {
    if let Some(c) = authenticate_session(kv, config, headers)? {
        return Ok(c);
    }
    if let Some(c) = authenticate_api_key(kv, headers)? {
        return Ok(c);
    }
    Ok(Caller::Anonymous)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_str;
    use crate::kv::MemoryStore;

    fn config() -> Config {
        load_str(
            r#"<moxy version="1"><auth issuer="https://idp" client-id="c"><admin email="boss@example.com"/></auth></moxy>"#,
            &|_| None,
        )
        .unwrap()
    }

    #[test]
    fn api_key_lifecycle() {
        let kv = MemoryStore::new();
        let (plain, rec) = create_api_key(&kv, "agent", "sub-1").unwrap();
        assert!(plain.starts_with("moxy_"));
        assert_eq!(plain.len(), 5 + 32);
        // only the hash is stored
        let stored = kv.scan_prefix("apikey#").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0, format!("apikey#{}", hash_api_key(&plain)));
        assert!(!stored[0].1.to_string().contains(&plain));

        let found = verify_api_key(&kv, &plain).unwrap().unwrap();
        assert_eq!(found.id, rec.id);
        assert!(list_api_keys(&kv).unwrap()[0].last_used_at.is_some());
        assert!(verify_api_key(&kv, "moxy_wrong").unwrap().is_none());
        assert!(verify_api_key(&kv, "sk-other").unwrap().is_none());

        assert!(revoke_api_key(&kv, &rec.id).unwrap());
        assert!(verify_api_key(&kv, &plain).unwrap().is_none());
        assert!(!revoke_api_key(&kv, "nope").unwrap());
    }

    #[test]
    fn session_lifecycle_and_roles() {
        let kv = MemoryStore::new();
        let cfg = config();
        upsert_user(&kv, "s1", "boss@example.com", "Boss", Role::Admin).unwrap();
        upsert_user(&kv, "s2", "dev@example.com", "Dev", Role::User).unwrap();
        let s1 = create_session(&kv, "s1", Duration::from_secs(3600)).unwrap();
        let s2 = create_session(&kv, "s2", Duration::from_secs(3600)).unwrap();
        assert_eq!(s1.id.len(), 43);

        let mut h = HeaderMap::new();
        h.insert(
            "cookie",
            format!("other=1; moxy_session={}", s1.id).parse().unwrap(),
        );
        let c = authenticate_session(&kv, &cfg, &h).unwrap().unwrap();
        assert_eq!(c.role(), Some(Role::Admin));

        h.insert("cookie", format!("moxy_session={}", s2.id).parse().unwrap());
        let c = authenticate_session(&kv, &cfg, &h).unwrap().unwrap();
        assert_eq!(c.role(), Some(Role::User));

        delete_session(&kv, &s2.id).unwrap();
        assert!(authenticate_session(&kv, &cfg, &h).unwrap().is_none());
    }

    #[test]
    fn sliding_expiry() {
        let kv = MemoryStore::new();
        let mut s = create_session(&kv, "s1", Duration::from_secs(7200)).unwrap();
        let old_exp = s.expires_at;
        s.refreshed_at = Some(Utc::now() - ChronoDuration::hours(2));
        s.expires_at = Utc::now() + ChronoDuration::minutes(5);
        kv.put_json(&session_key(&s.id), &s, Some(s.expires_at))
            .unwrap();
        let loaded = load_session(&kv, &s.id, Duration::from_secs(7200))
            .unwrap()
            .unwrap();
        assert!(loaded.expires_at >= old_exp - ChronoDuration::seconds(5));
    }

    #[test]
    fn count_identities() {
        let key = ApiKeyRecord {
            id: "k1".into(),
            name: "n".into(),
            owner: "owner-sub".into(),
            created_at: Utc::now(),
            last_used_at: None,
            revoked: false,
        };
        let c = Caller::ApiKey(key);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(c.count_identity(CountKey::ApiKey, Some(ip)), "k1");
        assert_eq!(c.count_identity(CountKey::User, Some(ip)), "owner-sub");
        assert_eq!(c.count_identity(CountKey::Ip, Some(ip)), "ip:10.0.0.1");
        assert_eq!(
            Caller::Anonymous.count_identity(CountKey::ApiKey, Some(ip)),
            "ip:10.0.0.1"
        );
        assert_eq!(c.cache_identity(), "apikey:k1");
    }

    #[test]
    fn cookies() {
        let c = session_cookie("abc", Duration::from_secs(60), false);
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax") && c.contains("Path=/"));
        assert!(!c.contains("Secure"));
        assert!(session_cookie("abc", Duration::from_secs(60), true).ends_with("; Secure"));
        assert!(clear_session_cookie(false).contains("Max-Age=0"));
    }
}
