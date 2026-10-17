//! End-to-end tests of the proxy path (`/*`) and its middleware.

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn forwards_requests_and_applies_header_middleware() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start_with_env(
        &format!(
            r#"<moxy version="1">
  <server host="127.0.0.1" port="{{PORT}}" />
  <route path="/svc/*" description="Echo service">
    <headers>
      <set name="Authorization" value="Bearer ${{UPSTREAM_TOKEN}}" />
      <add name="x-added" value="1" />
      <remove name="x-secret" />
    </headers>
    <response>
      <set name="x-moxy" value="yes" />
      <remove name="x-drop-me" />
    </response>
    <upstream url="{}/base" />
  </route>
</moxy>"#,
            up.url()
        ),
        &[("UPSTREAM_TOKEN", "upstream-secret")],
    );
    let c = client();
    let r = c
        .post(m.url("/svc/a/b?x=1&y=2"))
        .bearer_auth(&seed.user_key)
        .header("x-secret", "hide")
        .header("cookie", format!("theme=dark; {}", session_cookie(&seed.user_session)))
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-moxy"], "yes");
    assert!(r.headers().get("x-drop-me").is_none());
    assert_eq!(r.headers()["x-upstream"], "1");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["method"], "POST");
    assert_eq!(v["path"], "/base/a/b?x=1&y=2");
    assert_eq!(v["body"], "payload");
    let h = &v["headers"];
    assert_eq!(h["host"], format!("127.0.0.1:{}", up.port));
    // The caller's Moxy key is replaced by the upstream credential from header middleware.
    assert_eq!(h["authorization"], "Bearer upstream-secret");
    assert_eq!(h["x-added"], "1");
    assert!(h.get("x-secret").is_none());
    // The Moxy session cookie is removed; other cookies are kept.
    assert_eq!(h["cookie"], "theme=dark");

    // Exact prefix without trailing slash maps to the upstream base path.
    let v: Value = json(c.get(m.url("/svc")).bearer_auth(&seed.user_key).send().await.unwrap()).await;
    assert_eq!(v["path"], "/base");
}

