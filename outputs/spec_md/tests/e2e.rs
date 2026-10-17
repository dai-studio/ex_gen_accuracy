//! End-to-end tests: the real binary, real sockets, mock upstreams.

mod common;

use common::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

async fn json_of(r: reqwest::Response) -> Value {
    r.json().await.unwrap()
}

fn error_type(v: &Value) -> &str {
    v["error"]["type"].as_str().unwrap_or("")
}

#[tokio::test]
async fn discovery_page_endpoints_and_route_not_found() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/echo/*" auth="none" description="Echo service"><upstream url="{mock}/echo"/></route>
           <route path="/llm/*" description="Models"><ai provider="openai"/><upstream url="{mock}/v1"/></route>"#,
        &mock.url,
    )
    .start()
    .await;
    let c = client();

    let root = c.get(moxy.url("/")).send().await.unwrap();
    assert_eq!(root.status(), 200);
    assert!(
        root.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );

    let eps = json_of(c.get(moxy.url("/api/endpoints")).send().await.unwrap()).await;
    let list = eps["endpoints"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["path"], "/echo/*");
    assert_eq!(list[0]["description"], "Echo service");
    assert_eq!(list[0]["auth"], "none");
    assert_eq!(list[1]["auth"], "api-key");
    assert_eq!(list[1]["chat"], true);
    // never reveals upstreams
    assert!(!eps.to_string().contains(&mock.url));

    let missing = c.get(moxy.url("/nowhere")).send().await.unwrap();
    assert_eq!(missing.status(), 404);
    assert_eq!(error_type(&json_of(missing).await), "route_not_found");
}

#[tokio::test]
async fn proxy_forwards_and_strips_credentials() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/svc/*" description="secured">
             <headers>
               <set name="Authorization" value="Bearer ${TEST_UPSTREAM_TOKEN}"/>
               <add name="X-Added" value="1"/>
               <remove name="X-Secret"/>
             </headers>
             <request><rewrite-path from="/old/" to="/new/"/></request>
             <response><set name="X-Gateway" value="moxy"/><remove name="X-Upstream-Only"/></response>
             <upstream url="{mock}/echo/base"/>
           </route>"#,
        &mock.url,
    );
    let key = moxy.create_key("dev");
    let moxy = moxy.start().await;
    let c = client();

    // no key, wrong key
    let r = c.get(moxy.url("/svc/x")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["www-authenticate"], "Bearer");
    assert_eq!(error_type(&json_of(r).await), "unauthorized");
    let r = c
        .get(moxy.url("/svc/x"))
        .bearer_auth("moxy_not-a-real-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    let r = c
        .post(moxy.url("/svc/old/thing?a=1&b=two"))
        .bearer_auth(&key)
        .header("cookie", "moxy_session=abc; theme=dark")
        .header("x-secret", "hide me")
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-gateway"], "moxy");
    let v = json_of(r).await;
    assert_eq!(v["method"], "POST");
    assert_eq!(v["path"], "/echo/base/new/thing");
    assert_eq!(v["query"], "a=1&b=two");
    assert_eq!(v["body"], "payload");
    assert_eq!(v["headers"]["authorization"][0], "Bearer upstream-secret");
    assert_eq!(v["headers"]["cookie"][0], "theme=dark");
    assert_eq!(v["headers"]["x-added"][0], "1");
    assert!(v["headers"].get("x-secret").is_none());
    assert_eq!(
        v["headers"]["host"][0],
        mock.url.trim_start_matches("http://")
    );
    assert!(
        !v.to_string().contains(&key),
        "the Moxy key never reaches the upstream"
    );

    // every method is proxied
    let r = c
        .delete(moxy.url("/svc/item/7"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(json_of(r).await["method"], "DELETE");
}

#[tokio::test]
async fn rate_limit_rejects_with_retry_after() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/rl/*" auth="none"><rate-limit requests="2" per="1m" key="ip"/><upstream url="{mock}/echo"/></route>"#,
        &mock.url,
    )
    .start()
    .await;
    let c = client();
    for _ in 0..2 {
        assert_eq!(c.get(moxy.url("/rl/a")).send().await.unwrap().status(), 200);
    }
    let r = c.get(moxy.url("/rl/a")).send().await.unwrap();
    assert_eq!(r.status(), 429);
    let retry: u64 = r.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry));
    assert_eq!(error_type(&json_of(r).await), "rate_limited");
    assert_eq!(
        mock.state.hits.load(Ordering::SeqCst),
        2,
        "rejected request never reached the upstream"
    );
}

#[tokio::test]
async fn body_size_timeout_and_upstream_failures() {
    let mock = start_mock().await;
    let dead = format!("http://127.0.0.1:{}", free_port());
    let routes = format!(
        r#"<route path="/small/*" auth="none"><request><max-body-size bytes="10"/></request><upstream url="{{mock}}/echo"/></route>
           <route path="/slow" auth="none"><request><timeout duration="1s"/></request><upstream url="{{mock}}/slow"/></route>
           <route path="/dead/*" auth="none"><upstream url="{dead}"/></route>
           <route path="/err/*" auth="none"><upstream url="{{mock}}/status"/></route>"#
    );
    let moxy = prepare(&routes, &mock.url).start().await;
    let c = client();

    let r = c
        .post(moxy.url("/small/x"))
        .body("0123456789ABC")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    assert_eq!(error_type(&json_of(r).await), "payload_too_large");
    let r = c
        .post(moxy.url("/small/x"))
        .body("short")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let started = Instant::now();
    let r = c.get(moxy.url("/slow")).send().await.unwrap();
    assert_eq!(r.status(), 504);
    assert!(started.elapsed() < Duration::from_millis(2900));
    assert_eq!(error_type(&json_of(r).await), "upstream_timeout");

    let r = c.get(moxy.url("/dead/x")).send().await.unwrap();
    assert_eq!(r.status(), 502);
    assert_eq!(error_type(&json_of(r).await), "upstream_unavailable");

    // upstream HTTP errors pass through unchanged
    let r = c.get(moxy.url("/err/503")).send().await.unwrap();
    assert_eq!(r.status(), 503);
    assert_eq!(r.text().await.unwrap(), "upstream says no");
}

#[tokio::test]
async fn load_balance_fails_over_and_round_robins() {
    let mock = start_mock().await;
    let dead = format!("http://127.0.0.1:{}", free_port());
    let routes = format!(
        r#"<route path="/lb/*" auth="none">
             <load-balance strategy="failover"><upstream url="{dead}"/><upstream url="{{mock}}/echo/second"/></load-balance>
           </route>
           <route path="/rr/*" auth="none">
             <load-balance strategy="round-robin"><upstream url="{{mock}}/echo/one"/><upstream url="{{mock}}/echo/two"/></load-balance>
           </route>"#
    );
    let moxy = prepare(&routes, &mock.url).start().await;
    let c = client();
    for _ in 0..3 {
        let r = c.get(moxy.url("/lb/x")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(json_of(r).await["path"], "/echo/second/x");
    }
    let mut seen = Vec::new();
    for _ in 0..4 {
        let v = json_of(c.get(moxy.url("/rr/x")).send().await.unwrap()).await;
        seen.push(v["path"].as_str().unwrap().to_string());
    }
    assert_eq!(
        seen,
        vec!["/echo/one/x", "/echo/two/x", "/echo/one/x", "/echo/two/x"]
    );
}

#[tokio::test]
async fn streams_are_not_buffered() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/sse" auth="none"><upstream url="{mock}/sse"/></route>"#,
        &mock.url,
    )
    .start()
    .await;
    let started = Instant::now();
    let r = client().get(moxy.url("/sse")).send().await.unwrap();
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    let mut stream = r.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("first"));
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "first event arrived after {:?}; the stream was buffered",
        started.elapsed()
    );
    let mut rest = Vec::new();
    while let Some(chunk) = stream.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    assert!(String::from_utf8_lossy(&rest).contains("second"));
}

#[tokio::test]
async fn cache_is_per_caller_and_respects_no_cache() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/c/*"><response><cache ttl="5m"/></response><upstream url="{mock}"/></route>"#,
        &mock.url,
    );
    let key_a = moxy.create_key("a");
    let key_b = moxy.create_key("b");
    let moxy = moxy.start().await;
    let c = client();

    let get = |key: &str, no_cache: bool| {
        let mut req = c.get(moxy.url("/c/count")).bearer_auth(key);
        if no_cache {
            req = req.header("cache-control", "no-cache");
        }
        req.send()
    };
    let r1 = get(&key_a, false).await.unwrap();
    assert_eq!(r1.headers()["x-moxy-cache"], "MISS");
    assert_eq!(json_of(r1).await["count"], 1);
    let r2 = get(&key_a, false).await.unwrap();
    assert_eq!(r2.headers()["x-moxy-cache"], "HIT");
    assert_eq!(json_of(r2).await["count"], 1);
    // another caller never sees the first caller's entry
    let r3 = get(&key_b, false).await.unwrap();
    assert_eq!(r3.headers()["x-moxy-cache"], "MISS");
    assert_eq!(json_of(r3).await["count"], 2);
    // no-cache bypasses the cache
    let r4 = get(&key_a, true).await.unwrap();
    assert_eq!(json_of(r4).await["count"], 3);
    assert_eq!(mock.state.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn ai_model_override_and_token_budget() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/openai/*" description="OpenAI">
             <ai provider="openai" model="forced-model"/>
             <token-budget tokens="30" per="1h" key="api-key"/>
             <upstream url="{mock}/v1"/>
           </route>"#,
        &mock.url,
    );
    let key = moxy.create_key("agent");
    let moxy = moxy.start().await;
    let c = client();
    let call = |stream: bool| {
        c.post(moxy.url("/openai/chat/completions"))
            .bearer_auth(&key)
            .json(&json!({"model": "requested", "stream": stream, "messages": [{"role": "user", "content": "hi"}]}))
            .send()
    };

    let r = call(false).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(json_of(r).await["model"], "forced-model");

    // streamed: usage is requested from the upstream so it can be counted
    let r = call(true).await.unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(text.contains("Hello"));
    let sent = mock.state.last_chat_body.lock().unwrap().clone().unwrap();
    assert_eq!(sent["stream_options"]["include_usage"], true);
    assert_eq!(sent["model"], "forced-model");

    // 20 + 20 tokens used against a budget of 30
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = call(false).await.unwrap();
    assert_eq!(r.status(), 429);
    assert!(r.headers().contains_key("retry-after"));
    assert_eq!(error_type(&json_of(r).await), "token_budget_exceeded");
}

#[tokio::test]
async fn config_reloads_on_change_and_keeps_last_good() {
    let mock = start_mock().await;
    let first = r#"<route path="/a/*" auth="none"><upstream url="{mock}/echo/a"/></route>"#;
    let moxy = prepare(first, &mock.url).start().await;
    let c = client();
    assert_eq!(c.get(moxy.url("/b/x")).send().await.unwrap().status(), 404);

    moxy.write_config(
        r#"<route path="/a/*" auth="none"><upstream url="{mock}/echo/a"/></route>
           <route path="/b/*" auth="none"><upstream url="{mock}/echo/b"/></route>"#,
        &mock.url,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if c.get(moxy.url("/b/x")).send().await.unwrap().status() == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "configuration was not reloaded");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // an invalid file leaves the previous configuration active
    std::fs::write(&moxy.config_path, "<moxy version=\"1\"><nonsense/></moxy>").unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(c.get(moxy.url("/b/x")).send().await.unwrap().status(), 200);
}

#[tokio::test]
async fn sign_in_admin_api_keys_and_chat() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/openai/*" description="OpenAI models">
             <ai provider="openai"/>
             <upstream url="{mock}/v1"/>
           </route>
           <route path="/private/*" auth="session"><upstream url="{mock}/echo"/></route>"#,
        &mock.url,
    )
    .start()
    .await;
    let c = client();

    // anonymous visitors are sent to sign in
    let r = c.get(moxy.url("/admin")).send().await.unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(r.headers()["location"], "/auth/login?return_to=%2Fadmin");
    let r = c.get(moxy.url("/chat")).send().await.unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(
        c.get(moxy.url("/api/me")).send().await.unwrap().status(),
        401
    );
    assert_eq!(
        c.get(moxy.url("/api/config"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    // a bad state is rejected
    let r = c
        .get(moxy.url("/auth/callback?state=bogus&code=x"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(error_type(&json_of(r).await), "invalid_state");

    // administrator sign-in
    let (admin, target) = sign_in(&moxy, "admin@example.com", "/admin").await;
    assert_eq!(target, "/admin");
    let as_admin = |req: reqwest::RequestBuilder| req.header("cookie", &admin.cookie);
    assert_eq!(
        as_admin(c.get(moxy.url("/admin")))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        as_admin(c.get(moxy.url("/chat")))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let me = json_of(as_admin(c.get(moxy.url("/api/me"))).send().await.unwrap()).await;
    assert_eq!(me["role"], "admin");
    assert_eq!(me["email"], "admin@example.com");

    // session routes accept the cookie
    let r = as_admin(c.get(moxy.url("/private/x")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(json_of(r).await["headers"].get("cookie").is_none());

    // config resource, never containing resolved secrets
    let cfg = json_of(
        as_admin(c.get(moxy.url("/api/config")))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(cfg["status"]["generation"], 1);
    assert_eq!(
        cfg["config"]["auth"]["client_secret"],
        "${TEST_OIDC_SECRET}"
    );
    assert!(!cfg.to_string().contains(CLIENT_SECRET));

    // state-changing requests need the CSRF token
    let r = as_admin(c.post(moxy.url("/api/keys")))
        .json(&json!({"name": "agent"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert_eq!(error_type(&json_of(r).await), "csrf_token_invalid");
    let r = as_admin(c.post(moxy.url("/api/keys")))
        .header("x-csrf-token", &admin.csrf)
        .json(&json!({"name": "agent"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let created = json_of(r).await;
    let key = created["key"].as_str().unwrap().to_string();
    let key_id = created["id"].as_str().unwrap().to_string();
    assert!(key.starts_with("moxy_") && key.len() == 37);
    assert_eq!(created["owner"], "sub-admin@example.com");

    // the key works on the proxy and is listed without its plaintext
    let r = c
        .post(moxy.url("/openai/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({"model": "m", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let listed = json_of(as_admin(c.get(moxy.url("/api/keys"))).send().await.unwrap()).await;
    assert_eq!(listed["keys"][0]["id"], key_id.as_str());
    assert!(listed["keys"][0]["last_used_at"].is_string());
    assert!(!listed.to_string().contains(&key));

    // API keys act with the user role on the JSON API
    let r = c
        .get(moxy.url("/api/config"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let me = json_of(
        c.get(moxy.url("/api/me"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(me["auth"], "api-key");

    // revoke
    let r = as_admin(c.delete(moxy.url(&format!("/api/keys/{key_id}"))))
        .header("x-csrf-token", &admin.csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = c
        .post(moxy.url("/openai/chat/completions"))
        .bearer_auth(&key)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // chat through the route's pipeline, streamed as server-sent events
    let r = as_admin(c.post(moxy.url("/api/chat")))
        .header("x-csrf-token", &admin.csrf)
        .json(&json!({"route": "/openai/*", "model": "chat-model", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    let text = r.text().await.unwrap();
    assert!(text.contains(r#""type":"delta""#), "{text}");
    assert!(text.contains("Hello") && text.contains(" world"), "{text}");
    assert!(text.contains(r#""type":"done""#), "{text}");
    assert!(text.contains(r#""total_tokens":20"#), "{text}");

    // config writes: invalid input is rejected with 422, valid input rewrites moxy.xml
    let mut routes = cfg["config"]["routes"].clone();
    routes
        .as_array_mut()
        .unwrap()
        .push(json!({"path": "/api/*", "destination": {"type": "upstream", "url": "http://x"}}));
    let r = as_admin(c.put(moxy.url("/api/config/routes")))
        .header("x-csrf-token", &admin.csrf)
        .json(&routes)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let v = json_of(r).await;
    assert_eq!(error_type(&v), "validation_failed");
    assert!(!v["error"]["details"].as_array().unwrap().is_empty());

    let mut routes = cfg["config"]["routes"].clone();
    routes.as_array_mut().unwrap().push(json!({
        "path": "/added/*", "auth": "none", "description": "added from the API",
        "middleware": [{"type": "headers", "ops": [{"op": "set", "name": "X-From", "value": "api"}]}],
        "destination": {"type": "upstream", "url": format!("{}/echo", mock.url)}
    }));
    let r = as_admin(c.put(moxy.url("/api/config/routes")))
        .header("x-csrf-token", &admin.csrf)
        .json(&routes)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let on_disk = std::fs::read_to_string(&moxy.config_path).unwrap();
    assert!(
        on_disk.contains(r#"<route path="/added/*" auth="none" description="added from the API">"#)
    );
    assert!(on_disk.contains("${TEST_OIDC_SECRET}"));
    let v = json_of(c.get(moxy.url("/added/x")).send().await.unwrap()).await;
    assert_eq!(v["headers"]["x-from"][0], "api");

    // replace one route's middleware
    let r = as_admin(c.put(moxy.url("/api/config/middleware?path=/added/*")))
        .header("x-csrf-token", &admin.csrf)
        .json(&json!([{"type": "headers", "ops": [{"op": "set", "name": "X-From", "value": "middleware-api"}]}]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v = json_of(c.get(moxy.url("/added/x")).send().await.unwrap()).await;
    assert_eq!(v["headers"]["x-from"][0], "middleware-api");

    // system settings
    let mut system = json!({"server": cfg["config"]["server"], "auth": cfg["config"]["auth"]});
    system["auth"]["admins"] = json!(["admin@example.com", "second@example.com"]);
    let r = as_admin(c.put(moxy.url("/api/config/system")))
        .header("x-csrf-token", &admin.csrf)
        .json(&system)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        std::fs::read_to_string(&moxy.config_path)
            .unwrap()
            .contains("second@example.com")
    );

    // an ordinary user: chat yes, admin no
    let (user, target) = sign_in(&moxy, "dev@example.com", "/chat").await;
    assert_eq!(target, "/chat");
    let r = c
        .get(moxy.url("/admin"))
        .header("cookie", &user.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = c
        .get(moxy.url("/api/keys"))
        .header("cookie", &user.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let me = json_of(
        c.get(moxy.url("/api/me"))
            .header("cookie", &user.cookie)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(me["role"], "user");

    // logout
    let r = c
        .post(moxy.url("/auth/logout"))
        .header("cookie", &user.cookie)
        .header("x-csrf-token", &user.csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(r.headers()["location"], "/");
    assert!(
        r.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let r = c
        .get(moxy.url("/api/me"))
        .header("cookie", &user.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn admin_role_follows_configuration_on_next_request() {
    let mock = start_mock().await;
    let moxy = prepare("", &mock.url).start().await;
    let (user, _) = sign_in(&moxy, "later@example.com", "/").await;
    let c = client();
    assert_eq!(
        c.get(moxy.url("/admin"))
            .header("cookie", &user.cookie)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );

    let text = std::fs::read_to_string(&moxy.config_path).unwrap().replace(
        r#"<admin email="admin@example.com"/>"#,
        r#"<admin email="admin@example.com"/><admin email="later@example.com"/>"#,
    );
    std::fs::write(&moxy.config_path, text).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = c
            .get(moxy.url("/admin"))
            .header("cookie", &user.cookie)
            .send()
            .await
            .unwrap()
            .status();
        if status == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "role change not picked up");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn refuses_to_start_with_invalid_config() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/chat/*"><upstream url="http://x"/></route>"#,
        &mock.url,
    );
    let status = moxy.command().status().unwrap();
    assert!(!status.success());

    let moxy = prepare("", &mock.url);
    std::fs::remove_file(&moxy.config_path).unwrap();
    assert!(!moxy.command().status().unwrap().success());

    // unresolved environment references are invalid too
    let moxy = prepare(
        r#"<route path="/x/*"><headers><set name="A" value="${DEFINITELY_NOT_SET_42}"/></headers><upstream url="http://x"/></route>"#,
        &mock.url,
    );
    assert!(!moxy.command().status().unwrap().success());
}

#[tokio::test]
async fn graceful_shutdown_lets_in_flight_requests_finish() {
    let mock = start_mock().await;
    let mut moxy = prepare(
        r#"<route path="/slow" auth="none"><upstream url="{mock}/slow"/></route>"#,
        &mock.url,
    )
    .start()
    .await;
    let url = moxy.url("/slow");
    let pending = tokio::spawn(async move { client().get(url).send().await.map(|r| r.status()) });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let pid = moxy.child.as_ref().unwrap().id();
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );

    // new connections are refused while the old request completes
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        client()
            .get(moxy.url("/api/endpoints"))
            .send()
            .await
            .is_err()
    );

    let status = pending.await.unwrap().unwrap();
    assert_eq!(status, 200);
    let mut child = moxy.child.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(code) = child.try_wait().unwrap() {
            assert!(code.success());
            break;
        }
        assert!(Instant::now() < deadline, "moxy did not exit");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // the store was closed cleanly and can be reopened
    moxy::kv::RedbStore::open_dir(&moxy.data_dir()).unwrap();
}

fn chunked(body: Vec<u8>) -> reqwest::Body {
    let parts: Vec<Result<Vec<u8>, std::io::Error>> =
        body.chunks(8192).map(|c| Ok(c.to_vec())).collect();
    reqwest::Body::wrap_stream(futures_util::stream::iter(parts))
}

#[tokio::test]
async fn large_and_chunked_bodies_are_rewritten_and_limited() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/openai/*" auth="none"><ai provider="openai" model="forced-model"/><upstream url="{mock}/v1"/></route>
           <route path="/limited/*" auth="none"><request><max-body-size bytes="1000"/></request><upstream url="{mock}/echo"/></route>"#,
        &mock.url,
    )
    .start()
    .await;
    let c = client();
    let big = "x".repeat(100 * 1024);
    let body = serde_json::to_vec(
        &json!({"model": "requested", "messages": [{"role": "user", "content": big}]}),
    )
    .unwrap();

    // larger than the pre-buffer limit, with a Content-Length
    let r = c
        .post(moxy.url("/openai/chat/completions"))
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(json_of(r).await["model"], "forced-model");
    let sent = mock.state.last_chat_body.lock().unwrap().take().unwrap();
    assert_eq!(
        sent["messages"][0]["content"].as_str().unwrap().len(),
        big.len()
    );

    // chunked transfer encoding
    let r = c
        .post(moxy.url("/openai/chat/completions"))
        .header("content-type", "application/json")
        .body(chunked(body))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(json_of(r).await["model"], "forced-model");

    // a chunked body over max-body-size is rejected while streaming
    let r = c
        .post(moxy.url("/limited/x"))
        .body(chunked(vec![b'a'; 50_000]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let r = c
        .post(moxy.url("/limited/x"))
        .body(chunked(vec![b'a'; 500]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(json_of(r).await["body"].as_str().unwrap().len(), 500);
}

#[tokio::test]
async fn ai_post_responses_are_cached_but_streams_are_not() {
    let mock = start_mock().await;
    let moxy = prepare(
        r#"<route path="/openai/*" auth="none"><ai provider="openai"/><response><cache ttl="1m"/></response><upstream url="{mock}/v1"/></route>"#,
        &mock.url,
    )
    .start()
    .await;
    let c = client();
    let send = |body: Value| {
        c.post(moxy.url("/openai/chat/completions"))
            .json(&body)
            .send()
    };

    let r = send(json!({"model": "m", "messages": [{"role": "user", "content": "same"}]}))
        .await
        .unwrap();
    assert_eq!(r.headers()["x-moxy-cache"], "MISS");
    let first = r.text().await.unwrap();
    // equivalent JSON with different key order hits the cache
    let r = send(json!({"messages": [{"content": "same", "role": "user"}], "model": "m"}))
        .await
        .unwrap();
    assert_eq!(r.headers()["x-moxy-cache"], "HIT");
    assert_eq!(r.text().await.unwrap(), first);
    assert_eq!(mock.state.hits.load(Ordering::SeqCst), 1);

    // streamed responses are never stored
    for _ in 0..2 {
        let r = send(json!({"model": "m", "stream": true, "messages": []}))
            .await
            .unwrap();
        assert_ne!(
            r.headers().get("x-moxy-cache").map(|v| v.to_str().unwrap()),
            Some("HIT")
        );
        r.text().await.unwrap();
    }
    assert_eq!(mock.state.hits.load(Ordering::SeqCst), 3);
}
