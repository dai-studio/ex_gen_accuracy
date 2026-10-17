//! Test harness: runs the real `moxy` binary against a mock upstream and a
//! mock OpenID Connect provider.

#![allow(dead_code)]

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TEST_KEY_PEM: &str = include_str!("../fixtures/oidc_test_key.pem");
pub const TEST_KEY_N: &str = include_str!("../fixtures/oidc_test_key.n");
pub const CLIENT_ID: &str = "moxy-test";
pub const CLIENT_SECRET: &str = "test-client-secret";

/// A currently unused port, never handed out twice within one test process.
pub fn free_port() -> u16 {
    static USED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut used = USED.lock().unwrap();
        if !used.contains(&port) {
            used.push(port);
            return port;
        }
    }
}

// ---------------------------------------------------------------------------
// Mock upstream + identity provider
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MockState {
    pub hits: AtomicUsize,
    pub issuer: Mutex<String>,
    /// code -> (nonce, code_challenge, email, redirect_uri)
    codes: Mutex<HashMap<String, (String, String, String, String)>>,
    pub last_chat_body: Mutex<Option<Value>>,
}

pub struct Mock {
    pub url: String,
    pub state: Arc<MockState>,
}

pub async fn start_mock() -> Mock {
    let state = Arc::new(MockState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    *state.issuer.lock().unwrap() = url.clone();
    let app = Router::new()
        .route("/echo/{*rest}", any(echo))
        .route("/count", get(count))
        .route("/slow", any(slow))
        .route("/sse", get(sse))
        .route("/status/{code}", any(status))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/jwks", get(jwks))
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Mock { url, state }
}

async fn echo(State(st): State<Arc<MockState>>, req: Request) -> Response {
    st.hits.fetch_add(1, Ordering::SeqCst);
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);
    let headers: HashMap<String, Vec<String>> = req
        .headers()
        .keys()
        .map(|k| {
            (
                k.as_str().to_string(),
                req.headers()
                    .get_all(k)
                    .iter()
                    .map(|v| v.to_str().unwrap_or("").to_string())
                    .collect(),
            )
        })
        .collect();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap();
    axum::Json(json!({
        "method": method,
        "path": path,
        "query": query,
        "headers": headers,
        "body": String::from_utf8_lossy(&body),
    }))
    .into_response()
}

async fn count(State(st): State<Arc<MockState>>) -> Response {
    let n = st.hits.fetch_add(1, Ordering::SeqCst) + 1;
    axum::Json(json!({ "count": n })).into_response()
}

async fn slow(State(st): State<Arc<MockState>>) -> Response {
    st.hits.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    "slow done".into_response()
}

async fn sse() -> Response {
    let stream = async_stream(vec![
        (Duration::ZERO, "data: first\n\n"),
        (Duration::from_millis(1500), "data: second\n\n"),
    ]);
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn async_stream(
    parts: Vec<(Duration, &'static str)>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    futures_util::stream::unfold(parts.into_iter(), |mut it| async move {
        let (delay, data) = it.next()?;
        tokio::time::sleep(delay).await;
        Some((Ok(Bytes::from_static(data.as_bytes())), it))
    })
}

async fn status(axum::extract::Path(code): axum::extract::Path<u16>) -> Response {
    (StatusCode::from_u16(code).unwrap(), "upstream says no").into_response()
}

/// A tiny OpenAI-compatible chat endpoint that reports 10 prompt + 10 completion tokens.
async fn openai_chat(
    State(st): State<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    st.hits.fetch_add(1, Ordering::SeqCst);
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    *st.last_chat_body.lock().unwrap() = Some(v.clone());
    let model = v["model"].as_str().unwrap_or("none").to_string();
    let auth = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    if v["stream"] == true {
        let usage = v["stream_options"]["include_usage"] == true;
        let mut events = vec![
            format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"content":"Hello"}}],"model":model})
            ),
            format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"content":" world"}}],"model":model})
            ),
        ];
        if usage {
            events.push(format!("data: {}\n\n", json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":10,"total_tokens":20}})));
        }
        events.push("data: [DONE]\n\n".to_string());
        let stream = futures_util::stream::iter(
            events
                .into_iter()
                .map(|e| Ok::<_, std::io::Error>(Bytes::from(e))),
        );
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    axum::Json(json!({
        "model": model,
        "upstream_auth": auth,
        "choices": [{"message": {"role": "assistant", "content": "Hello world"}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20},
    }))
    .into_response()
}

async fn discovery(State(st): State<Arc<MockState>>) -> Response {
    let issuer = st.issuer.lock().unwrap().clone();
    axum::Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
    }))
    .into_response()
}

/// Signs the user in immediately as `email` (a test-only query parameter).
async fn authorize(
    State(st): State<Arc<MockState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(
        q.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    let code = format!("code-{}", uuid::Uuid::new_v4());
    let email = q
        .get("email")
        .cloned()
        .unwrap_or_else(|| "admin@example.com".into());
    st.codes.lock().unwrap().insert(
        code.clone(),
        (
            q["nonce"].clone(),
            q["code_challenge"].clone(),
            email,
            q["redirect_uri"].clone(),
        ),
    );
    let target = format!(
        "{}?code={}&state={}",
        q["redirect_uri"],
        code,
        url::form_urlencoded::byte_serialize(q["state"].as_bytes()).collect::<String>()
    );
    Redirect::to(&target).into_response()
}

async fn token(State(st): State<Arc<MockState>>, headers: HeaderMap, body: Bytes) -> Response {
    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let expected_basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some(expected_basic.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let Some((nonce, challenge, email, redirect_uri)) =
        st.codes.lock().unwrap().remove(&form["code"])
    else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    };
    let verifier_hash = URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes()));
    if verifier_hash != challenge || form["redirect_uri"] != redirect_uri {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error": "invalid_grant", "error_description": "pkce"})),
        )
            .into_response();
    }
    let issuer = st.issuer.lock().unwrap().clone();
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": issuer,
        "aud": CLIENT_ID,
        "sub": format!("sub-{email}"),
        "email": email,
        "name": "Test Person",
        "nonce": nonce,
        "iat": now,
        "exp": now + 300,
    });
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some("test-key".into());
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(TEST_KEY_PEM.as_bytes()).unwrap();
    let id_token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
    axum::Json(json!({"access_token": "at", "token_type": "Bearer", "id_token": id_token}))
        .into_response()
}

