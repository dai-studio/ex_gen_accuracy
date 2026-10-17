//! The raw MML document model.
//!
//! Attribute values are kept exactly as written (including `${NAME}` references) so that
//! the document can be written back to moxy.xml without leaking secrets. The same model
//! is used as the JSON representation in `/api/config`.

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub server: Server,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    #[serde(default)]
    pub routes: Vec<Route>,
}

fn default_version() -> String {
    "1".into()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: default_version(),
            server: Server::default(),
            auth: None,
            routes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "opt_string_or_number"
    )]
    pub port: Option<String>,
}

impl Default for Server {
    fn default() -> Self {
        Server {
            host: None,
            port: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Auth {
    pub issuer: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ttl: Option<String>,
    #[serde(default)]
    pub admins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub middleware: Vec<Middleware>,
    pub upstream: Upstream,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Middleware {
    Headers {
        #[serde(default)]
        ops: Vec<HeaderOp>,
    },
    Request {
        #[serde(default)]
        ops: Vec<RequestOp>,
    },
    Response {
        #[serde(default)]
        ops: Vec<ResponseOp>,
    },
    Ai {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    RateLimit {
        #[serde(deserialize_with = "string_or_number")]
        requests: String,
        per: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    TokenBudget {
        #[serde(deserialize_with = "string_or_number")]
        tokens: String,
        per: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum HeaderOp {
    Set { name: String, value: String },
    Add { name: String, value: String },
    Remove { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum RequestOp {
    RewritePath {
        from: String,
        to: String,
    },
    MaxBodySize {
        #[serde(deserialize_with = "string_or_number")]
        bytes: String,
    },
    Timeout {
        duration: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ResponseOp {
    Set { name: String, value: String },
    Add { name: String, value: String },
    Remove { name: String },
    Cache { ttl: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Upstream {
    Upstream {
        url: String,
    },
    LoadBalance {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strategy: Option<String>,
        upstreams: Vec<String>,
    },
}

fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SN {
        S(String),
        N(serde_json::Number),
    }
    Ok(match SN::deserialize(d)? {
        SN::S(s) => s,
        SN::N(n) => n.to_string(),
    })
}

fn opt_string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SN {
        S(String),
        N(serde_json::Number),
    }
    Ok(Option::<SN>::deserialize(d)?.map(|v| match v {
        SN::S(s) => s,
        SN::N(n) => n.to_string(),
    }))
}
