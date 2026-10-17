//! moxy.xml: Moxy Markup Language (MML v1).

pub mod model;
pub mod parse;
pub mod resolve;
pub mod write;

pub use model::*;
pub use resolve::*;

use crate::util::sha256_hex;
use chrono::{DateTime, Utc};

/// A loaded, validated config together with its raw document.
#[derive(Debug, Clone)]
pub struct ActiveConfig {
    pub raw: Config,
    pub resolved: Resolved,
    /// Short content hash of the moxy.xml text that produced this config.
    pub version: String,
    pub loaded_at: DateTime<Utc>,
}

pub fn version_of(text: &str) -> String {
    sha256_hex(text.as_bytes())[..12].to_string()
}

pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Parse and validate MML text.
pub fn load_str(
    text: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ActiveConfig, Vec<String>> {
    let raw = parse::parse(text)?;
    let resolved = resolve::resolve(&raw, env)?;
    Ok(ActiveConfig {
        raw,
        resolved,
        version: version_of(text),
        loaded_at: Utc::now(),
    })
}

/// Validate a raw config (e.g. from /api) by round-tripping it through MML.
/// Returns the XML text to write on success.
pub fn validate_raw(
    cfg: &Config,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(String, ActiveConfig), Vec<String>> {
    let xml = write::to_xml(cfg);
    let active = load_str(&xml, env)?;
    Ok((xml, active))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    const FULL: &str = r#"<?xml version="1.0"?>
<moxy version="1">
  <!-- comment -->
  <server host="127.0.0.1" port="9090" />
  <auth issuer="https://idp.example.com" client-id="moxy" client-secret="${SECRET}" session-ttl="12h">
    <admin email="Admin@Example.com" />
  </auth>
  <route path="/openai/*" auth="api-key" description="OpenAI models">
    <headers>
      <set name="Authorization" value="Bearer ${OPENAI_KEY}" />
      <remove name="x-drop" />
    </headers>
    <request>
      <rewrite-path from="/v1" to="/" />
      <max-body-size bytes="1048576" />
      <timeout duration="30s" />
    </request>
    <ai provider="openai" model="gpt-4o-mini" />
    <rate-limit requests="10" per="1m" key="ip" />
    <token-budget tokens="1000" per="1d" key="user" />
    <response>
      <set name="x-moxy" value="1" />
      <cache ttl="5m" />
    </response>
    <upstream url="https://api.openai.com/v1" />
  </route>
  <route path="/lb/*" auth="none">
    <load-balance strategy="failover">
      <upstream url="http://127.0.0.1:1" />
      <upstream url="http://127.0.0.1:2" />
    </load-balance>
  </route>
  <route path="/exact" auth="session">
    <upstream url="http://example.com/x" />
  </route>
</moxy>"#;

    #[test]
    fn parses_full_document() {
        let env = env_of(&[("SECRET", "s3"), ("OPENAI_KEY", "sk-1")]);
        let a = load_str(FULL, &env).expect("valid");
        let r = &a.resolved;
        assert_eq!(r.port, 9090);
        let auth = r.auth.as_ref().unwrap();
        assert_eq!(auth.client_secret.as_deref(), Some("s3"));
        assert_eq!(auth.session_ttl.as_secs(), 12 * 3600);
        assert!(r.is_admin_email("admin@example.com"));
        assert_eq!(r.routes.len(), 3);
        let o = &r.routes[0];
        assert_eq!(o.auth, AuthMode::ApiKey);
        assert_eq!(o.middleware.len(), 6);
        assert_eq!(
            o.middleware[0],
            Mw::Headers(vec![
                HeaderOpR::Set("Authorization".into(), "Bearer sk-1".into()),
                HeaderOpR::Remove("x-drop".into())
            ])
        );
        assert_eq!(o.cache_ttl.unwrap().as_secs(), 300);
        assert!(o.has_token_budget);
        assert_eq!(o.ai.as_ref().unwrap().model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(r.routes[1].upstream.strategy, Strategy::Failover);
        assert_eq!(r.routes[2].auth, AuthMode::Session);
        // The raw model keeps the reference, never the secret.
        assert_eq!(
            a.raw.auth.as_ref().unwrap().client_secret.as_deref(),
            Some("${SECRET}")
        );
    }

    #[test]
    fn xml_round_trip() {
        let env = env_of(&[("SECRET", "s3"), ("OPENAI_KEY", "sk-1")]);
        let a = load_str(FULL, &env).unwrap();
        let xml = write::to_xml(&a.raw);
        assert!(xml.contains("${OPENAI_KEY}"));
        assert!(!xml.contains("sk-1"));
        let b = load_str(&xml, &env).unwrap();
        assert_eq!(a.raw, b.raw);
        // JSON round trip of the raw model.
        let json = serde_json::to_string(&a.raw).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a.raw);
    }

    #[test]
    fn defaults() {
        let a = load_str(
            r#"<moxy version="1"><route path="/x/*"><upstream url="http://h" /></route></moxy>"#,
            &env_of(&[]),
        )
        .unwrap();
        assert_eq!(a.resolved.host, "127.0.0.1");
        assert_eq!(a.resolved.port, 8080);
        assert!(a.resolved.auth.is_none());
        assert_eq!(a.resolved.routes[0].auth, AuthMode::ApiKey);
    }

    fn errors(xml: &str) -> Vec<String> {
        load_str(xml, &env_of(&[])).err().expect("should be invalid")
    }

    #[test]
    fn validation_errors() {
        let e = errors(
            r#"<moxy version="1">
              <route path="/a/*"><upstream url="http://h" /></route>
              <route path="/a/*"><upstream url="http://h" /></route>
            </moxy>"#,
        );
        assert!(e.iter().any(|m| m.contains("duplicate route path")), "{e:?}");

        for p in ["/", "/chat", "/chat/*", "/admin/x", "/auth/*", "/api/*", "/_app/*"] {
            let e = errors(&format!(
                r#"<moxy version="1"><route path="{p}"><upstream url="http://h" /></route></moxy>"#
            ));
            assert!(e.iter().any(|m| m.contains("reserved")), "{p}: {e:?}");
        }
        // Not reserved: shares a prefix but not a path segment.
        assert!(
            load_str(
                r#"<moxy version="1"><route path="/chatgpt/*"><upstream url="http://h" /></route></moxy>"#,
                &env_of(&[])
            )
            .is_ok()
        );

        let e = errors(r#"<moxy version="1"><route path="/a/*"><headers /></route></moxy>"#);
        assert!(e.iter().any(|m| m.contains("no <upstream>")), "{e:?}");

        let e = errors(
            r#"<moxy version="1"><route path="/a/*"><bogus /><upstream url="http://h" /></route></moxy>"#,
        );
        assert!(e.iter().any(|m| m.contains("unknown element <bogus>")), "{e:?}");

        let e = errors(
            r#"<moxy version="1"><route path="/a/*"><upstream url="${NOPE}" /></route></moxy>"#,
        );
        assert!(e.iter().any(|m| m.contains("NOPE is not set")), "{e:?}");

        let e = errors(
            r#"<moxy version="1"><route path="/a/*"><load-balance><upstream url="http://h" /></load-balance></route></moxy>"#,
        );
        assert!(e.iter().any(|m| m.contains("two or more")), "{e:?}");

        let e = errors(
            r#"<moxy version="1"><route path="/a/*"><rate-limit requests="1" per="2m" /><upstream url="http://h" /></route></moxy>"#,
        );
        assert!(e.iter().any(|m| m.contains("per must be")), "{e:?}");

        let e = errors(r#"<moxy version="2"></moxy>"#);
        assert!(e.iter().any(|m| m.contains("version")), "{e:?}");

        let e = errors("<moxy version=\"1\"");
        assert!(e[0].contains("invalid XML"));
    }

    #[test]
    fn route_matching() {
        let a = load_str(
            r#"<moxy version="1">
              <route path="/a/*"><upstream url="http://one" /></route>
              <route path="/a/b/*"><upstream url="http://two" /></route>
              <route path="/a/b/*x" auth="none"><upstream url="http://bad" /></route>
            </moxy>"#,
            &env_of(&[]),
        );
        assert!(a.is_err(), "'*' in the middle is invalid");

        let a = load_str(
            r#"<moxy version="1">
              <route path="/a/*"><upstream url="http://one" /></route>
              <route path="/a/b/*"><upstream url="http://two" /></route>
              <route path="/exact"><upstream url="http://three" /></route>
              <route path="/exact/*"><upstream url="http://four" /></route>
            </moxy>"#,
            &env_of(&[]),
        )
        .unwrap();
        let m = |p: &str| {
            a.resolved
                .match_route(p)
                .map(|(r, s)| (r.upstream.urls[0].host_str().unwrap().to_string(), s))
        };
        assert_eq!(m("/a/x/y"), Some(("one".into(), "/x/y".into())));
        assert_eq!(m("/a"), Some(("one".into(), "".into())));
        assert_eq!(m("/a/b/c"), Some(("two".into(), "/c".into())));
        assert_eq!(m("/ab"), None);
        // Tie between exact and wildcard of the same length: first declared wins.
        assert_eq!(m("/exact"), Some(("three".into(), "".into())));
        assert_eq!(m("/exact/z"), Some(("four".into(), "/z".into())));
        assert_eq!(m("/nothing"), None);
    }

    #[test]
    fn substitution() {
        let env = env_of(&[("A", "1"), ("B", "two")]);
        assert_eq!(substitute("x${A}y${B}", &env).unwrap(), "x1ytwo");
        assert!(substitute("${C}", &env).is_err());
        assert!(substitute("${A", &env).is_err());
        assert_eq!(substitute("plain $ text", &env).unwrap(), "plain $ text");
    }
}
