//! Reading and writing Moxy Markup Language (moxy.xml).
//!
//! Parsing here is purely structural: it checks element and attribute names
//! and produces a [`ConfigDoc`]. Semantic validation happens when the
//! document is compiled into a runtime configuration.

use super::ValidationError;
use super::doc::*;
use roxmltree::{Document, Node};

pub fn parse(text: &str) -> Result<ConfigDoc, Vec<ValidationError>> {
    let doc = Document::parse(text).map_err(|e| {
        vec![ValidationError::new(
            "moxy.xml",
            format!("XML syntax error: {e}"),
        )]
    })?;
    let mut p = Parser {
        doc: &doc,
        errors: Vec::new(),
    };
    let result = p.parse_root(doc.root_element());
    if p.errors.is_empty() {
        Ok(result)
    } else {
        Err(p.errors)
    }
}

struct Parser<'a, 'i> {
    doc: &'a Document<'i>,
    errors: Vec<ValidationError>,
}

impl<'a, 'i> Parser<'a, 'i> {
    fn loc(&self, node: Node<'a, 'i>) -> String {
        let pos = self.doc.text_pos_at(node.range().start);
        format!("line {}: <{}>", pos.row, node.tag_name().name())
    }

    fn err(&mut self, node: Node<'a, 'i>, msg: impl Into<String>) {
        let loc = self.loc(node);
        self.errors.push(ValidationError::new(loc, msg));
    }

    /// Check that `node` only carries the given attributes.
    fn check_attrs(&mut self, node: Node<'a, 'i>, allowed: &[&str]) {
        for attr in node.attributes() {
            if !allowed.contains(&attr.name()) {
                self.err(node, format!("unknown attribute '{}'", attr.name()));
            }
        }
    }

    fn required(&mut self, node: Node<'a, 'i>, name: &str) -> String {
        match node.attribute(name) {
            Some(v) => v.to_string(),
            None => {
                self.err(node, format!("missing required attribute '{name}'"));
                String::new()
            }
        }
    }

    fn optional(&self, node: Node<'a, 'i>, name: &str) -> Option<String> {
        node.attribute(name).map(str::to_string)
    }

    /// Element children, reporting stray text content.
    fn elements(&mut self, node: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
        let mut out = Vec::new();
        for child in node.children() {
            if child.is_element() {
                out.push(child);
            } else if child.is_text() && child.text().is_some_and(|t| !t.trim().is_empty()) {
                self.err(node, "unexpected text content");
            }
        }
        out
    }

    fn no_children(&mut self, node: Node<'a, 'i>) {
        for child in self.elements(node) {
            self.err(
                child,
                format!(
                    "unknown element <{}> inside <{}>",
                    child.tag_name().name(),
                    node.tag_name().name()
                ),
            );
        }
    }

    fn parse_root(&mut self, root: Node<'a, 'i>) -> ConfigDoc {
        let mut doc = ConfigDoc {
            version: String::new(),
            server: None,
            auth: None,
            routes: Vec::new(),
        };
        if root.tag_name().name() != "moxy" {
            self.err(root, "the root element must be <moxy>");
            return doc;
        }
        self.check_attrs(root, &["version"]);
        doc.version = self.required(root, "version");
        for child in self.elements(root) {
            match child.tag_name().name() {
                "server" => {
                    if doc.server.is_some() {
                        self.err(child, "only one <server> element is allowed");
                    }
                    self.check_attrs(child, &["host", "port"]);
                    self.no_children(child);
                    doc.server = Some(ServerDoc {
                        host: self.optional(child, "host"),
                        port: self.optional(child, "port"),
                    });
                }
                "auth" => {
                    if doc.auth.is_some() {
                        self.err(child, "only one <auth> element is allowed");
                    }
                    doc.auth = Some(self.parse_auth(child));
                }
                "route" => {
                    let r = self.parse_route(child);
                    doc.routes.push(r);
                }
                other => self.err(child, format!("unknown element <{other}> inside <moxy>")),
            }
        }
        doc
    }

