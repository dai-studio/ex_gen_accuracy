//! Configuration: Moxy Markup Language (moxy.xml).
//!
//! `doc` is the raw document model, `xml` reads and writes it, and this module
//! compiles a document into the validated, typed, environment-resolved
//! runtime [`Config`] that request handling uses.

pub mod doc;
pub mod duration;
pub mod env;
pub mod xml;

use crate::routing;
pub use doc::ConfigDoc;
use doc::*;
use duration::parse_duration;
use http::{HeaderName, HeaderValue};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(24 * 3600);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A single problem found while parsing or validating moxy.xml.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationError {
    pub location: String,
    pub message: String,
}

impl ValidationError {
    pub fn new(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

pub fn format_errors(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

// ---------------------------------------------------------------------------
// Runtime model
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Config {
    /// The raw document this configuration was compiled from.
    pub doc: ConfigDoc,
    pub server: ServerConfig,
    pub auth: Option<AuthConfig>,
    pub routes: Vec<Arc<Route>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl ServerConfig {
    /// Whether the server is bound to a loopback name for which cookies need not be Secure.
    pub fn is_local(&self) -> bool {
        self.host == "127.0.0.1" || self.host == "localhost"
    }
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub session_ttl: Duration,
    /// Administrator emails, lowercased.
    pub admins: Vec<String>,
}

impl AuthConfig {
    pub fn is_admin(&self, email: &str) -> bool {
        let email = email.trim().to_ascii_lowercase();
        !email.is_empty() && self.admins.contains(&email)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    None,
    ApiKey,
    Session,
}

impl AuthMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMode::None => "none",
            AuthMode::ApiKey => "api-key",
            AuthMode::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutePattern {
    /// `/prefix/*`: `prefix` keeps its trailing slash.
    Prefix {
        raw: String,
        prefix: String,
    },
    Exact {
        raw: String,
    },
}

impl RoutePattern {
    pub fn parse(raw: &str) -> Result<Self, String> {
        if !raw.starts_with('/') {
            return Err("route path must start with '/'".into());
        }
        if raw
            .chars()
            .any(|c| c.is_whitespace() || c == '?' || c == '#')
        {
            return Err("route path must not contain whitespace, '?' or '#'".into());
        }
        if let Some(prefix) = raw.strip_suffix('*') {
            if !prefix.ends_with('/') {
                return Err("a wildcard route path must end with '/*'".into());
            }
            if prefix.contains('*') {
                return Err("'*' is only allowed at the end of a route path".into());
            }
            Ok(RoutePattern::Prefix {
                raw: raw.to_string(),
                prefix: prefix.to_string(),
            })
        } else if raw.contains('*') {
            Err("'*' is only allowed at the end of a route path".into())
        } else {
            Ok(RoutePattern::Exact {
                raw: raw.to_string(),
            })
        }
    }

    pub fn raw(&self) -> &str {
        match self {
            RoutePattern::Prefix { raw, .. } | RoutePattern::Exact { raw } => raw,
        }
    }

    /// The literal (non-wildcard) part of the pattern.
    pub fn literal(&self) -> &str {
        match self {
            RoutePattern::Prefix { prefix, .. } => prefix,
            RoutePattern::Exact { raw } => raw,
        }
    }

    /// If `path` matches, returns the suffix matched by the asterisk, with its
    /// leading slash (empty for exact matches).
    pub fn matches<'p>(&self, path: &'p str) -> Option<&'p str> {
        match self {
            RoutePattern::Prefix { prefix, .. } => {
                if let Some(rest) = path.strip_prefix(prefix.as_str()) {
                    // keep the slash that separated prefix and suffix
                    Some(&path[path.len() - rest.len() - 1..])
                } else if path == &prefix[..prefix.len() - 1] {
                    Some("")
                } else {
                    None
                }
            }
            RoutePattern::Exact { raw } => (path == raw).then_some(""),
        }
    }

    pub fn specificity(&self) -> usize {
        self.literal().len()
    }
}

#[derive(Debug)]
pub struct Route {
    pub index: usize,
    pub pattern: RoutePattern,
    pub auth: AuthMode,
    pub description: String,
    pub middleware: Vec<Middleware>,
    pub destination: Destination,
    pub doc: RouteDoc,
}

impl Route {
    pub fn path(&self) -> &str {
        self.pattern.raw()
    }

    pub fn ai(&self) -> Option<&AiConfig> {
        self.middleware.iter().find_map(|m| match m {
            Middleware::Ai(ai) => Some(ai),
            _ => None,
        })
    }

    pub fn cache_ttl(&self) -> Option<Duration> {
        self.middleware.iter().find_map(|m| match m {
            Middleware::Response(r) => r.cache,
            _ => None,
        })
    }

    /// The effective upstream timeout (the last `<timeout>` wins).
    pub fn timeout(&self) -> Duration {
        let mut t = DEFAULT_TIMEOUT;
        for m in &self.middleware {
            if let Middleware::Request(r) = m {
                for op in &r.ops {
                    if let RequestOp::Timeout(d) = op {
                        t = *d;
                    }
                }
            }
        }
        t
    }

    /// The smallest configured maximum request body size, if any.
    pub fn max_body_size(&self) -> Option<u64> {
        let mut limit: Option<u64> = None;
        for m in &self.middleware {
            if let Middleware::Request(r) = m {
                for op in &r.ops {
                    if let RequestOp::MaxBodySize(n) = op {
                        limit = Some(limit.map_or(*n, |l| l.min(*n)));
                    }
                }
            }
        }
        limit
    }

    pub fn token_budgets(&self) -> impl Iterator<Item = &TokenBudgetConfig> {
        self.middleware.iter().filter_map(|m| match m {
            Middleware::TokenBudget(t) => Some(t),
            _ => None,
        })
    }

    /// Apply every `<rewrite-path>` in document order to the forwarded suffix.
    pub fn rewrite_path(&self, suffix: &str) -> String {
        let mut path = suffix.to_string();
        for m in &self.middleware {
            if let Middleware::Request(r) = m {
                for op in &r.ops {
                    if let RequestOp::RewritePath { from, to } = op
                        && let Some(rest) = path.strip_prefix(from.as_str())
                    {
                        path = format!("{to}{rest}");
                    }
                }
            }
        }
        path
    }

    /// Response-phase header operations, in the order they run (reverse document order).
    pub fn response_header_ops(&self) -> Vec<&HeaderOp> {
        let mut ops = Vec::new();
        for m in self.middleware.iter().rev() {
            if let Middleware::Response(r) = m {
                ops.extend(r.headers.iter());
            }
        }
        ops
    }
}

#[derive(Debug, Clone)]
pub enum Middleware {
    Headers(Vec<HeaderOp>),
    Request(RequestMw),
    Response(ResponseMw),
    Ai(AiConfig),
    RateLimit(RateLimitConfig),
    TokenBudget(TokenBudgetConfig),
}

#[derive(Debug, Clone)]
pub enum HeaderOp {
    Set(HeaderName, HeaderValue),
    Add(HeaderName, HeaderValue),
    Remove(HeaderName),
}

impl HeaderOp {
    pub fn apply(&self, headers: &mut http::HeaderMap) {
        match self {
            HeaderOp::Set(n, v) => {
                headers.insert(n.clone(), v.clone());
            }
            HeaderOp::Add(n, v) => {
                headers.append(n.clone(), v.clone());
            }
            HeaderOp::Remove(n) => {
                headers.remove(n);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RequestMw {
    pub ops: Vec<RequestOp>,
}

#[derive(Debug, Clone)]
pub enum RequestOp {
    RewritePath { from: String, to: String },
    MaxBodySize(u64),
    Timeout(Duration),
}

#[derive(Debug, Clone)]
pub struct ResponseMw {
    pub headers: Vec<HeaderOp>,
    pub cache: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    OpenAi,
    Anthropic,
    Ollama,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Ollama => "ollama",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AiConfig {
    pub provider: Provider,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountKey {
    ApiKey,
    User,
    Ip,
}

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub requests: u64,
    pub per: Duration,
    pub key: CountKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetWindow {
    Hour,
    Day,
    ThirtyDays,
}

impl BudgetWindow {
    pub fn seconds(&self) -> i64 {
        match self {
            BudgetWindow::Hour => 3600,
            BudgetWindow::Day => 86_400,
            BudgetWindow::ThirtyDays => 30 * 86_400,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TokenBudgetConfig {
    pub tokens: u64,
    pub window: BudgetWindow,
    pub key: CountKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    RoundRobin,
    Random,
    Failover,
}

#[derive(Debug, Clone)]
pub enum Destination {
    Single(Url),
    LoadBalance {
        strategy: Strategy,
        upstreams: Vec<Url>,
    },
}

impl Destination {
    pub fn upstreams(&self) -> &[Url] {
        match self {
            Destination::Single(u) => std::slice::from_ref(u),
            Destination::LoadBalance { upstreams, .. } => upstreams,
        }
    }
}

impl Config {
    /// Find the proxy route for `path`: longest literal prefix wins, then declaration order.
    /// Returns the route and the suffix matched by its asterisk.
    pub fn match_route<'p>(&self, path: &'p str) -> Option<(Arc<Route>, &'p str)> {
        let mut best: Option<(&Arc<Route>, &'p str)> = None;
        for route in &self.routes {
            if let Some(suffix) = route.pattern.matches(path) {
                let better = match &best {
                    None => true,
                    Some((b, _)) => route.pattern.specificity() > b.pattern.specificity(),
                };
                if better {
                    best = Some((route, suffix));
                }
            }
        }
        best.map(|(r, s)| (r.clone(), s))
    }

    pub fn route_by_path(&self, path: &str) -> Option<Arc<Route>> {
        self.routes.iter().find(|r| r.path() == path).cloned()
    }

    pub fn session_ttl(&self) -> Duration {
        self.auth
            .as_ref()
            .map(|a| a.session_ttl)
            .unwrap_or(DEFAULT_SESSION_TTL)
    }

    pub fn is_admin_email(&self, email: &str) -> bool {
        self.auth.as_ref().is_some_and(|a| a.is_admin(email))
    }
}

// ---------------------------------------------------------------------------
// Compilation and validation
// ---------------------------------------------------------------------------

pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Look up variables in the process environment.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Parse and compile moxy.xml text.
pub fn load_str(text: &str, env: EnvLookup) -> Result<Config, Vec<ValidationError>> {
    let doc = xml::parse(text)?;
    compile(doc, env)
}

struct Compiler<'a> {
    env: EnvLookup<'a>,
    errors: Vec<ValidationError>,
}

impl<'a> Compiler<'a> {
    fn err(&mut self, loc: &str, msg: impl Into<String>) {
        self.errors.push(ValidationError::new(loc, msg));
    }

    fn resolve(&mut self, loc: &str, attr: &str, value: &str) -> Option<String> {
        match env::substitute(value, self.env) {
            Ok(v) => Some(v),
            Err(e) => {
                self.err(loc, format!("attribute '{attr}': {e}"));
                None
            }
        }
    }

    fn duration(&mut self, loc: &str, attr: &str, value: &str) -> Option<Duration> {
        let v = self.resolve(loc, attr, value)?;
        match parse_duration(&v) {
            Ok(d) if !d.is_zero() => Some(d),
            Ok(_) => {
                self.err(loc, format!("attribute '{attr}' must be greater than zero"));
                None
            }
            Err(e) => {
                self.err(loc, format!("attribute '{attr}': {e}"));
                None
            }
        }
    }

    fn positive(&mut self, loc: &str, attr: &str, value: &str) -> Option<u64> {
        let v = self.resolve(loc, attr, value)?;
        match v.trim().parse::<u64>() {
            Ok(n) if n > 0 => Some(n),
            _ => {
                self.err(
                    loc,
                    format!("attribute '{attr}' must be a positive integer"),
                );
                None
            }
        }
    }

    fn header_op(
        &mut self,
        loc: &str,
        kind: &str,
        name: &str,
        value: Option<&str>,
    ) -> Option<HeaderOp> {
        let name_r = self.resolve(loc, "name", name)?;
        let hname = match HeaderName::from_bytes(name_r.as_bytes()) {
            Ok(n) => n,
            Err(_) => {
                self.err(loc, format!("invalid header name '{name_r}'"));
                return None;
            }
        };
        match (kind, value) {
            ("remove", _) => Some(HeaderOp::Remove(hname)),
            (_, Some(value)) => {
                let v = self.resolve(loc, "value", value)?;
                let hv = match HeaderValue::from_str(&v) {
                    Ok(v) => v,
                    Err(_) => {
                        self.err(loc, format!("invalid value for header '{name_r}'"));
                        return None;
                    }
                };
                Some(if kind == "set" {
                    HeaderOp::Set(hname, hv)
                } else {
                    HeaderOp::Add(hname, hv)
                })
            }
            _ => None,
        }
    }

    fn url(&mut self, loc: &str, value: &str) -> Option<Url> {
        let v = self.resolve(loc, "url", value)?;
        match Url::parse(&v) {
            Ok(u) if (u.scheme() == "http" || u.scheme() == "https") && u.host_str().is_some() => {
                if u.query().is_some() || u.fragment().is_some() {
                    self.err(
                        loc,
                        format!("upstream url '{v}' must not have a query or fragment"),
                    );
                    return None;
                }
                Some(u)
            }
            _ => {
                self.err(
                    loc,
                    format!("upstream url '{v}' must be an absolute http or https URL"),
                );
                None
            }
        }
    }

    fn count_key(
        &mut self,
        loc: &str,
        value: Option<&str>,
        default: CountKey,
        allow_ip: bool,
    ) -> Option<CountKey> {
        let Some(value) = value else {
            return Some(default);
        };
        let v = self.resolve(loc, "key", value)?;
        match v.as_str() {
            "api-key" => Some(CountKey::ApiKey),
            "user" => Some(CountKey::User),
            "ip" if allow_ip => Some(CountKey::Ip),
            _ => {
                let allowed = if allow_ip {
                    "api-key, user or ip"
                } else {
                    "api-key or user"
                };
                self.err(loc, format!("attribute 'key' must be {allowed}"));
                None
            }
        }
    }
}

/// Validate a document and compile it into a runtime configuration.
pub fn compile(doc: ConfigDoc, env: EnvLookup) -> Result<Config, Vec<ValidationError>> {
    let mut c = Compiler {
        env,
        errors: Vec::new(),
    };

    if doc.version != "1" {
        c.err(
            "<moxy>",
            format!("unsupported version '{}', expected '1'", doc.version),
        );
    }

    // <server>
    let mut server = ServerConfig {
        host: DEFAULT_HOST.into(),
        port: DEFAULT_PORT,
    };
    if let Some(s) = &doc.server {
        if let Some(h) = &s.host
            && let Some(h) = c.resolve("<server>", "host", h)
        {
            if h.trim().is_empty() {
                c.err("<server>", "attribute 'host' must not be empty");
            } else {
                server.host = h.trim().to_string();
            }
        }
        if let Some(p) = &s.port
            && let Some(p) = c.resolve("<server>", "port", p)
        {
            match p.trim().parse::<u16>() {
                Ok(n) if n > 0 => server.port = n,
                _ => c.err(
                    "<server>",
                    "attribute 'port' must be a number between 1 and 65535",
                ),
            }
        }
    }

    // <auth>
    let auth = doc.auth.as_ref().and_then(|a| compile_auth(&mut c, a));

    // <route>s
    let mut routes = Vec::new();
    let mut seen = HashSet::new();
    for (index, rd) in doc.routes.iter().enumerate() {
        let loc = format!("<route path=\"{}\">", rd.path);
        if !seen.insert(rd.path.clone()) {
            c.err(&loc, "two routes declare the same path");
        }
        if let Some(route) = compile_route(&mut c, &loc, index, rd) {
            routes.push(Arc::new(route));
        }
    }

    if c.errors.is_empty() {
        Ok(Config {
            doc,
            server,
            auth,
            routes,
        })
    } else {
        Err(c.errors)
    }
}

fn compile_auth(c: &mut Compiler, a: &AuthDoc) -> Option<AuthConfig> {
    let loc = "<auth>";
    let before = c.errors.len();
    let issuer = c.resolve(loc, "issuer", &a.issuer).unwrap_or_default();
    match Url::parse(&issuer) {
        Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
        _ => c.err(loc, "attribute 'issuer' must be an http or https URL"),
    }
    let client_id = c
        .resolve(loc, "client-id", &a.client_id)
        .unwrap_or_default();
    if client_id.is_empty() {
        c.err(loc, "attribute 'client-id' must not be empty");
    }
    let client_secret = match &a.client_secret {
        Some(raw) if !raw.is_empty() => {
            if !env::has_reference(raw) {
                c.err(
                    loc,
                    "attribute 'client-secret' must be supplied through an environment variable reference such as ${MOXY_OIDC_CLIENT_SECRET}, never literally",
                );
            }
            c.resolve(loc, "client-secret", raw)
                .filter(|s| !s.is_empty())
        }
        _ => None,
    };
    let session_ttl = match &a.session_ttl {
        Some(t) => c
            .duration(loc, "session-ttl", t)
            .unwrap_or(DEFAULT_SESSION_TTL),
        None => DEFAULT_SESSION_TTL,
    };
    if a.admins.is_empty() {
        c.err(loc, "at least one <admin> element is required");
    }
    let mut admins = Vec::new();
    for email in &a.admins {
        if let Some(e) = c.resolve("<admin>", "email", email) {
            let e = e.trim().to_ascii_lowercase();
            if !e.contains('@') {
                c.err("<admin>", format!("'{e}' is not an email address"));
            }
            admins.push(e);
        }
    }
    (c.errors.len() == before).then_some(AuthConfig {
        issuer,
        client_id,
        client_secret,
        session_ttl,
        admins,
    })
}

fn compile_route(c: &mut Compiler, loc: &str, index: usize, rd: &RouteDoc) -> Option<Route> {
    let before = c.errors.len();

    let pattern = match RoutePattern::parse(&rd.path) {
        Ok(p) => {
            let literal = p.literal();
            if literal == "/" {
                c.err(loc, "a route must not use the root path");
            } else if routing::is_reserved(literal) {
                c.err(
                    loc,
                    "the path falls under a reserved prefix (/chat, /admin, /auth, /api, /_app)",
                );
            }
            Some(p)
        }
        Err(e) => {
            c.err(loc, e);
            None
        }
    };

    let auth = match rd.auth.as_deref() {
        None => AuthMode::ApiKey,
        Some(raw) => match c.resolve(loc, "auth", raw).as_deref() {
            Some("none") => AuthMode::None,
            Some("api-key") => AuthMode::ApiKey,
            Some("session") => AuthMode::Session,
            Some(_) => {
                c.err(loc, "attribute 'auth' must be none, api-key or session");
                AuthMode::ApiKey
            }
            None => AuthMode::ApiKey,
        },
    };
    let default_key = match auth {
        AuthMode::None => CountKey::Ip,
        AuthMode::ApiKey => CountKey::ApiKey,
        AuthMode::Session => CountKey::User,
    };

    let description = rd
        .description
        .as_deref()
        .and_then(|d| c.resolve(loc, "description", d))
        .unwrap_or_default();

    let mut middleware = Vec::new();
    let mut ai_count = 0;
    let mut cache_count = 0;
    let mut has_budget = false;
    for mw in &rd.middleware {
        match mw {
            MiddlewareDoc::Headers { ops } => {
                let mut out = Vec::new();
                for op in ops {
                    let compiled = match op {
                        HeaderOpDoc::Set { name, value } => {
                            c.header_op(loc, "set", name, Some(value))
                        }
                        HeaderOpDoc::Add { name, value } => {
                            c.header_op(loc, "add", name, Some(value))
                        }
                        HeaderOpDoc::Remove { name } => c.header_op(loc, "remove", name, None),
                    };
                    out.extend(compiled);
                }
                middleware.push(Middleware::Headers(out));
            }
            MiddlewareDoc::Request { ops } => {
                let mut out = Vec::new();
                for op in ops {
                    match op {
                        RequestOpDoc::RewritePath { from, to } => {
                            let f = c.resolve(loc, "from", from);
                            let t = c.resolve(loc, "to", to);
                            if let (Some(f), Some(t)) = (f, t) {
                                if !f.starts_with('/') || !t.starts_with('/') {
                                    c.err(
                                        loc,
                                        "<rewrite-path> 'from' and 'to' must start with '/'",
                                    );
                                } else {
                                    out.push(RequestOp::RewritePath { from: f, to: t });
                                }
                            }
                        }
                        RequestOpDoc::MaxBodySize { bytes } => {
                            if let Some(n) = c.positive(loc, "bytes", bytes) {
                                out.push(RequestOp::MaxBodySize(n));
                            }
                        }
                        RequestOpDoc::Timeout { duration } => {
                            if let Some(d) = c.duration(loc, "duration", duration) {
                                out.push(RequestOp::Timeout(d));
                            }
                        }
                    }
                }
                middleware.push(Middleware::Request(RequestMw { ops: out }));
            }
            MiddlewareDoc::Response { ops } => {
                let mut headers = Vec::new();
                let mut cache = None;
                for op in ops {
                    match op {
                        ResponseOpDoc::Set { name, value } => {
                            headers.extend(c.header_op(loc, "set", name, Some(value)))
                        }
                        ResponseOpDoc::Add { name, value } => {
                            headers.extend(c.header_op(loc, "add", name, Some(value)))
                        }
                        ResponseOpDoc::Remove { name } => {
                            headers.extend(c.header_op(loc, "remove", name, None))
                        }
                        ResponseOpDoc::Cache { ttl } => {
                            cache_count += 1;
                            cache = c.duration(loc, "ttl", ttl);
                        }
                    }
                }
                middleware.push(Middleware::Response(ResponseMw { headers, cache }));
            }
            MiddlewareDoc::Ai { provider, model } => {
                ai_count += 1;
                let provider = match c.resolve(loc, "provider", provider).as_deref() {
                    Some("openai") => Some(Provider::OpenAi),
                    Some("anthropic") => Some(Provider::Anthropic),
                    Some("ollama") => Some(Provider::Ollama),
                    Some(_) => {
                        c.err(
                            loc,
                            "<ai> attribute 'provider' must be openai, anthropic or ollama",
                        );
                        None
                    }
                    None => None,
                };
                let model = model
                    .as_deref()
                    .and_then(|m| c.resolve(loc, "model", m))
                    .filter(|m| !m.is_empty());
                if let Some(provider) = provider {
                    middleware.push(Middleware::Ai(AiConfig { provider, model }));
                }
            }
            MiddlewareDoc::RateLimit { requests, per, key } => {
                let requests = c.positive(loc, "requests", requests);
                let per = match c.resolve(loc, "per", per).as_deref() {
                    Some("1s" | "second") => Some(Duration::from_secs(1)),
                    Some("1m" | "minute") => Some(Duration::from_secs(60)),
                    Some("1h" | "hour") => Some(Duration::from_secs(3600)),
                    Some(_) => {
                        c.err(loc, "<rate-limit> attribute 'per' must be 1s, 1m or 1h");
                        None
                    }
                    None => None,
                };
                let key = c.count_key(loc, key.as_deref(), default_key, true);
                if let (Some(requests), Some(per), Some(key)) = (requests, per, key) {
                    middleware.push(Middleware::RateLimit(RateLimitConfig {
                        requests,
                        per,
                        key,
                    }));
                }
            }
            MiddlewareDoc::TokenBudget { tokens, per, key } => {
                has_budget = true;
                let tokens = c.positive(loc, "tokens", tokens);
                let window = match c.resolve(loc, "per", per).as_deref() {
                    Some("1h" | "hour") => Some(BudgetWindow::Hour),
                    Some("1d" | "day" | "24h") => Some(BudgetWindow::Day),
                    Some("30d" | "month") => Some(BudgetWindow::ThirtyDays),
                    Some(_) => {
                        c.err(loc, "<token-budget> attribute 'per' must be 1h, 1d or 30d");
                        None
                    }
                    None => None,
                };
                let budget_default = if default_key == CountKey::Ip {
                    CountKey::ApiKey
                } else {
                    default_key
                };
                let key = c.count_key(loc, key.as_deref(), budget_default, false);
                if let (Some(tokens), Some(window), Some(key)) = (tokens, window, key) {
                    middleware.push(Middleware::TokenBudget(TokenBudgetConfig {
                        tokens,
                        window,
                        key,
                    }));
                }
            }
        }
    }
    if ai_count > 1 {
        c.err(loc, "a route may contain at most one <ai> element");
    }
    if cache_count > 1 {
        c.err(loc, "a route may contain at most one <cache> element");
    }
    if has_budget && ai_count == 0 {
        c.err(
            loc,
            "<token-budget> requires an <ai> element on the same route",
        );
    }

    let destination = match &rd.destination {
        None => {
            c.err(
                loc,
                "a route needs an <upstream> or a <load-balance> element",
            );
            None
        }
        Some(DestinationDoc::Upstream { url }) => c.url(loc, url).map(Destination::Single),
        Some(DestinationDoc::LoadBalance {
            strategy,
            upstreams,
        }) => {
            let strategy = match c.resolve(loc, "strategy", strategy).as_deref() {
                Some("round-robin") => Some(Strategy::RoundRobin),
                Some("random") => Some(Strategy::Random),
                Some("failover") => Some(Strategy::Failover),
                Some(_) => {
                    c.err(loc, "<load-balance> attribute 'strategy' must be round-robin, random or failover");
                    None
                }
                None => None,
            };
            if upstreams.len() < 2 {
                c.err(loc, "<load-balance> needs two or more <upstream> elements");
            }
            let urls: Vec<Url> = upstreams.iter().filter_map(|u| c.url(loc, u)).collect();
            strategy.map(|strategy| Destination::LoadBalance {
                strategy,
                upstreams: urls,
            })
        }
    };

    if c.errors.len() != before {
        return None;
    }
    Some(Route {
        index,
        pattern: pattern?,
        auth,
        description,
        middleware,
        destination: destination?,
        doc: rd.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "SECRET" => Some("s3cret".into()),
            "OPENAI_API_KEY" => Some("sk-test".into()),
            _ => None,
        }
    }

    fn load(xml: &str) -> Result<Config, Vec<ValidationError>> {
        load_str(xml, &env)
    }

    fn wrap(routes: &str) -> String {
        format!(
            r#"<moxy version="1"><server host="127.0.0.1" port="8080"/>
            <auth issuer="https://idp.example.com" client-id="moxy" client-secret="${{SECRET}}">
              <admin email="Admin@Example.com"/></auth>{routes}</moxy>"#
        )
    }

    fn messages(errs: &[ValidationError]) -> String {
        format_errors(errs)
    }

    #[test]
    fn compiles_valid_config() {
        let cfg = load(&wrap(
            r#"<route path="/openai/*" description="OpenAI">
                 <headers><set name="Authorization" value="Bearer ${OPENAI_API_KEY}"/></headers>
                 <ai provider="openai"/>
                 <token-budget tokens="1000" per="1d"/>
                 <upstream url="https://api.openai.com/v1"/></route>"#,
        ))
        .unwrap();
        assert_eq!(cfg.server.port, 8080);
        let auth = cfg.auth.as_ref().unwrap();
        assert_eq!(auth.client_secret.as_deref(), Some("s3cret"));
        assert!(auth.is_admin("admin@example.com"));
        assert!(auth.is_admin("ADMIN@example.com"));
        let r = &cfg.routes[0];
        assert_eq!(r.auth, AuthMode::ApiKey);
        assert!(r.ai().is_some());
        match &r.middleware[0] {
            Middleware::Headers(ops) => match &ops[0] {
                HeaderOp::Set(_, v) => assert_eq!(v, "Bearer sk-test"),
                _ => panic!(),
            },
            _ => panic!(),
        }
        // the raw document keeps the reference, never the secret
        assert!(xml::write(&cfg.doc).contains("${OPENAI_API_KEY}"));
        assert!(!xml::write(&cfg.doc).contains("sk-test"));
    }

    #[test]
    fn defaults_without_server_and_auth() {
        let cfg = load(r#"<moxy version="1"><route path="/x/*" auth="none"><upstream url="http://127.0.0.1:1"/></route></moxy>"#).unwrap();
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 8080);
        assert!(cfg.auth.is_none());
        assert_eq!(cfg.session_ttl(), DEFAULT_SESSION_TTL);
    }

    #[test]
    fn rejects_duplicate_paths() {
        let errs = load(&wrap(
            r#"<route path="/a/*"><upstream url="http://x"/></route>
               <route path="/a/*"><upstream url="http://y"/></route>"#,
        ))
        .unwrap_err();
        assert!(messages(&errs).contains("same path"));
    }

    #[test]
    fn rejects_reserved_and_root_paths() {
        for p in [
            "/",
            "/*",
            "/chat/*",
            "/admin",
            "/auth/*",
            "/api/v1/*",
            "/_app/*",
            "/chat",
        ] {
            let errs = load(&wrap(&format!(
                r#"<route path="{p}"><upstream url="http://x"/></route>"#
            )))
            .unwrap_err();
            let m = messages(&errs);
            assert!(m.contains("reserved") || m.contains("root"), "{p}: {m}");
        }
        // similar-looking but not reserved
        assert!(
            load(&wrap(
                r#"<route path="/chatty/*"><upstream url="http://x"/></route>"#
            ))
            .is_ok()
        );
    }

    #[test]
    fn rejects_missing_destination() {
        let errs = load(&wrap(
            r#"<route path="/a/*"><ai provider="openai"/></route>"#,
        ))
        .unwrap_err();
        assert!(messages(&errs).contains("needs an <upstream>"));
    }

    #[test]
    fn rejects_unresolved_env() {
        let errs = load(&wrap(
            r#"<route path="/a/*"><headers><set name="X" value="${MISSING}"/></headers><upstream url="http://x"/></route>"#,
        ))
        .unwrap_err();
        assert!(messages(&errs).contains("'MISSING' is not set"));
    }

    #[test]
    fn rejects_literal_client_secret() {
        let errs = load(r#"<moxy version="1"><auth issuer="https://i" client-id="c" client-secret="literal"><admin email="a@b.c"/></auth></moxy>"#).unwrap_err();
        assert!(messages(&errs).contains("client-secret"));
    }

    #[test]
    fn rejects_bad_middleware_values() {
        let errs = load(&wrap(
            r#"<route path="/a/*">
                 <rate-limit requests="0" per="1d" key="nobody"/>
                 <token-budget tokens="10" per="1d"/>
                 <load-balance strategy="weird"><upstream url="ftp://x"/></load-balance>
               </route>"#,
        ))
        .unwrap_err();
        let m = messages(&errs);
        assert!(m.contains("'requests' must be a positive integer"), "{m}");
        assert!(m.contains("'per' must be 1s, 1m or 1h"), "{m}");
        assert!(m.contains("'key' must be"), "{m}");
        assert!(m.contains("requires an <ai>"), "{m}");
        assert!(m.contains("strategy"), "{m}");
        assert!(m.contains("two or more"), "{m}");
        assert!(m.contains("absolute http or https"), "{m}");
    }

    #[test]
    fn rejects_bad_version() {
        let errs = load(r#"<moxy version="2"></moxy>"#).unwrap_err();
        assert!(messages(&errs).contains("unsupported version"));
    }

    #[test]
    fn route_matching_longest_prefix_then_order() {
        let cfg = load(&wrap(
            r#"<route path="/m/*"><upstream url="http://a"/></route>
               <route path="/m/special/*"><upstream url="http://b"/></route>
               <route path="/m/exact"><upstream url="http://c"/></route>"#,
        ))
        .unwrap();
        let (r, s) = cfg.match_route("/m/special/x").unwrap();
        assert_eq!(r.path(), "/m/special/*");
        assert_eq!(s, "/x");
        let (r, s) = cfg.match_route("/m/other?").unwrap();
        assert_eq!(r.path(), "/m/*");
        assert_eq!(s, "/other?");
        let (r, s) = cfg.match_route("/m/exact").unwrap();
        assert_eq!(r.path(), "/m/exact");
        assert_eq!(s, "");
        let (r, s) = cfg.match_route("/m").unwrap();
        assert_eq!(r.path(), "/m/*");
        assert_eq!(s, "");
        assert!(cfg.match_route("/nope").is_none());
        assert!(cfg.match_route("/mx").is_none());
    }

    #[test]
    fn tie_goes_to_first_declared() {
        // an exact route and a prefix route with equal literal lengths
        let cfg = load(&wrap(
            r#"<route path="/a/b/"><upstream url="http://first"/></route>
               <route path="/a/b/*"><upstream url="http://second"/></route>"#,
        ))
        .unwrap();
        let (r, _) = cfg.match_route("/a/b/").unwrap();
        assert_eq!(r.path(), "/a/b/");
    }

    #[test]
    fn request_helpers() {
        let cfg = load(&wrap(
            r#"<route path="/a/*">
                 <request><rewrite-path from="/v1/" to="/api/"/><max-body-size bytes="100"/><timeout duration="5s"/></request>
                 <request><max-body-size bytes="50"/></request>
                 <response><set name="X-A" value="1"/><cache ttl="5m"/></response>
                 <response><remove name="X-B"/></response>
                 <upstream url="http://x"/></route>"#,
        ))
        .unwrap();
        let r = &cfg.routes[0];
        assert_eq!(r.rewrite_path("/v1/chat"), "/api/chat");
        assert_eq!(r.rewrite_path("/other"), "/other");
        assert_eq!(r.max_body_size(), Some(50));
        assert_eq!(r.timeout(), Duration::from_secs(5));
        assert_eq!(r.cache_ttl(), Some(Duration::from_secs(300)));
        let ops = r.response_header_ops();
        assert!(matches!(ops[0], HeaderOp::Remove(_)));
        assert!(matches!(ops[1], HeaderOp::Set(_, _)));
    }
}
