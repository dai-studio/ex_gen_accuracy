//! End-to-end tests of /api, the UI routes and config reloads.

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::Duration;

fn base_xml(up: &str) -> String {
    format!(
        r#"<moxy version="1">
  <server host="127.0.0.1" port="{{PORT}}" />
  <auth issuer="https://idp.example.com" client-id="moxy" client-secret="${{OIDC_SECRET}}">
    <admin email="admin@example.com" />
  </auth>
  <route path="/openai/*" description="OpenAI-compatible">
    <ai provider="openai" model="m1" />
    <token-budget tokens="100000" per="1d" key="user" />
    <upstream url="{up}/v1" />
  </route>
  <route path="/plain/*" auth="none" description="Plain">
    <headers><set name="x-api-key" value="${{OIDC_SECRET}}" /></headers>
    <upstream url="{up}" />
  </route>
</moxy>"#
    )
}

const ENV: &[(&str, &str)] = &[("OIDC_SECRET", "top-secret")];

#[tokio::test(flavor = "multi_thread")]
async fn endpoints_and_identity() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start_with_env(&base_xml(&up.url()), ENV);
    let c = client();

    let v = json(c.get(m.url("/api/endpoints")).send().await.unwrap()).await;
    let eps = v["endpoints"].as_array().unwrap();
    assert_eq!(eps.len(), 2);
    assert_eq!(eps[0], json!({"path": "/openai/*", "description": "OpenAI-compatible", "auth": "api-key", "chat": true}));
    assert!(!v.to_string().contains(&up.url()), "upstream URLs are never exposed");

    let r = c.get(m.url("/api/me")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    let me = json(c.get(m.url("/api/me")).bearer_auth(&seed.admin_key).send().await.unwrap()).await;
    assert_eq!(me["role"], "admin");
    assert_eq!(me["authMethod"], "api-key");
    let me = json(
        c.get(m.url("/api/me"))
            .header("cookie", session_cookie(&seed.user_session))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(me["role"], "user");
    assert_eq!(me["email"], "user@example.com");
    assert_eq!(me["csrfToken"], seed.user_session.1);

    // The anonymous index page is served; /chat and /admin need a session.
    let r = c.get(m.url("/")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let r = c.get(m.url("/chat")).send().await.unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(r.headers()["location"], "/auth/login?return_to=%2Fchat");
    let r = c.get(m.url("/admin")).send().await.unwrap();
    assert_eq!(r.headers()["location"], "/auth/login?return_to=%2Fadmin");
    let r = c
        .get(m.url("/chat"))
        .header("cookie", session_cookie(&seed.user_session))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .get(m.url("/admin"))
        .header("cookie", session_cookie(&seed.user_session))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .get(m.url("/admin"))
        .header("cookie", session_cookie(&seed.admin_session))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Logout deletes the session.
    let r = c
        .post(m.url("/auth/logout"))
        .header("cookie", session_cookie(&seed.user_session))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    assert!(r.headers()["set-cookie"].to_str().unwrap().contains("Max-Age=0"));
    let r = c
        .get(m.url("/api/me"))
        .header("cookie", session_cookie(&seed.user_session))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_config_api_and_csrf() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start_with_env(&base_xml(&up.url()), ENV);
    let c = client();

    let r = c.get(m.url("/api/config")).bearer_auth(&seed.user_key).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let cfg = json(c.get(m.url("/api/config")).bearer_auth(&seed.admin_key).send().await.unwrap()).await;
    assert_eq!(cfg["config"]["routes"].as_array().unwrap().len(), 2);
    // Secrets stay as references.
    assert_eq!(cfg["config"]["auth"]["clientSecret"], "${OIDC_SECRET}");
    assert!(!cfg.to_string().contains("top-secret"));
    let v1 = cfg["version"].as_str().unwrap().to_string();

    // Invalid routes are rejected with 422 and the file is untouched.
    let before = std::fs::read_to_string(&m.config_path).unwrap();
    let r = c
        .put(m.url("/api/config/routes"))
        .bearer_auth(&seed.admin_key)
        .json(&json!([{"path": "/api/*", "upstream": {"type": "upstream", "url": "http://x"}}]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let e = json(r).await;
    assert_eq!(e["error"]["type"], "validation_failed");
    assert!(e["error"]["details"][0].as_str().unwrap().contains("reserved"));
    assert_eq!(std::fs::read_to_string(&m.config_path).unwrap(), before);

    // Valid routes are written to moxy.xml and take effect.
    let mut routes = cfg["config"]["routes"].clone();
    routes.as_array_mut().unwrap().push(json!({
        "path": "/new/*", "auth": "none", "description": "added",
        "middleware": [{"type": "response", "ops": [{"op": "set", "name": "x-new", "value": "1"}]}],
        "upstream": {"type": "upstream", "url": up.url()}
    }));
    let r = c
        .put(m.url("/api/config/routes"))
        .bearer_auth(&seed.admin_key)
        .json(&json!({"routes": routes}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = json(r).await;
    assert_ne!(v["version"], v1);
    let xml = std::fs::read_to_string(&m.config_path).unwrap();
    assert!(xml.contains(r#"<route path="/new/*" auth="none" description="added">"#), "{xml}");
    assert!(xml.contains("${OIDC_SECRET}") && !xml.contains("top-secret"));
    let r = c.get(m.url("/new/hello")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-new"], "1");

    // Middleware of one route.
    let r = c
        .put(m.url("/api/config/routes/%2Fnew%2F%2A/middleware"))
        .bearer_auth(&seed.admin_key)
        .json(&json!([{"type": "rate-limit", "requests": 1, "per": "1h", "key": "ip"}]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(c.get(m.url("/new/a")).send().await.unwrap().status(), 200);
    assert_eq!(c.get(m.url("/new/a")).send().await.unwrap().status(), 429);
    let r = c
        .put(m.url("/api/config/routes/%2Fnope%2F%2A/middleware"))
        .bearer_auth(&seed.admin_key)
        .json(&json!([]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Settings.
    let r = c
        .put(m.url("/api/config/settings"))
        .bearer_auth(&seed.admin_key)
        .json(&json!({"server": {"host": "127.0.0.1", "port": "not-a-port"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let r = c
        .put(m.url("/api/config/settings"))
        .bearer_auth(&seed.admin_key)
        .json(&json!({"auth": {"issuer": "https://idp.example.com", "clientId": "moxy",
                                "clientSecret": "${OIDC_SECRET}", "sessionTtl": "2h",
                                "admins": ["admin@example.com", "other@example.com"]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(std::fs::read_to_string(&m.config_path).unwrap().contains(r#"<admin email="other@example.com" />"#));

    // Session callers need the CSRF token for state-changing calls.
    let r = c
        .put(m.url("/api/config/routes"))
        .header("cookie", session_cookie(&seed.admin_session))
        .json(&routes)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(json(r).await["error"]["type"], "csrf_failed");
    let r = c
        .put(m.url("/api/config/routes"))
        .header("cookie", session_cookie(&seed.admin_session))
        .header("x-csrf-token", &seed.admin_session.1)
        .json(&routes)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn api_keys_lifecycle() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start_with_env(&base_xml(&up.url()), ENV);
    let c = client();
    let r = c
        .post(m.url("/api/keys"))
        .bearer_auth(&seed.user_key)
        .json(&json!({"name": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    let r = c
        .post(m.url("/api/keys"))
        .header("cookie", session_cookie(&seed.admin_session))
        .header("x-csrf-token", &seed.admin_session.1)
        .json(&json!({"name": "agent"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let created = json(r).await;
    let key = created["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("moxy_") && key.len() == 5 + 32);
    assert_eq!(created["ownerSub"], "admin-sub");

    let list = json(c.get(m.url("/api/keys")).bearer_auth(&seed.admin_key).send().await.unwrap()).await;
    let keys = list["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 3);
    assert!(!list.to_string().contains(&key), "plaintext is only returned once");

    // The new key works on proxy routes, until revoked.
    let r = c
        .post(m.url("/openai/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let id = created["id"].as_str().unwrap();
    let r = c
        .delete(m.url(&format!("/api/keys/{id}")))
        .bearer_auth(&seed.admin_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = c
        .post(m.url("/openai/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let list = json(c.get(m.url("/api/keys")).bearer_auth(&seed.admin_key).send().await.unwrap()).await;
    let revoked = list["keys"].as_array().unwrap().iter().find(|k| k["id"] == id).unwrap();
    assert_eq!(revoked["revoked"], true);
    assert!(revoked["lastUsedAt"].is_string());
    let r = c.delete(m.url("/api/keys/unknown")).bearer_auth(&seed.admin_key).send().await.unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_streams_reply_through_pipeline() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start_with_env(&base_xml(&up.url()), ENV);
    let c = client();
    let body = json!({"route": "/openai/*", "messages": [{"role": "user", "content": "hi"}]});

    // Needs a signed-in user and, with a session, the CSRF token.
    assert_eq!(c.post(m.url("/api/chat")).json(&body).send().await.unwrap().status(), 401);
    let r = c
        .post(m.url("/api/chat"))
        .header("cookie", session_cookie(&seed.user_session))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    let r = c
        .post(m.url("/api/chat"))
        .header("cookie", session_cookie(&seed.user_session))
        .header("x-csrf-token", &seed.user_session.1)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let text = r.text().await.unwrap();
    let events: Vec<Value> = text
        .split("\n\n")
        .filter_map(|e| e.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    let reply: String = events
        .iter()
        .filter(|e| e["type"] == "delta")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(reply, "Hello world");
    let done = events.last().unwrap();
    assert_eq!(done["type"], "done");
    assert_eq!(done["usage"], json!({"input": 3, "output": 5}));

    // Routes without <ai> cannot be used for chat.
    let r = c
        .post(m.url("/api/chat"))
        .bearer_auth(&seed.user_key)
        .json(&json!({"route": "/plain/*", "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let r = c
        .post(m.url("/api/chat"))
        .bearer_auth(&seed.user_key)
        .json(&json!({"route": "/nope/*", "messages": [{"role": "user", "content": "x"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn hot_reload_from_disk() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start_with_env(&base_xml(&up.url()), ENV);
    let c = client();
    assert_eq!(c.get(m.url("/later/x")).send().await.unwrap().status(), 404);

    let xml = std::fs::read_to_string(&m.config_path).unwrap().replace(
        "</moxy>",
        &format!(r#"<route path="/later/*" auth="none"><upstream url="{}" /></route></moxy>"#, up.url()),
    );
    std::fs::write(&m.config_path, &xml).unwrap();
    let ok = eventually(Duration::from_secs(5), || async {
        c.get(m.url("/later/x")).send().await.unwrap().status() == 200
    })
    .await;
    assert!(ok, "new route becomes active");

    // An invalid file keeps the previous config and reports the error.
    std::fs::write(&m.config_path, xml.replace("</moxy>", "<bogus /></moxy>")).unwrap();
    let ok = eventually(Duration::from_secs(5), || async {
        let v = json(c.get(m.url("/api/config")).bearer_auth(&seed.admin_key).send().await.unwrap()).await;
        v["lastError"].is_object()
    })
    .await;
    assert!(ok, "reload error is shown");
    assert_eq!(c.get(m.url("/later/x")).send().await.unwrap().status(), 200);
    let v = json(c.get(m.url("/api/config")).bearer_auth(&seed.admin_key).send().await.unwrap()).await;
    assert!(v["lastError"]["errors"][0].as_str().unwrap().contains("bogus"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_drops_cache_of_changed_routes() {
    let up = mock_upstream("a").await;
    let xml = format!(
        r#"<moxy version="1"><server port="{{PORT}}" />
  <route path="/c/*" auth="none" description="v1"><response><cache ttl="10m" /></response><upstream url="{u}" /></route>
</moxy>"#,
        u = up.url()
    );
    let (m, _) = Moxy::start(&xml);
    let c = client();
    c.get(m.url("/c/x")).send().await.unwrap();
    let r = c.get(m.url("/c/x")).send().await.unwrap();
    assert_eq!(r.headers()["x-moxy-cache"], "hit");
    let changed = std::fs::read_to_string(&m.config_path).unwrap().replace("v1", "v2");
    std::fs::write(&m.config_path, changed).unwrap();
    let ok = eventually(Duration::from_secs(5), || async {
        let v = json(c.get(m.url("/api/endpoints")).send().await.unwrap()).await;
        v["endpoints"][0]["description"] == "v2"
    })
    .await;
    assert!(ok);
    let r = c.get(m.url("/c/x")).send().await.unwrap();
    assert!(r.headers().get("x-moxy-cache").is_none(), "cache was dropped");
}