async fn jwks() -> Response {
    axum::Json(json!({
        "keys": [{"kty": "RSA", "kid": "test-key", "use": "sig", "alg": "RS256", "n": TEST_KEY_N.trim(), "e": "AQAB"}]
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// The Moxy process
// ---------------------------------------------------------------------------

pub struct Moxy {
    pub child: Option<Child>,
    pub port: u16,
    pub base: String,
    pub dir: tempfile::TempDir,
    pub config_path: PathBuf,
}

impl Drop for Moxy {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// `routes` is inserted into a moxy.xml whose server port is chosen here.
/// `{mock}` in the text is replaced with the mock server URL.
pub fn config_text(port: u16, mock: &str, routes: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<moxy version="1">
  <server host="127.0.0.1" port="{port}"/>
  <auth issuer="{mock}" client-id="{CLIENT_ID}" client-secret="${{TEST_OIDC_SECRET}}" session-ttl="1h">
    <admin email="admin@example.com"/>
  </auth>
  {routes}
</moxy>"#
    )
    .replace("{mock}", mock)
}

pub fn prepare(routes: &str, mock: &str) -> Moxy {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config_path = dir.path().join("moxy.xml");
    std::fs::write(&config_path, config_text(port, mock, routes)).unwrap();
    Moxy {
        child: None,
        port,
        base: format!("http://127.0.0.1:{port}"),
        dir,
        config_path,
    }
}

impl Moxy {
    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    /// Create an API key directly in the store (only while the server is stopped).
    pub fn create_key(&self, name: &str) -> String {
        let kv = moxy::kv::RedbStore::open_dir(&self.data_dir()).unwrap();
        let (plain, _) = moxy::auth::create_api_key(&kv, name, "sub-admin@example.com").unwrap();
        use moxy::kv::KvStore;
        kv.close().unwrap();
        plain
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_moxy"));
        cmd.env("MOXY_CONFIG", &self.config_path)
            .env("MOXY_DATA_DIR", self.data_dir())
            .env("MOXY_LOG", "warn")
            .env("TEST_OIDC_SECRET", CLIENT_SECRET)
            .env("TEST_UPSTREAM_TOKEN", "upstream-secret")
            .stdout(Stdio::null())
            .stderr(
                std::fs::File::create(self.dir.path().join("moxy.log"))
                    .map(Stdio::from)
                    .unwrap_or(Stdio::null()),
            );
        cmd
    }

    pub async fn start(mut self) -> Self {
        for _ in 0..3 {
            if self.try_start().await {
                return self;
            }
            // the port was taken by something else in the meantime: pick another one
            let old = format!("port=\"{}\"", self.port);
            self.port = free_port();
            self.base = format!("http://127.0.0.1:{}", self.port);
            let text = std::fs::read_to_string(&self.config_path).unwrap();
            std::fs::write(
                &self.config_path,
                text.replace(&old, &format!("port=\"{}\"", self.port)),
            )
            .unwrap();
        }
        panic!("moxy could not bind a port");
    }

    async fn try_start(&mut self) -> bool {
        self.child = Some(self.command().spawn().unwrap());
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(r) = client
                .get(format!("{}/api/endpoints", self.base))
                .send()
                .await
                && r.status().is_success()
            {
                return true;
            }
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                let log =
                    std::fs::read_to_string(self.dir.path().join("moxy.log")).unwrap_or_default();
                if log.contains("cannot bind") {
                    self.child = None;
                    return false;
                }
                panic!("moxy exited early: {status}\n{log}");
            }
            assert!(Instant::now() < deadline, "moxy did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub fn write_config(&self, routes: &str, mock: &str) {
        std::fs::write(&self.config_path, config_text(self.port, mock, routes)).unwrap();
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// A signed-in browser: session cookie plus CSRF token.
pub struct Browser {
    pub cookie: String,
    pub csrf: String,
}

/// Run the full OIDC sign-in flow against the mock provider.
pub async fn sign_in(moxy: &Moxy, email: &str, return_to: &str) -> (Browser, String) {
    let c = client();
    let login = c
        .get(moxy.url(&format!("/auth/login?return_to={return_to}")))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 302, "login redirects to the provider");
    let provider_url = login.headers()["location"].to_str().unwrap().to_string();
    let authorize = c
        .get(format!("{provider_url}&email={email}"))
        .send()
        .await
        .unwrap();
    assert_eq!(authorize.status(), 303);
    let callback_url = authorize.headers()["location"]
        .to_str()
        .unwrap()
        .to_string();
    let callback = c.get(&callback_url).send().await.unwrap();
    assert_eq!(
        callback.status(),
        302,
        "callback redirects: {:?}",
        callback.text().await
    );
    let target = callback.headers()["location"].to_str().unwrap().to_string();
    let set_cookie = callback.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Lax"));
    assert!(
        !set_cookie.contains("Secure"),
        "no Secure flag on 127.0.0.1"
    );
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    let me: Value = c
        .get(moxy.url("/api/me"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let csrf = me["csrf_token"].as_str().unwrap().to_string();
    (Browser { cookie, csrf }, target)
}