#[tokio::test(flavor = "multi_thread")]
async fn authentication_modes_and_errors() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start(&format!(
        r#"<moxy version="1">
  <server port="{{PORT}}" />
  <route path="/key/*"><upstream url="{u}" /></route>
  <route path="/open/*" auth="none"><upstream url="{u}" /></route>
  <route path="/sess/*" auth="session"><upstream url="{u}" /></route>
</moxy>"#,
        u = up.url()
    ));
    let c = client();
    let r = c.get(m.url("/key/x")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(json(r).await["error"]["type"], "unauthorized");
    let r = c.get(m.url("/key/x")).bearer_auth("moxy_wrong").send().await.unwrap();
    assert_eq!(r.status(), 401);
    let r = c.get(m.url("/key/x")).bearer_auth(&seed.user_key).send().await.unwrap();
    assert_eq!(r.status(), 200);
    // The Authorization header is not forwarded.
    assert!(json(r).await["headers"].get("authorization").is_none());

    assert_eq!(c.get(m.url("/open/x")).send().await.unwrap().status(), 200);

    assert_eq!(c.get(m.url("/sess/x")).send().await.unwrap().status(), 401);
    let r = c
        .get(m.url("/sess/x"))
        .header("cookie", session_cookie(&seed.user_session))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = c.get(m.url("/missing")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    assert_eq!(json(r).await["error"]["type"], "route_not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn request_middleware_rewrite_body_size_and_timeout() {
    let up = mock_upstream("a").await;
    let (m, _) = Moxy::start(&format!(
        r#"<moxy version="1">
  <server port="{{PORT}}" />
  <route path="/r/*" auth="none">
    <request>
      <rewrite-path from="/old" to="/new" />
      <max-body-size bytes="10" />
      <timeout duration="1s" />
    </request>
    <upstream url="{u}" />
  </route>
</moxy>"#,
        u = up.url()
    ));
    let c = client();
    let v = json(c.get(m.url("/r/old/x?q=1")).send().await.unwrap()).await;
    assert_eq!(v["path"], "/new/x?q=1");

    let r = c.post(m.url("/r/x")).body("this body is too long").send().await.unwrap();
    assert_eq!(r.status(), 413);
    assert_eq!(json(r).await["error"]["type"], "payload_too_large");
    let hits = up.hits();
    // Chunked body (no Content-Length) is limited while streaming.
    let stream = futures_util::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from("0123456789")),
        Ok(bytes::Bytes::from("0123456789")),
    ]);
    let r = c
        .post(m.url("/r/x"))
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    let _ = hits;

    let r = c.get(m.url("/r/slow")).send().await.unwrap();
    assert_eq!(r.status(), 504);
    assert_eq!(json(r).await["error"]["type"], "upstream_timeout");
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_errors() {
    let up = mock_upstream("a").await;
    let dead = free_port();
    let (m, _) = Moxy::start(&format!(
        r#"<moxy version="1">
  <server port="{{PORT}}" />
  <route path="/dead/*" auth="none"><upstream url="http://127.0.0.1:{dead}" /></route>
  <route path="/ok/*" auth="none"><upstream url="{u}" /></route>
</moxy>"#,
        u = up.url()
    ));
    let c = client();
    let r = c.get(m.url("/dead/x")).send().await.unwrap();
    assert_eq!(r.status(), 502);
    assert_eq!(json(r).await["error"]["type"], "upstream_unavailable");
    // Upstream HTTP errors pass through unchanged.
    let r = c.get(m.url("/ok/error")).send().await.unwrap();
    assert_eq!(r.status(), 418);
    assert_eq!(r.text().await.unwrap(), "teapot");
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_rejects_with_retry_after() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start(&format!(
        r#"<moxy version="1">
  <server port="{{PORT}}" />
  <route path="/rl/*">
    <rate-limit requests="2" per="1h" key="api-key" />
    <upstream url="{u}" />
  </route>
</moxy>"#,
        u = up.url()
    ));
    let c = client();
    for _ in 0..2 {
        let r = c.get(m.url("/rl/x")).bearer_auth(&seed.user_key).send().await.unwrap();
        assert_eq!(r.status(), 200);
    }
    let r = c.get(m.url("/rl/x")).bearer_auth(&seed.user_key).send().await.unwrap();
    assert_eq!(r.status(), 429);
    let retry: u64 = r.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!(retry >= 1 && retry <= 3600);
    assert_eq!(json(r).await["error"]["type"], "rate_limited");
    assert_eq!(up.hits(), 2, "rejected request never reaches the upstream");
    // Another key has its own counter.
    let r = c.get(m.url("/rl/x")).bearer_auth(&seed.admin_key).send().await.unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_model_rewrite_and_token_budget() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start(&format!(
        r#"<moxy version="1">
  <server port="{{PORT}}" />
  <route path="/openai/*">
    <ai provider="openai" model="forced-model" />
    <token-budget tokens="10" per="1d" key="api-key" />
    <upstream url="{u}/v1" />
  </route>
</moxy>"#,
        u = up.url()
    ));
    let c = client();
    let body = json!({"model": "client-model", "messages": [{"role": "user", "content": "hi"}]});
    // 8 tokens per call: two calls fit (0 -> 8 -> 16), the third is rejected.
    for i in 0..2 {
        let r = c
            .post(m.url("/openai/chat/completions"))
            .bearer_auth(&seed.user_key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "call {i}");
        assert_eq!(json(r).await["model"], "forced-model");
    }
    let r = c
        .post(m.url("/openai/chat/completions"))
        .bearer_auth(&seed.user_key)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    assert!(r.headers().get("retry-after").is_some());
    assert_eq!(json(r).await["error"]["type"], "token_budget_exceeded");

    // Streamed responses are passed through and counted too (another key).
    let r = c
        .post(m.url("/openai/chat/completions"))
        .bearer_auth(&seed.admin_key)
        .json(&json!({"model": "x", "stream": true, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let text = r.text().await.unwrap();
    assert!(text.contains("Hello") && text.contains("[DONE]"));
    assert!(text.contains("usage"), "include_usage was requested by moxy: {text}");
    let r = c
        .post(m.url("/openai/chat/completions"))
        .bearer_auth(&seed.admin_key)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "8 tokens used so far");
    let r = c
        .post(m.url("/openai/chat/completions"))
        .bearer_auth(&seed.admin_key)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429, "16 tokens used");
}

#[tokio::test(flavor = "multi_thread")]
async fn response_cache() {
    let up = mock_upstream("a").await;
    let (m, seed) = Moxy::start(&format!(
        r#"<moxy version="1">
  <server port="{{PORT}}" />
  <route path="/c/*">
    <response><cache ttl="1m" /></response>
    <upstream url="{u}" />
  </route>
  <route path="/ai/*">
    <ai provider="openai" />
    <response><cache ttl="1m" /></response>
    <upstream url="{u}/v1" />
  </route>
</moxy>"#,
        u = up.url()
    ));
    let c = client();
    let get = |path: &str, key: &str| c.get(m.url(path)).bearer_auth(key.to_string()).send();
    let r1 = get("/c/x", &seed.user_key).await.unwrap();
    assert!(r1.headers().get("x-moxy-cache").is_none());
    let v1 = json(r1).await;
    let r2 = get("/c/x", &seed.user_key).await.unwrap();
    assert_eq!(r2.headers()["x-moxy-cache"], "hit");
    assert_eq!(r2.headers()["content-type"], "application/json");
    assert_eq!(json(r2).await["n"], v1["n"]);
    assert_eq!(up.hits(), 1);

    // Another caller never gets this caller's cached response.
    get("/c/x", &seed.admin_key).await.unwrap();
    assert_eq!(up.hits(), 2);

    // Cache-Control: no-cache on the request bypasses the cache.
    c.get(m.url("/c/x"))
        .bearer_auth(&seed.user_key)
        .header("cache-control", "no-cache")
        .send()
        .await
        .unwrap();
    assert_eq!(up.hits(), 3);

    // no-store responses are not cached.
    get("/c/nostore", &seed.user_key).await.unwrap();
    get("/c/nostore", &seed.user_key).await.unwrap();
    assert_eq!(up.hits(), 5);

    // POST on an <ai> route is cached; JSON key order does not matter.
    let a = json!({"model": "m", "messages": [{"role": "user", "content": "q"}]});
    let b: Value = serde_json::from_str(r#"{"messages":[{"content":"q","role":"user"}],"model":"m"}"#).unwrap();
    let before = up.hits();
    c.post(m.url("/ai/chat/completions")).bearer_auth(&seed.user_key).json(&a).send().await.unwrap();
    let r = c.post(m.url("/ai/chat/completions")).bearer_auth(&seed.user_key).json(&b).send().await.unwrap();
    assert_eq!(r.headers()["x-moxy-cache"], "hit");
    assert_eq!(up.hits(), before + 1);
    // Streamed requests are never cached.
    for _ in 0..2 {
        c.post(m.url("/ai/chat/completions"))
            .bearer_auth(&seed.user_key)
            .json(&json!({"model": "m", "stream": true, "messages": []}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
    }
    assert_eq!(up.hits(), before + 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn load_balancing_round_robin_and_failover() {
    let a = mock_upstream("a").await;
    let b = mock_upstream("b").await;
    let dead = free_port();
    let (m, _) = Moxy::start(&format!(
        r#"<moxy version="1">
  <server port="{{PORT}}" />
  <route path="/rr/*" auth="none">
    <load-balance strategy="round-robin">
      <upstream url="{a}" />
      <upstream url="{b}" />
    </load-balance>
  </route>
  <route path="/fo/*" auth="none">
    <load-balance strategy="failover">
      <upstream url="http://127.0.0.1:{dead}" />
      <upstream url="{b}" />
    </load-balance>
  </route>
</moxy>"#,
        a = a.url(),
        b = b.url()
    ));
    let c = client();
    let mut seen = Vec::new();
    for _ in 0..4 {
        let v = json(c.get(m.url("/rr/x")).send().await.unwrap()).await;
        seen.push(v["upstream"].as_str().unwrap().to_string());
    }
    assert_eq!(seen, vec!["a", "b", "a", "b"]);

    for _ in 0..3 {
        let r = c.get(m.url("/fo/x")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(json(r).await["upstream"], "b");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_responses_are_not_buffered() {
    use futures_util::StreamExt;
    // An upstream that sends one SSE event, waits, then sends another.
    let app = axum::Router::new().fallback(|| async {
        let s = futures_util::stream::unfold(0, |i| async move {
            if i == 2 {
                return None;
            }
            if i == 1 {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            Some((Ok::<_, std::io::Error>(format!("data: {i}\n\n")), i + 1))
        });
        (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(s),
        )
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

    let (m, _) = Moxy::start(&format!(
        r#"<moxy version="1"><server port="{{PORT}}" />
  <route path="/s/*" auth="none"><upstream url="http://127.0.0.1:{port}" /></route></moxy>"#
    ));
    let r = client().get(m.url("/s/events")).send().await.unwrap();
    let start = std::time::Instant::now();
    let mut stream = r.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("data: 0"));
    assert!(start.elapsed() < Duration::from_millis(1000), "first event arrives before the second is sent");
    let mut rest = Vec::new();
    while let Some(c) = stream.next().await {
        rest.extend_from_slice(&c.unwrap());
    }
    assert!(String::from_utf8_lossy(&rest).contains("data: 1"));
}