    fn parse_auth(&mut self, node: Node<'a, 'i>) -> AuthDoc {
        self.check_attrs(
            node,
            &["issuer", "client-id", "client-secret", "session-ttl"],
        );
        let mut auth = AuthDoc {
            issuer: self.required(node, "issuer"),
            client_id: self.required(node, "client-id"),
            client_secret: self.optional(node, "client-secret"),
            session_ttl: self.optional(node, "session-ttl"),
            admins: Vec::new(),
        };
        for child in self.elements(node) {
            match child.tag_name().name() {
                "admin" => {
                    self.check_attrs(child, &["email"]);
                    self.no_children(child);
                    auth.admins.push(self.required(child, "email"));
                }
                other => self.err(child, format!("unknown element <{other}> inside <auth>")),
            }
        }
        auth
    }

    fn parse_route(&mut self, node: Node<'a, 'i>) -> RouteDoc {
        self.check_attrs(node, &["path", "auth", "description"]);
        let mut route = RouteDoc {
            path: self.required(node, "path"),
            auth: self.optional(node, "auth"),
            description: self.optional(node, "description"),
            middleware: Vec::new(),
            destination: None,
        };
        for child in self.elements(node) {
            let name = child.tag_name().name();
            if route.destination.is_some() {
                self.err(
                    child,
                    "the destination (<upstream> or <load-balance>) must be the last element of a route",
                );
            }
            match name {
                "upstream" => {
                    self.check_attrs(child, &["url"]);
                    self.no_children(child);
                    route.destination = Some(DestinationDoc::Upstream {
                        url: self.required(child, "url"),
                    });
                }
                "load-balance" => {
                    self.check_attrs(child, &["strategy"]);
                    let strategy = self.required(child, "strategy");
                    let mut upstreams = Vec::new();
                    for up in self.elements(child) {
                        if up.tag_name().name() == "upstream" {
                            self.check_attrs(up, &["url"]);
                            self.no_children(up);
                            upstreams.push(self.required(up, "url"));
                        } else {
                            self.err(
                                up,
                                format!(
                                    "unknown element <{}> inside <load-balance>",
                                    up.tag_name().name()
                                ),
                            );
                        }
                    }
                    route.destination = Some(DestinationDoc::LoadBalance {
                        strategy,
                        upstreams,
                    });
                }
                "headers" => {
                    self.check_attrs(child, &[]);
                    let mut ops = Vec::new();
                    for op in self.elements(child) {
                        if let Some(h) = self.parse_header_op(op, "headers") {
                            ops.push(h);
                        }
                    }
                    route.middleware.push(MiddlewareDoc::Headers { ops });
                }
                "request" => {
                    self.check_attrs(child, &[]);
                    let mut ops = Vec::new();
                    for op in self.elements(child) {
                        match op.tag_name().name() {
                            "rewrite-path" => {
                                self.check_attrs(op, &["from", "to"]);
                                self.no_children(op);
                                ops.push(RequestOpDoc::RewritePath {
                                    from: self.required(op, "from"),
                                    to: self.required(op, "to"),
                                });
                            }
                            "max-body-size" => {
                                self.check_attrs(op, &["bytes"]);
                                self.no_children(op);
                                ops.push(RequestOpDoc::MaxBodySize {
                                    bytes: self.required(op, "bytes"),
                                });
                            }
                            "timeout" => {
                                self.check_attrs(op, &["duration"]);
                                self.no_children(op);
                                ops.push(RequestOpDoc::Timeout {
                                    duration: self.required(op, "duration"),
                                });
                            }
                            other => {
                                self.err(op, format!("unknown element <{other}> inside <request>"))
                            }
                        }
                    }
                    route.middleware.push(MiddlewareDoc::Request { ops });
                }
                "response" => {
                    self.check_attrs(child, &[]);
                    let mut ops = Vec::new();
                    for op in self.elements(child) {
                        if op.tag_name().name() == "cache" {
                            self.check_attrs(op, &["ttl"]);
                            self.no_children(op);
                            ops.push(ResponseOpDoc::Cache {
                                ttl: self.required(op, "ttl"),
                            });
                        } else if let Some(h) = self.parse_header_op(op, "response") {
                            ops.push(match h {
                                HeaderOpDoc::Set { name, value } => {
                                    ResponseOpDoc::Set { name, value }
                                }
                                HeaderOpDoc::Add { name, value } => {
                                    ResponseOpDoc::Add { name, value }
                                }
                                HeaderOpDoc::Remove { name } => ResponseOpDoc::Remove { name },
                            });
                        }
                    }
                    route.middleware.push(MiddlewareDoc::Response { ops });
                }
                "ai" => {
                    self.check_attrs(child, &["provider", "model"]);
                    self.no_children(child);
                    route.middleware.push(MiddlewareDoc::Ai {
                        provider: self.required(child, "provider"),
                        model: self.optional(child, "model"),
                    });
                }
                "rate-limit" => {
                    self.check_attrs(child, &["requests", "per", "key"]);
                    self.no_children(child);
                    route.middleware.push(MiddlewareDoc::RateLimit {
                        requests: self.required(child, "requests"),
                        per: self.required(child, "per"),
                        key: self.optional(child, "key"),
                    });
                }
                "token-budget" => {
                    self.check_attrs(child, &["tokens", "per", "key"]);
                    self.no_children(child);
                    route.middleware.push(MiddlewareDoc::TokenBudget {
                        tokens: self.required(child, "tokens"),
                        per: self.required(child, "per"),
                        key: self.optional(child, "key"),
                    });
                }
                other => self.err(child, format!("unknown element <{other}> inside <route>")),
            }
        }
        route
    }

