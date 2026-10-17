//! Test harness: runs the real `moxy` binary against mock upstreams.
#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use moxy::store::{KvStore, RedbStore};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

// ------------------------------------------------------------- mock upstream

#[derive(Default)]
pub struct Upstream {
    pub hits: AtomicUsize,
    pub name: String,
}

pub struct MockUpstream {
    pub port: u16,
    pub state: Arc<Upstream>,
}

impl MockUpstream {
    pub fn hits(&self) -> usize {
        self.state.hits.load(Ordering::SeqCst)
    }
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

async fn upstream_handler(
    State(st): State<Arc<Upstream>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let n = st.hits.fetch_add(1, Ordering::SeqCst) + 1;
    let path = uri.path().to_string();
    if path.ends_with("/slow") {
        tokio::time::sleep(Duration::from_secs(3)).await;
        return (StatusCode::OK, "slow").into_response();
    }
    if path.ends_with("/chat/completions") {
        let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
        let model = req["model"].clone();
        if req["stream"] == json!(true) {
            let include_usage = req["stream_options"]["include_usage"] == json!(true);
            let mut sse = String::new();
            for word in ["Hello", " world"] {
                sse.push_str(&format!(
                    "data: {}\n\n",
                    json!({"model": model, "choices": [{"delta": {"content": word}}]})
                ));
            }
            if include_usage {
                sse.push_str(&format!(
                    "data: {}\n\n",
                    json!({"choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 5}})
                ));
            }
            sse.push_str("data: [DONE]\n\n");
            return ([("content-type", "text/event-stream")], sse).into_response();
        }
        return axum::Json(json!({
            "model": model,
            "n": n,
            "upstream": st.name,
            "choices": [{"message": {"role": "assistant", "content": "hi"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
        }))
        .into_response();
    }
    if path.ends_with("/error") {
        return (StatusCode::IM_A_TEAPOT, "teapot").into_response();
    }
    let hdrs: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v.to_str().unwrap_or(""))))
        .collect();
    let mut resp = axum::Json(json!({
        "method": method.as_str(),
        "path": uri.path_and_query().map(|p| p.as_str()).unwrap_or(""),
        "headers": hdrs,
        "body": String::from_utf8_lossy(&body),
        "n": n,
        "upstream": st.name,
    }))
    .into_response();
    resp.headers_mut().insert("x-upstream", "1".parse().unwrap());
    resp.headers_mut().insert("x-drop-me", "1".parse().unwrap());
    if path.ends_with("/nostore") {
        resp.headers_mut()
            .insert("cache-control", "no-store".parse().unwrap());
    }
    resp
}

pub async fn mock_upstream(name: &str) -> MockUpstream {
    let state = Arc::new(Upstream {
        hits: AtomicUsize::new(0),
        name: name.to_string(),
    });
    let app = axum::Router::new()
        .fallback(upstream_handler)
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MockUpstream { port, state }
}

// ------------------------------------------------------------- moxy process

pub struct Moxy {
    pub port: u16,
    pub dir: tempfile::TempDir,
    pub config_path: PathBuf,
    child: Option<Child>,
}

pub struct Seed {
    pub admin_key: String,
    pub user_key: String,
    pub user_session: (String, String),
    pub admin_session: (String, String),
}

pub fn seed_user(kv: &dyn KvStore, sub: &str, email: &str) {
    let now = chrono::Utc::now();
    kv.put_raw(
        &format!("user#{sub}"),
        &json!({"sub": sub, "email": email, "name": sub, "role": "user",
                "createdAt": now, "lastLoginAt": now, "emailVerified": true}),
    )
    .unwrap();
}

pub fn seed_session(kv: &dyn KvStore, sub: &str) -> (String, String) {
    let id = moxy::util::random_b64(32);
    let csrf = moxy::util::random_b64(32);
    let now = chrono::Utc::now();
    kv.put_raw(
        &format!("session#{id}"),
        &json!({"id": id, "userSub": sub, "csrfToken": csrf, "createdAt": now,
                "expiresAt": now + chrono::Duration::hours(24)}),
    )
    .unwrap();
    (id, csrf)
}

impl Moxy {
    /// Start moxy with `xml` (`{PORT}` is replaced by the listen port) and seeded data.
    pub fn start(xml: &str) -> (Moxy, Seed) {
        Self::start_with_env(xml, &[])
    }

    pub fn start_with_env(xml: &str, env: &[(&str, &str)]) -> (Moxy, Seed) {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let seed = {
            let kv = RedbStore::open(&data.join("moxy.redb")).unwrap();
            seed_user(&kv, "admin-sub", "admin@example.com");
            seed_user(&kv, "user-sub", "user@example.com");
            let (admin_key, _) = moxy::auth::new_api_key(&kv, "admin key", "admin-sub").unwrap();
            let (user_key, _) = moxy::auth::new_api_key(&kv, "user key", "user-sub").unwrap();
            Seed {
                admin_key,
                user_key,
                user_session: seed_session(&kv, "user-sub"),
                admin_session: seed_session(&kv, "admin-sub"),
            }
        };
        let port = free_port();
        let config_path = dir.path().join("moxy.xml");
        std::fs::write(&config_path, xml.replace("{PORT}", &port.to_string())).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_moxy"));
        cmd.env("MOXY_CONFIG", &config_path)
            .env("MOXY_DATA_DIR", &data)
            .env("MOXY_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().unwrap();
        let m = Moxy {
            port,
            dir,
            config_path,
            child: Some(child),
        };
        m.wait_ready();
        (m, seed)
    }

    fn wait_ready(&self) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(15) {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("moxy did not start");
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn child(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }
}

impl Drop for Moxy {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub fn session_cookie(s: &(String, String)) -> String {
    format!("moxy_session={}", s.0)
}

pub async fn json(resp: reqwest::Response) -> Value {
    resp.json().await.unwrap()
}

/// Poll `f` until it returns true or the timeout expires.
pub async fn eventually<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
