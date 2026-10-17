//! moxy.xml (MML v1) -> raw [`Config`].
//!
//! This step checks structure only: known elements, known attributes and required
//! attributes. Semantic validation happens in [`super::resolve`].

use super::model::*;
use roxmltree::{Document, Node};

pub fn parse(xml: &str) -> Result<Config, Vec<String>> {
    let doc = Document::parse(xml).map_err(|e| vec![format!("invalid XML: {e}")])?;
    let mut p = Parser {
        doc: &doc,
        errors: Vec::new(),
    };
    let cfg = p.moxy(doc.root_element());
    if p.errors.is_empty() {
        Ok(cfg)
    } else {
        Err(p.errors)
    }
}

struct Parser<'a, 'i> {
    doc: &'a Document<'i>,
    errors: Vec<String>,
}

impl Parser<'_, '_> {
    fn loc(&self, n: Node) -> String {
        let pos = self.doc.text_pos_at(n.range().start);
        format!("line {}", pos.row)
    }

    fn err(&mut self, n: Node, msg: impl std::fmt::Display) {
        let loc = self.loc(n);
        self.errors.push(format!("{loc}: {msg}"));
    }

    fn check_attrs(&mut self, n: Node, allowed: &[&str]) {
        for a in n.attributes() {
            if !allowed.contains(&a.name()) {
                self.err(
                    n,
                    format!("unknown attribute '{}' on <{}>", a.name(), n.tag_name().name()),
                );
            }
        }
    }

    fn req(&mut self, n: Node, name: &str) -> String {
        match n.attribute(name) {
            Some(v) => v.to_string(),
            None => {
                self.err(
                    n,
                    format!("<{}> requires attribute '{name}'", n.tag_name().name()),
                );
                String::new()
            }
        }
    }

    fn opt(&self, n: Node, name: &str) -> Option<String> {
        n.attribute(name).map(str::to_string)
    }

    fn children<'a, 'i>(&mut self, n: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
        let mut out = Vec::new();
        for c in n.children() {
            if c.is_element() {
                out.push(c);
            } else if c.is_text() && !c.text().unwrap_or("").trim().is_empty() {
                self.err(c, format!("unexpected text inside <{}>", n.tag_name().name()));
            }
        }
        out
    }

    fn unknown(&mut self, n: Node, parent: &str) {
        self.err(
            n,
            format!("unknown element <{}> in <{parent}>", n.tag_name().name()),
        );
    }

    fn moxy(&mut self, root: Node) -> Config {
        let mut cfg = Config::default();
        if root.tag_name().name() != "moxy" {
            self.err(root, "root element must be <moxy>");
            return cfg;
        }
        self.check_attrs(root, &["version"]);
        cfg.version = self.req(root, "version");
        let mut seen_server = false;
        for c in self.children(root) {
            match c.tag_name().name() {
                "server" => {
                    if seen_server {
                        self.err(c, "duplicate <server> element");
                    }
                    seen_server = true;
                    self.check_attrs(c, &["host", "port"]);
                    for g in self.children(c) {
                        self.unknown(g, "server");
                    }
                    cfg.server = Server {
                        host: self.opt(c, "host"),
                        port: self.opt(c, "port"),
                    };
                }
                "auth" => {
                    if cfg.auth.is_some() {
                        self.err(c, "duplicate <auth> element");
                    }
                    cfg.auth = Some(self.auth(c));
                }
                "route" => {
                    if let Some(r) = self.route(c) {
                        cfg.routes.push(r);
                    }
                }
                _ => self.unknown(c, "moxy"),
            }
        }
        cfg
    }

    fn auth(&mut self, n: Node) -> Auth {
        self.check_attrs(n, &["issuer", "client-id", "client-secret", "session-ttl"]);
        let mut auth = Auth {
            issuer: self.req(n, "issuer"),
            client_id: self.req(n, "client-id"),
            client_secret: self.opt(n, "client-secret"),
            session_ttl: self.opt(n, "session-ttl"),
            admins: Vec::new(),
        };
        for c in self.children(n) {
            match c.tag_name().name() {
                "admin" => {
                    self.check_attrs(c, &["email"]);
                    let email = self.req(c, "email");
                    auth.admins.push(email);
                }
                _ => self.unknown(c, "auth"),
            }
        }
        auth
    }