    fn parse_header_op(&mut self, op: Node<'a, 'i>, parent: &str) -> Option<HeaderOpDoc> {
        match op.tag_name().name() {
            "set" | "add" => {
                self.check_attrs(op, &["name", "value"]);
                self.no_children(op);
                let name = self.required(op, "name");
                let value = self.required(op, "value");
                Some(if op.tag_name().name() == "set" {
                    HeaderOpDoc::Set { name, value }
                } else {
                    HeaderOpDoc::Add { name, value }
                })
            }
            "remove" => {
                self.check_attrs(op, &["name"]);
                self.no_children(op);
                Some(HeaderOpDoc::Remove {
                    name: self.required(op, "name"),
                })
            }
            other => {
                self.err(op, format!("unknown element <{other}> inside <{parent}>"));
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out
}

struct Writer {
    out: String,
}

impl Writer {
    fn open(&mut self, indent: usize, name: &str, attrs: &[(&str, Option<&str>)], empty: bool) {
        self.out.push_str(&"  ".repeat(indent));
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in attrs {
            if let Some(v) = v {
                self.out.push_str(&format!(" {k}=\"{}\"", escape(v)));
            }
        }
        self.out.push_str(if empty { "/>\n" } else { ">\n" });
    }

    fn close(&mut self, indent: usize, name: &str) {
        self.out.push_str(&"  ".repeat(indent));
        self.out.push_str(&format!("</{name}>\n"));
    }

    fn header_op(&mut self, indent: usize, op: &HeaderOpDoc) {
        match op {
            HeaderOpDoc::Set { name, value } => self.open(
                indent,
                "set",
                &[("name", Some(name)), ("value", Some(value))],
                true,
            ),
            HeaderOpDoc::Add { name, value } => self.open(
                indent,
                "add",
                &[("name", Some(name)), ("value", Some(value))],
                true,
            ),
            HeaderOpDoc::Remove { name } => {
                self.open(indent, "remove", &[("name", Some(name))], true)
            }
        }
    }
}

/// Serialise a document as moxy.xml. Comments and original formatting are not preserved.
pub fn write(doc: &ConfigDoc) -> String {
    let mut w = Writer {
        out: String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"),
    };
    w.open(0, "moxy", &[("version", Some(&doc.version))], false);
    if let Some(server) = &doc.server {
        w.open(
            1,
            "server",
            &[
                ("host", server.host.as_deref()),
                ("port", server.port.as_deref()),
            ],
            true,
        );
    }
    if let Some(auth) = &doc.auth {
        w.open(
            1,
            "auth",
            &[
                ("issuer", Some(&auth.issuer)),
                ("client-id", Some(&auth.client_id)),
                ("client-secret", auth.client_secret.as_deref()),
                ("session-ttl", auth.session_ttl.as_deref()),
            ],
            auth.admins.is_empty(),
        );
        if !auth.admins.is_empty() {
            for email in &auth.admins {
                w.open(2, "admin", &[("email", Some(email))], true);
            }
            w.close(1, "auth");
        }
    }
    for route in &doc.routes {
        w.open(
            1,
            "route",
            &[
                ("path", Some(&route.path)),
                ("auth", route.auth.as_deref()),
                ("description", route.description.as_deref()),
            ],
            route.middleware.is_empty() && route.destination.is_none(),
        );
        if route.middleware.is_empty() && route.destination.is_none() {
            continue;
        }
        for mw in &route.middleware {
            match mw {
                MiddlewareDoc::Headers { ops } => {
                    w.open(2, "headers", &[], ops.is_empty());
                    if !ops.is_empty() {
                        for op in ops {
                            w.header_op(3, op);
                        }
                        w.close(2, "headers");
                    }
                }
                MiddlewareDoc::Request { ops } => {
                    w.open(2, "request", &[], ops.is_empty());
                    if !ops.is_empty() {
                        for op in ops {
                            match op {
                                RequestOpDoc::RewritePath { from, to } => w.open(
                                    3,
                                    "rewrite-path",
                                    &[("from", Some(from)), ("to", Some(to))],
                                    true,
                                ),
                                RequestOpDoc::MaxBodySize { bytes } => {
                                    w.open(3, "max-body-size", &[("bytes", Some(bytes))], true)
                                }
                                RequestOpDoc::Timeout { duration } => {
                                    w.open(3, "timeout", &[("duration", Some(duration))], true)
                                }
                            }
                        }
                        w.close(2, "request");
                    }
                }
                MiddlewareDoc::Response { ops } => {
                    w.open(2, "response", &[], ops.is_empty());
                    if !ops.is_empty() {
                        for op in ops {
                            match op {
                                ResponseOpDoc::Cache { ttl } => {
                                    w.open(3, "cache", &[("ttl", Some(ttl))], true)
                                }
                                ResponseOpDoc::Set { name, value } => w.header_op(
                                    3,
                                    &HeaderOpDoc::Set {
                                        name: name.clone(),
                                        value: value.clone(),
                                    },
                                ),
                                ResponseOpDoc::Add { name, value } => w.header_op(
                                    3,
                                    &HeaderOpDoc::Add {
                                        name: name.clone(),
                                        value: value.clone(),
                                    },
                                ),
                                ResponseOpDoc::Remove { name } => {
                                    w.header_op(3, &HeaderOpDoc::Remove { name: name.clone() })
                                }
                            }
                        }
                        w.close(2, "response");
                    }
                }
                MiddlewareDoc::Ai { provider, model } => w.open(
                    2,
                    "ai",
                    &[("provider", Some(provider)), ("model", model.as_deref())],
                    true,
                ),
                MiddlewareDoc::RateLimit { requests, per, key } => w.open(
                    2,
                    "rate-limit",
                    &[
                        ("requests", Some(requests)),
                        ("per", Some(per)),
                        ("key", key.as_deref()),
                    ],
                    true,
                ),
                MiddlewareDoc::TokenBudget { tokens, per, key } => w.open(
                    2,
                    "token-budget",
                    &[
                        ("tokens", Some(tokens)),
                        ("per", Some(per)),
                        ("key", key.as_deref()),
                    ],
                    true,
                ),
            }
        }
        match &route.destination {
            Some(DestinationDoc::Upstream { url }) => {
                w.open(2, "upstream", &[("url", Some(url))], true)
            }
            Some(DestinationDoc::LoadBalance {
                strategy,
                upstreams,
            }) => {
                w.open(2, "load-balance", &[("strategy", Some(strategy))], false);
                for url in upstreams {
                    w.open(3, "upstream", &[("url", Some(url))], true);
                }
                w.close(2, "load-balance");
            }
            None => {}
        }
        w.close(1, "route");
    }
    w.close(0, "moxy");
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<moxy version="1">
  <!-- comment -->
  <server host="127.0.0.1" port="8080"/>
  <auth issuer="https://idp.example.com" client-id="moxy" client-secret="${MOXY_OIDC_CLIENT_SECRET}" session-ttl="24h">
    <admin email="admin@example.com"/>
  </auth>
  <route path="/openai/*" auth="api-key" description="OpenAI models">
    <headers>
      <set name="Authorization" value="Bearer ${OPENAI_API_KEY}"/>
      <remove name="X-Debug"/>
    </headers>
    <request>
      <rewrite-path from="/old" to="/new"/>
      <max-body-size bytes="1048576"/>
      <timeout duration="30s"/>
    </request>
    <response>
      <add name="X-Served-By" value="moxy"/>
      <cache ttl="5m"/>
    </response>
    <ai provider="openai" model="gpt-4o-mini"/>
    <rate-limit requests="60" per="1m" key="api-key"/>
    <token-budget tokens="100000" per="1d" key="user"/>
    <upstream url="https://api.openai.com/v1"/>
  </route>
  <route path="/local/*" auth="none">
    <load-balance strategy="round-robin">
      <upstream url="http://127.0.0.1:11434"/>
      <upstream url="http://127.0.0.1:11435"/>
    </load-balance>
  </route>
</moxy>"#;

    #[test]
    fn parses_sample() {
        let doc = parse(SAMPLE).unwrap();
        assert_eq!(doc.version, "1");
        assert_eq!(doc.server.as_ref().unwrap().port.as_deref(), Some("8080"));
        let auth = doc.auth.as_ref().unwrap();
        assert_eq!(auth.admins, vec!["admin@example.com"]);
        assert_eq!(
            auth.client_secret.as_deref(),
            Some("${MOXY_OIDC_CLIENT_SECRET}")
        );
        assert_eq!(doc.routes.len(), 2);
        assert_eq!(doc.routes[0].middleware.len(), 6);
        assert!(matches!(
            doc.routes[1].destination,
            Some(DestinationDoc::LoadBalance { ref upstreams, .. }) if upstreams.len() == 2
        ));
    }

    #[test]
    fn roundtrip() {
        let doc = parse(SAMPLE).unwrap();
        let written = write(&doc);
        let again = parse(&written).unwrap();
        assert_eq!(doc, again);
    }

    #[test]
    fn rejects_unknown_elements_and_attributes() {
        let bad = r#"<moxy version="1"><route path="/a/*"><bogus/><upstream url="http://x" extra="1"/></route></moxy>"#;
        let errs = parse(bad).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.message.contains("unknown element <bogus>"))
        );
        assert!(
            errs.iter()
                .any(|e| e.message.contains("unknown attribute 'extra'"))
        );
    }

    #[test]
    fn rejects_wrong_root_and_syntax() {
        assert!(parse("<config/>").is_err());
        assert!(parse("<moxy version=\"1\">").is_err());
    }

    #[test]
    fn destination_must_be_last() {
        let bad = r#"<moxy version="1"><route path="/a/*"><upstream url="http://x"/><ai provider="openai"/></route></moxy>"#;
        let errs = parse(bad).unwrap_err();
        assert!(errs[0].message.contains("must be the last"));
    }

    #[test]
    fn escapes_attribute_values() {
        let mut doc = parse(SAMPLE).unwrap();
        doc.routes[0].description = Some("a \"quoted\" <thing> & more".into());
        let again = parse(&write(&doc)).unwrap();
        assert_eq!(again.routes[0].description, doc.routes[0].description);
    }
}
