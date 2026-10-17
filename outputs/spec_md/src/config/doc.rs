//! The document model of moxy.xml.
//!
//! This is a faithful, unresolved representation of the file: attribute values
//! are kept exactly as written (including `${VAR}` references), so that the
//! admin interface can show and write back the configuration without ever
//! exposing or persisting secrets. The same model is the JSON form used by the
//! JSON API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDoc {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub server: Option<ServerDoc>,
    #[serde(default)]
    pub auth: Option<AuthDoc>,
    #[serde(default)]
    pub routes: Vec<RouteDoc>,
}

fn default_version() -> String {
    "1".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ServerDoc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AuthDoc {
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ttl: Option<String>,
    #[serde(default)]
    pub admins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDoc {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub middleware: Vec<MiddlewareDoc>,
    #[serde(default)]
    pub destination: Option<DestinationDoc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MiddlewareDoc {
    Headers {
        #[serde(default)]
        ops: Vec<HeaderOpDoc>,
    },
    Request {
        #[serde(default)]
        ops: Vec<RequestOpDoc>,
    },
    Response {
        #[serde(default)]
        ops: Vec<ResponseOpDoc>,
    },
    Ai {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    RateLimit {
        requests: String,
        per: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    TokenBudget {
        tokens: String,
        per: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HeaderOpDoc {
    Set { name: String, value: String },
    Add { name: String, value: String },
    Remove { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RequestOpDoc {
    RewritePath { from: String, to: String },
    MaxBodySize { bytes: String },
    Timeout { duration: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ResponseOpDoc {
    Set { name: String, value: String },
    Add { name: String, value: String },
    Remove { name: String },
    Cache { ttl: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DestinationDoc {
    Upstream {
        url: String,
    },
    LoadBalance {
        strategy: String,
        upstreams: Vec<String>,
    },
}