    fn route(&mut self, n: Node) -> Option<Route> {
        self.check_attrs(n, &["path", "auth", "description"]);
        let path = self.req(n, "path");
        let mut middleware = Vec::new();
        let mut upstream: Option<Upstream> = None;
        for c in self.children(n) {
            let name = c.tag_name().name();
            match name {
                "upstream" | "load-balance" => {
                    if upstream.is_some() {
                        self.err(
                            c,
                            format!("route {path} has more than one <upstream>/<load-balance>"),
                        );
                    }
                    upstream = Some(if name == "upstream" {
                        self.check_attrs(c, &["url"]);
                        for g in self.children(c) {
                            self.unknown(g, "upstream");
                        }
                        Upstream::Upstream {
                            url: self.req(c, "url"),
                        }
                    } else {
                        self.check_attrs(c, &["strategy"]);
                        let mut urls = Vec::new();
                        for g in self.children(c) {
                            if g.tag_name().name() == "upstream" {
                                self.check_attrs(g, &["url"]);
                                urls.push(self.req(g, "url"));
                            } else {
                                self.unknown(g, "load-balance");
                            }
                        }
                        Upstream::LoadBalance {
                            strategy: self.opt(c, "strategy"),
                            upstreams: urls,
                        }
                    });
                }
                "headers" => {
                    self.check_attrs(c, &[]);
                    let mut ops = Vec::new();
                    for g in self.children(c) {
                        match g.tag_name().name() {
                            "set" | "add" => {
                                self.check_attrs(g, &["name", "value"]);
                                let (name, value) = (self.req(g, "name"), self.req(g, "value"));
                                ops.push(if g.tag_name().name() == "set" {
                                    HeaderOp::Set { name, value }
                                } else {
                                    HeaderOp::Add { name, value }
                                });
                            }
                            "remove" => {
                                self.check_attrs(g, &["name"]);
                                ops.push(HeaderOp::Remove {
                                    name: self.req(g, "name"),
                                });
                            }
                            _ => self.unknown(g, "headers"),
                        }
                    }
                    middleware.push(Middleware::Headers { ops });
                }
                "request" => {
                    self.check_attrs(c, &[]);
                    let mut ops = Vec::new();
                    for g in self.children(c) {
                        match g.tag_name().name() {
                            "rewrite-path" => {
                                self.check_attrs(g, &["from", "to"]);
                                ops.push(RequestOp::RewritePath {
                                    from: self.req(g, "from"),
                                    to: self.req(g, "to"),
                                });
                            }
                            "max-body-size" => {
                                self.check_attrs(g, &["bytes"]);
                                ops.push(RequestOp::MaxBodySize {
                                    bytes: self.req(g, "bytes"),
                                });
                            }
                            "timeout" => {
                                self.check_attrs(g, &["duration"]);
                                ops.push(RequestOp::Timeout {
                                    duration: self.req(g, "duration"),
                                });
                            }
                            _ => self.unknown(g, "request"),
                        }
                    }
                    middleware.push(Middleware::Request { ops });
                }
                "response" => {
                    self.check_attrs(c, &[]);
                    let mut ops = Vec::new();
                    for g in self.children(c) {
                        match g.tag_name().name() {
                            "set" | "add" => {
                                self.check_attrs(g, &["name", "value"]);
                                let (name, value) = (self.req(g, "name"), self.req(g, "value"));
                                ops.push(if g.tag_name().name() == "set" {
                                    ResponseOp::Set { name, value }
                                } else {
                                    ResponseOp::Add { name, value }
                                });
                            }
                            "remove" => {
                                self.check_attrs(g, &["name"]);
                                ops.push(ResponseOp::Remove {
                                    name: self.req(g, "name"),
                                });
                            }
                            "cache" => {
                                self.check_attrs(g, &["ttl"]);
                                ops.push(ResponseOp::Cache {
                                    ttl: self.req(g, "ttl"),
                                });
                            }
                            _ => self.unknown(g, "response"),
                        }
                    }
                    middleware.push(Middleware::Response { ops });
                }
                "ai" => {
                    self.check_attrs(c, &["provider", "model"]);
                    self.no_children(c);
                    middleware.push(Middleware::Ai {
                        provider: self.req(c, "provider"),
                        model: self.opt(c, "model"),
                    });
                }
                "rate-limit" => {
                    self.check_attrs(c, &["requests", "per", "key"]);
                    self.no_children(c);
                    middleware.push(Middleware::RateLimit {
                        requests: self.req(c, "requests"),
                        per: self.req(c, "per"),
                        key: self.opt(c, "key"),
                    });
                }
                "token-budget" => {
                    self.check_attrs(c, &["tokens", "per", "key"]);
                    self.no_children(c);
                    middleware.push(Middleware::TokenBudget {
                        tokens: self.req(c, "tokens"),
                        per: self.req(c, "per"),
                        key: self.opt(c, "key"),
                    });
                }
                _ => self.unknown(c, "route"),
            }
        }
        match upstream {
            Some(upstream) => Some(Route {
                path,
                auth: self.opt(n, "auth"),
                description: self.opt(n, "description"),
                middleware,
                upstream,
            }),
            None => {
                self.err(n, format!("route {path} has no <upstream> or <load-balance>"));
                None
            }
        }
    }

    fn no_children(&mut self, n: Node) {
        let name = n.tag_name().name().to_string();
        for g in self.children(n) {
            self.unknown(g, &name);
        }
    }
}
