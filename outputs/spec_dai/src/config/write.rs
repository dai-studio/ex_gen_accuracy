//! Raw [`Config`] -> moxy.xml text.
//!
//! Comments and original formatting are not preserved (see the spec); `${NAME}`
//! references are written back verbatim because the raw model never contains the
//! substituted values.

use super::model::*;
use std::fmt::Write;

pub fn to_xml(cfg: &Config) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(out, "<moxy version=\"{}\">", esc(&cfg.version));
    let mut server = String::from("  <server");
    attr(&mut server, "host", cfg.server.host.as_deref());
    attr(&mut server, "port", cfg.server.port.as_deref());
    out.push_str(&server);
    out.push_str(" />\n");

    if let Some(auth) = &cfg.auth {
        let mut a = String::from("  <auth");
        attr(&mut a, "issuer", Some(&auth.issuer));
        attr(&mut a, "client-id", Some(&auth.client_id));
        attr(&mut a, "client-secret", auth.client_secret.as_deref());
        attr(&mut a, "session-ttl", auth.session_ttl.as_deref());
        if auth.admins.is_empty() {
            out.push_str(&a);
            out.push_str(" />\n");
        } else {
            out.push_str(&a);
            out.push_str(">\n");
            for email in &auth.admins {
                let _ = writeln!(out, "    <admin email=\"{}\" />", esc(email));
            }
            out.push_str("  </auth>\n");
        }
    }

    for r in &cfg.routes {
        out.push('\n');
        let mut open = String::from("  <route");
        attr(&mut open, "path", Some(&r.path));
        attr(&mut open, "auth", r.auth.as_deref());
        attr(&mut open, "description", r.description.as_deref());
        out.push_str(&open);
        out.push_str(">\n");
        for m in &r.middleware {
            write_middleware(&mut out, m);
        }
        match &r.upstream {
            Upstream::Upstream { url } => {
                let _ = writeln!(out, "    <upstream url=\"{}\" />", esc(url));
            }
            Upstream::LoadBalance {
                strategy,
                upstreams,
            } => {
                let mut lb = String::from("    <load-balance");
                attr(&mut lb, "strategy", strategy.as_deref());
                out.push_str(&lb);
                out.push_str(">\n");
                for u in upstreams {
                    let _ = writeln!(out, "      <upstream url=\"{}\" />", esc(u));
                }
                out.push_str("    </load-balance>\n");
            }
        }
        out.push_str("  </route>\n");
    }
    out.push_str("</moxy>\n");
    out
}

fn write_middleware(out: &mut String, m: &Middleware) {
    match m {
        Middleware::Headers { ops } => {
            out.push_str("    <headers>\n");
            for op in ops {
                match op {
                    HeaderOp::Set { name, value } => kv_line(out, "set", name, value),
                    HeaderOp::Add { name, value } => kv_line(out, "add", name, value),
                    HeaderOp::Remove { name } => {
                        let _ = writeln!(out, "      <remove name=\"{}\" />", esc(name));
                    }
                }
            }
            out.push_str("    </headers>\n");
        }
        Middleware::Request { ops } => {
            out.push_str("    <request>\n");
            for op in ops {
                let _ = match op {
                    RequestOp::RewritePath { from, to } => writeln!(
                        out,
                        "      <rewrite-path from=\"{}\" to=\"{}\" />",
                        esc(from),
                        esc(to)
                    ),
                    RequestOp::MaxBodySize { bytes } => {
                        writeln!(out, "      <max-body-size bytes=\"{}\" />", esc(bytes))
                    }
                    RequestOp::Timeout { duration } => {
                        writeln!(out, "      <timeout duration=\"{}\" />", esc(duration))
                    }
                };
            }
            out.push_str("    </request>\n");
        }
        Middleware::Response { ops } => {
            out.push_str("    <response>\n");
            for op in ops {
                match op {
                    ResponseOp::Set { name, value } => kv_line(out, "set", name, value),
                    ResponseOp::Add { name, value } => kv_line(out, "add", name, value),
                    ResponseOp::Remove { name } => {
                        let _ = writeln!(out, "      <remove name=\"{}\" />", esc(name));
                    }
                    ResponseOp::Cache { ttl } => {
                        let _ = writeln!(out, "      <cache ttl=\"{}\" />", esc(ttl));
                    }
                }
            }
            out.push_str("    </response>\n");
        }
        Middleware::Ai { provider, model } => {
            let mut s = String::from("    <ai");
            attr(&mut s, "provider", Some(provider));
            attr(&mut s, "model", model.as_deref());
            out.push_str(&s);
            out.push_str(" />\n");
        }
        Middleware::RateLimit { requests, per, key } => {
            let mut s = String::from("    <rate-limit");
            attr(&mut s, "requests", Some(requests));
            attr(&mut s, "per", Some(per));
            attr(&mut s, "key", key.as_deref());
            out.push_str(&s);
            out.push_str(" />\n");
        }
        Middleware::TokenBudget { tokens, per, key } => {
            let mut s = String::from("    <token-budget");
            attr(&mut s, "tokens", Some(tokens));
            attr(&mut s, "per", Some(per));
            attr(&mut s, "key", key.as_deref());
            out.push_str(&s);
            out.push_str(" />\n");
        }
    }
}

fn kv_line(out: &mut String, tag: &str, name: &str, value: &str) {
    let _ = writeln!(
        out,
        "      <{tag} name=\"{}\" value=\"{}\" />",
        esc(name),
        esc(value)
    );
}

fn attr(out: &mut String, name: &str, value: Option<&str>) {
    if let Some(v) = value {
        let _ = write!(out, " {name}=\"{}\"", esc(v));
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            '\n' => o.push_str("&#10;"),
            '\r' => o.push_str("&#13;"),
            '\t' => o.push_str("&#9;"),
            c => o.push(c),
        }
    }
    o
}
