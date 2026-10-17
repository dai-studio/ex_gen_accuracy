//! Raw [`Config`] -> validated, typed [`Resolved`] config.
//!
//! `${NAME}` references are substituted from the environment here; a missing variable
//! is a validation error.

use super::model::*;
use crate::util::{parse_duration, sha256_hex};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub const RESERVED_PREFIXES: &[&str] = &["/chat", "/admin", "/auth", "/api", "/_app"];
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone)]
pub struct Resolved {
    pub host: String,
    pub port: u16,
    pub auth: Option<AuthSettings>,
    pub routes: Vec<Arc<RouteSpec>>,
}

#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub session_ttl: Duration,
    /// Lower-cased admin emails.
    pub admins: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    None,
    ApiKey,
    Session,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::None => "none",
            AuthMode::ApiKey => "api-key",
            AuthMode::Session => "session",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Anthropic,
    Ollama,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiSpec {
    pub provider: Provider,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderOpR {
    Set(String, String),
    Add(String, String),
    Remove(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqOpR {
    RewritePath { from: String, to: String },
    MaxBodySize(u64),
    Timeout(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespOpR {
    Header(HeaderOpR),
    Cache(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKey {
    ApiKey,
    User,
    Ip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitSpec {
    /// Index of the middleware element within the route (distinguishes counters).
    pub id: usize,
    pub requests: u64,
    pub per: Duration,
    pub key: LimitKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBudgetSpec {
    pub id: usize,
    pub tokens: u64,
    pub per: Duration,
    pub key: LimitKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mw {
    Headers(Vec<HeaderOpR>),
    Request(Vec<ReqOpR>),
    Response(Vec<RespOpR>),
    Ai(AiSpec),
    RateLimit(RateLimitSpec),
    TokenBudget(TokenBudgetSpec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Single,
    RoundRobin,
    Random,
    Failover,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamSpec {
    pub strategy: Strategy,
    pub urls: Vec<Url>,
}

#[derive(Debug, Clone)]
pub struct RouteSpec {
    pub index: usize,
    /// The path as written in moxy.xml, e.g. `/openai/*`.
    pub path: String,
    /// For wildcard routes the literal prefix without `*` (e.g. `/openai/`); for exact routes the path.
    pub base: String,
    pub wildcard: bool,
    pub auth: AuthMode,
    pub description: String,
    pub middleware: Vec<Mw>,
    pub upstream: UpstreamSpec,
    pub ai: Option<AiSpec>,
    pub cache_ttl: Option<Duration>,
    pub has_token_budget: bool,
    /// Hash of the resolved route definition; used to detect changes on reload.
    pub fingerprint: String,
}

impl RouteSpec {
    /// If this route matches `path`, the matched length and the part matched by `*`.
    pub fn matches(&self, path: &str) -> Option<(usize, String)> {
        if self.wildcard {
            let stem = &self.base[..self.base.len() - 1]; // without trailing '/'
            if path == stem {
                Some((stem.len(), String::new()))
            } else if path.starts_with(&self.base) {
                Some((stem.len(), path[stem.len()..].to_string()))
            } else {
                None
            }
        } else if path == self.base {
            Some((self.base.len(), String::new()))
        } else {
            None
        }
    }
}

impl Resolved {
    /// Longest matching prefix wins; ties go to the route declared first.
    pub fn match_route(&self, path: &str) -> Option<(Arc<RouteSpec>, String)> {
        let mut best: Option<(usize, &Arc<RouteSpec>, String)> = None;
        for r in &self.routes {
            if let Some((len, suffix)) = r.matches(path) {
                if best.as_ref().is_none_or(|(l, _, _)| len > *l) {
                    best = Some((len, r, suffix));
                }
            }
        }
        best.map(|(_, r, s)| (r.clone(), s))
    }

    pub fn route_by_path(&self, path: &str) -> Option<Arc<RouteSpec>> {
        self.routes.iter().find(|r| r.path == path).cloned()
    }

    pub fn is_admin_email(&self, email: &str) -> bool {
        let email = email.to_ascii_lowercase();
        self.auth
            .as_ref()
            .is_some_and(|a| a.admins.iter().any(|e| *e == email))
    }
}

/// Is `path` (an exact path or a wildcard base without `/*`) reserved for built-in routes?
pub fn is_reserved(stem: &str) -> bool {
    stem == "/"
        || RESERVED_PREFIXES
            .iter()
            .any(|r| stem == *r || stem.starts_with(&format!("{r}/")))
}

/// Replace `${NAME}` with the value of environment variable NAME.
pub fn substitute(s: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated ${{...}} in '{s}'"))?;
        let name = &after[..end];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("invalid variable reference '${{{name}}}'"));
        }
        match env(name) {
            Some(v) => out.push_str(&v),
            None => return Err(format!("environment variable {name} is not set")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

struct Ctx<'a> {
    env: &'a dyn Fn(&str) -> Option<String>,
    errors: Vec<String>,
}

impl Ctx<'_> {
    fn s(&mut self, where_: &str, v: &str) -> String {
        match substitute(v, self.env) {
            Ok(s) => s,
            Err(e) => {
                self.errors.push(format!("{where_}: {e}"));
                String::new()
            }
        }
    }
    fn os(&mut self, where_: &str, v: &Option<String>) -> Option<String> {
        v.as_ref().map(|v| self.s(where_, v))
    }
    fn err(&mut self, msg: String) {
        self.errors.push(msg);
    }
}

/// Substitute environment variables everywhere in a raw route (used for the fingerprint).
fn substitute_route(c: &mut Ctx, r: &Route) -> Route {
    let w = format!("route {}", r.path);
    let middleware = r
        .middleware
        .iter()
        .map(|m| match m {
            Middleware::Headers { ops } => Middleware::Headers {
                ops: ops.iter().map(|o| sub_header(c, &w, o)).collect(),
            },
            Middleware::Request { ops } => Middleware::Request {
                ops: ops
                    .iter()
                    .map(|o| match o {
                        RequestOp::RewritePath { from, to } => RequestOp::RewritePath {
                            from: c.s(&w, from),
                            to: c.s(&w, to),
                        },
                        RequestOp::MaxBodySize { bytes } => RequestOp::MaxBodySize {
                            bytes: c.s(&w, bytes),
                        },
                        RequestOp::Timeout { duration } => RequestOp::Timeout {
                            duration: c.s(&w, duration),
                        },
                    })
                    .collect(),
            },
            Middleware::Response { ops } => Middleware::Response {
                ops: ops
                    .iter()
                    .map(|o| match o {
                        ResponseOp::Set { name, value } => ResponseOp::Set {
                            name: c.s(&w, name),
                            value: c.s(&w, value),
                        },
                        ResponseOp::Add { name, value } => ResponseOp::Add {
                            name: c.s(&w, name),
                            value: c.s(&w, value),
                        },
                        ResponseOp::Remove { name } => ResponseOp::Remove {
                            name: c.s(&w, name),
                        },
                        ResponseOp::Cache { ttl } => ResponseOp::Cache { ttl: c.s(&w, ttl) },
                    })
                    .collect(),
            },
            Middleware::Ai { provider, model } => Middleware::Ai {
                provider: c.s(&w, provider),
                model: c.os(&w, model),
            },
            Middleware::RateLimit { requests, per, key } => Middleware::RateLimit {
                requests: c.s(&w, requests),
                per: c.s(&w, per),
                key: c.os(&w, key),
            },
            Middleware::TokenBudget { tokens, per, key } => Middleware::TokenBudget {
                tokens: c.s(&w, tokens),
                per: c.s(&w, per),
                key: c.os(&w, key),
            },
        })
        .collect();
    let upstream = match &r.upstream {
        Upstream::Upstream { url } => Upstream::Upstream { url: c.s(&w, url) },
        Upstream::LoadBalance {
            strategy,
            upstreams,
        } => Upstream::LoadBalance {
            strategy: c.os(&w, strategy),
            upstreams: upstreams.iter().map(|u| c.s(&w, u)).collect(),
        },
    };
    Route {
        path: c.s(&w, &r.path),
        auth: c.os(&w, &r.auth),
        description: c.os(&w, &r.description),
        middleware,
        upstream,
    }
}

fn sub_header(c: &mut Ctx, w: &str, o: &HeaderOp) -> HeaderOp {
    match o {
        HeaderOp::Set { name, value } => HeaderOp::Set {
            name: c.s(w, name),
            value: c.s(w, value),
        },
        HeaderOp::Add { name, value } => HeaderOp::Add {
            name: c.s(w, name),
            value: c.s(w, value),
        },
        HeaderOp::Remove { name } => HeaderOp::Remove { name: c.s(w, name) },
    }
}

fn header_op(c: &mut Ctx, w: &str, name: &str, value: Option<&str>) -> bool {
    if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
        c.err(format!("{w}: invalid header name '{name}'"));
        return false;
    }
    if let Some(v) = value {
        if http::HeaderValue::from_str(v).is_err() {
            c.err(format!("{w}: invalid value for header '{name}'"));
            return false;
        }
    }
    true
}

fn header_r(c: &mut Ctx, w: &str, o: &HeaderOp) -> Option<HeaderOpR> {
    match o {
        HeaderOp::Set { name, value } => {
            header_op(c, w, name, Some(value)).then(|| HeaderOpR::Set(name.clone(), value.clone()))
        }
        HeaderOp::Add { name, value } => {
            header_op(c, w, name, Some(value)).then(|| HeaderOpR::Add(name.clone(), value.clone()))
        }
        HeaderOp::Remove { name } => {
            header_op(c, w, name, None).then(|| HeaderOpR::Remove(name.clone()))
        }
    }
}

fn duration(c: &mut Ctx, w: &str, what: &str, v: &str) -> Duration {
    match parse_duration(v) {
        Some(d) if !d.is_zero() => d,
        _ => {
            c.err(format!("{w}: invalid {what} '{v}' (expected e.g. 30s, 5m, 24h)"));
            Duration::from_secs(1)
        }
    }
}

fn positive(c: &mut Ctx, w: &str, what: &str, v: &str) -> u64 {
    match v.trim().parse::<u64>() {
        Ok(n) if n > 0 => n,
        _ => {
            c.err(format!("{w}: {what} must be a positive integer, got '{v}'"));
            1
        }
    }
}

fn limit_key(c: &mut Ctx, w: &str, v: Option<&str>, allow_ip: bool) -> LimitKey {
    match v.unwrap_or("api-key") {
        "api-key" => LimitKey::ApiKey,
        "user" => LimitKey::User,
        "ip" if allow_ip => LimitKey::Ip,
        other => {
            let allowed = if allow_ip {
                "api-key, user or ip"
            } else {
                "api-key or user"
            };
            c.err(format!("{w}: key must be {allowed}, got '{other}'"));
            LimitKey::ApiKey
        }
    }
}

fn upstream_url(c: &mut Ctx, w: &str, v: &str) -> Option<Url> {
    match Url::parse(v) {
        Ok(u)
            if (u.scheme() == "http" || u.scheme() == "https")
                && u.host_str().is_some()
                && u.query().is_none()
                && u.fragment().is_none() =>
        {
            Some(u)
        }
        _ => {
            c.err(format!(
                "{w}: invalid upstream url '{v}' (expected http(s)://host[:port][/path])"
            ));
            None
        }
    }
}

pub fn resolve(cfg: &Config, env: &dyn Fn(&str) -> Option<String>) -> Result<Resolved, Vec<String>> {
    let mut c = Ctx {
        env,
        errors: Vec::new(),
    };
    if cfg.version != "1" {
        c.err(format!(
            "unsupported MML version '{}' (expected \"1\")",
            cfg.version
        ));
    }

    let host = c
        .os("<server host>", &cfg.server.host)
        .unwrap_or_else(|| DEFAULT_HOST.into());
    if host.trim().is_empty() {
        c.err("<server host> must not be empty".into());
    }
    let port = match c.os("<server port>", &cfg.server.port) {
        None => DEFAULT_PORT,
        Some(p) => match p.trim().parse::<u16>() {
            Ok(n) if n > 0 => n,
            _ => {
                c.err(format!("<server port> must be 1-65535, got '{p}'"));
                DEFAULT_PORT
            }
        },
    };

    let auth = cfg.auth.as_ref().map(|a| {
        let issuer = c.s("<auth issuer>", &a.issuer);
        if Url::parse(&issuer).is_err() {
            c.err(format!("<auth issuer> must be a URL, got '{issuer}'"));
        }
        let client_id = c.s("<auth client-id>", &a.client_id);
        if client_id.is_empty() {
            c.err("<auth client-id> must not be empty".into());
        }
        let client_secret = c.os("<auth client-secret>", &a.client_secret);
        let session_ttl = match c.os("<auth session-ttl>", &a.session_ttl) {
            Some(t) => duration(&mut c, "<auth>", "session-ttl", &t),
            None => DEFAULT_SESSION_TTL,
        };
        let mut admins = Vec::new();
        for e in &a.admins {
            let e = c.s("<admin email>", e);
            if !e.contains('@') {
                c.err(format!("<admin email> '{e}' is not an email address"));
            }
            admins.push(e.to_ascii_lowercase());
        }
        AuthSettings {
            issuer,
            client_id,
            client_secret,
            session_ttl,
            admins,
        }
    });

    let mut routes: Vec<Arc<RouteSpec>> = Vec::new();
    for (index, raw) in cfg.routes.iter().enumerate() {
        let r = substitute_route(&mut c, raw);
        let w = format!("route {}", r.path);

        // Path checks.
        let path = r.path.clone();
        let (base, wildcard) = if let Some(stem) = path.strip_suffix("/*") {
            (format!("{stem}/"), true)
        } else {
            (path.clone(), false)
        };
        let stem = if wildcard {
            base.trim_end_matches('/').to_string()
        } else {
            base.clone()
        };
        if !path.starts_with('/') {
            c.err(format!("{w}: path must start with '/'"));
        } else if base[..base.len().saturating_sub(if wildcard { 1 } else { 0 })].contains('*') {
            c.err(format!(
                "{w}: '*' is only allowed as the final path segment (/prefix/*)"
            ));
        } else if path == "/" || (!stem.is_empty() && is_reserved(&stem)) {
            c.err(format!(
                "{w}: path is reserved for built-in routes (/, /chat, /admin, /auth, /api, /_app)"
            ));
        }
        if routes.iter().any(|o| o.path == path) {
            c.err(format!("duplicate route path {path}"));
        }

        let auth = match r.auth.as_deref().unwrap_or("api-key") {
            "none" => AuthMode::None,
            "api-key" => AuthMode::ApiKey,
            "session" => AuthMode::Session,
            other => {
                c.err(format!(
                    "{w}: auth must be none, api-key or session, got '{other}'"
                ));
                AuthMode::ApiKey
            }
        };

        let mut middleware = Vec::new();
        let mut ai = None;
        let mut cache_ttl = None;
        let mut has_token_budget = false;
        for (id, m) in r.middleware.iter().enumerate() {
            match m {
                Middleware::Headers { ops } => {
                    let ops = ops.iter().filter_map(|o| header_r(&mut c, &w, o)).collect();
                    middleware.push(Mw::Headers(ops));
                }
                Middleware::Request { ops } => {
                    let mut out = Vec::new();
                    for o in ops {
                        out.push(match o {
                            RequestOp::RewritePath { from, to } => {
                                if !from.starts_with('/') || !to.starts_with('/') {
                                    c.err(format!(
                                        "{w}: rewrite-path from/to must start with '/'"
                                    ));
                                }
                                ReqOpR::RewritePath {
                                    from: from.clone(),
                                    to: to.clone(),
                                }
                            }
                            RequestOp::MaxBodySize { bytes } => {
                                ReqOpR::MaxBodySize(positive(&mut c, &w, "max-body-size bytes", bytes))
                            }
                            RequestOp::Timeout { duration: d } => {
                                ReqOpR::Timeout(duration(&mut c, &w, "timeout duration", d))
                            }
                        });
                    }
                    middleware.push(Mw::Request(out));
                }
                Middleware::Response { ops } => {
                    let mut out = Vec::new();
                    for o in ops {
                        match o {
                            ResponseOp::Set { name, value } => {
                                let h = HeaderOp::Set {
                                    name: name.clone(),
                                    value: value.clone(),
                                };
                                if let Some(h) = header_r(&mut c, &w, &h) {
                                    out.push(RespOpR::Header(h));
                                }
                            }
                            ResponseOp::Add { name, value } => {
                                let h = HeaderOp::Add {
                                    name: name.clone(),
                                    value: value.clone(),
                                };
                                if let Some(h) = header_r(&mut c, &w, &h) {
                                    out.push(RespOpR::Header(h));
                                }
                            }
                            ResponseOp::Remove { name } => {
                                let h = HeaderOp::Remove { name: name.clone() };
                                if let Some(h) = header_r(&mut c, &w, &h) {
                                    out.push(RespOpR::Header(h));
                                }
                            }
                            ResponseOp::Cache { ttl } => {
                                if cache_ttl.is_some() {
                                    c.err(format!("{w}: only one <cache> is allowed per route"));
                                }
                                let d = duration(&mut c, &w, "cache ttl", ttl);
                                cache_ttl = Some(d);
                                out.push(RespOpR::Cache(d));
                            }
                        }
                    }
                    middleware.push(Mw::Response(out));
                }
                Middleware::Ai { provider, model } => {
                    let provider = match provider.as_str() {
                        "openai" => Provider::OpenAi,
                        "anthropic" => Provider::Anthropic,
                        "ollama" => Provider::Ollama,
                        other => {
                            c.err(format!(
                                "{w}: ai provider must be openai, anthropic or ollama, got '{other}'"
                            ));
                            Provider::OpenAi
                        }
                    };
                    if ai.is_some() {
                        c.err(format!("{w}: only one <ai> is allowed per route"));
                    }
                    let spec = AiSpec {
                        provider,
                        model: model.clone().filter(|m| !m.is_empty()),
                    };
                    ai = Some(spec.clone());
                    middleware.push(Mw::Ai(spec));
                }
                Middleware::RateLimit { requests, per, key } => {
                    let per = match per.as_str() {
                        "1s" | "1m" | "1h" => parse_duration(per).unwrap(),
                        other => {
                            c.err(format!("{w}: rate-limit per must be 1s, 1m or 1h, got '{other}'"));
                            Duration::from_secs(1)
                        }
                    };
                    middleware.push(Mw::RateLimit(RateLimitSpec {
                        id,
                        requests: positive(&mut c, &w, "rate-limit requests", requests),
                        per,
                        key: limit_key(&mut c, &w, key.as_deref(), true),
                    }));
                }
                Middleware::TokenBudget { tokens, per, key } => {
                    let per = match per.as_str() {
                        "1h" | "1d" | "30d" => parse_duration(per).unwrap(),
                        other => {
                            c.err(format!(
                                "{w}: token-budget per must be 1h, 1d or 30d, got '{other}'"
                            ));
                            Duration::from_secs(3600)
                        }
                    };
                    has_token_budget = true;
                    middleware.push(Mw::TokenBudget(TokenBudgetSpec {
                        id,
                        tokens: positive(&mut c, &w, "token-budget tokens", tokens),
                        per,
                        key: limit_key(&mut c, &w, key.as_deref(), false),
                    }));
                }
            }
        }
        if has_token_budget && ai.is_none() {
            c.err(format!("{w}: <token-budget> requires an <ai> element on the route"));
        }

        let upstream = match &r.upstream {
            Upstream::Upstream { url } => UpstreamSpec {
                strategy: Strategy::Single,
                urls: upstream_url(&mut c, &w, url).into_iter().collect(),
            },
            Upstream::LoadBalance {
                strategy,
                upstreams,
            } => {
                let strategy = match strategy.as_deref().unwrap_or("round-robin") {
                    "round-robin" => Strategy::RoundRobin,
                    "random" => Strategy::Random,
                    "failover" => Strategy::Failover,
                    other => {
                        c.err(format!(
                            "{w}: load-balance strategy must be round-robin, random or failover, got '{other}'"
                        ));
                        Strategy::RoundRobin
                    }
                };
                if upstreams.len() < 2 {
                    c.err(format!(
                        "{w}: <load-balance> needs two or more <upstream> elements"
                    ));
                }
                UpstreamSpec {
                    strategy,
                    urls: upstreams
                        .iter()
                        .filter_map(|u| upstream_url(&mut c, &w, u))
                        .collect(),
                }
            }
        };

        let fingerprint = sha256_hex(&serde_json::to_vec(&r).unwrap_or_default());
        routes.push(Arc::new(RouteSpec {
            index,
            path,
            base,
            wildcard,
            auth,
            description: r.description.clone().unwrap_or_default(),
            middleware,
            upstream,
            ai,
            cache_ttl,
            has_token_budget,
            fingerprint,
        }));
    }

    if c.errors.is_empty() {
        Ok(Resolved {
            host,
            port,
            auth,
            routes,
        })
    } else {
        Err(c.errors)
    }
}
