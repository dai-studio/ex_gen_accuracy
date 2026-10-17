//! `POST /api/chat`: the browser chat, run through the chosen route's full
//! middleware pipeline and streamed back as server-sent events.
//!
//! Events sent to the browser (each as `data: <json>`):
//! `{"type":"delta","text":"..."}`, `{"type":"error","message":"..."}` and a
//! final `{"type":"done","usage":{...}|null}`.

use super::{ApiRequest, ApiResponse, Body};
use crate::auth::Caller;
use crate::config::{AiConfig, Config, Provider, Route};
use crate::error::MoxyError;
use crate::middleware::ai::{EventSplitter, UsageTracker, delta_text, stream_error};
use crate::middleware::{self, RequestInput, budget, lb};
use crate::state::AppState;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// Path of the route to use, as declared in moxy.xml (for example `/openai/*`).
    pub route: String,
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

fn invalid(msg: impl Into<String>) -> MoxyError {
    MoxyError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", msg)
}

/// The upstream path suffix and request body for a provider.
pub fn provider_request(ai: &AiConfig, model: &str, req: &ChatRequest) -> (&'static str, Value) {
    match ai.provider {
        Provider::OpenAi => {
            let mut body = json!({
                "model": model,
                "messages": req.messages,
                "stream": true,
                "stream_options": { "include_usage": true },
            });
            if let Some(m) = req.max_tokens {
                body["max_tokens"] = json!(m);
            }
            if let Some(t) = req.temperature {
                body["temperature"] = json!(t);
            }
            ("/chat/completions", body)
        }
        Provider::Anthropic => {
            let system: Vec<&str> = req
                .messages
                .iter()
                .filter(|m| m.role == "system")
                .map(|m| m.content.as_str())
                .collect();
            let messages: Vec<&ChatMessage> =
                req.messages.iter().filter(|m| m.role != "system").collect();
            let mut body = json!({
                "model": model,
                "messages": messages,
                "max_tokens": req.max_tokens.unwrap_or(1024),
                "stream": true,
            });
            if !system.is_empty() {
                body["system"] = json!(system.join("\n\n"));
            }
            if let Some(t) = req.temperature {
                body["temperature"] = json!(t);
            }
            ("/messages", body)
        }
        Provider::Ollama => {
            let mut body = json!({ "model": model, "messages": req.messages, "stream": true });
            if let Some(t) = req.temperature {
                body["options"] = json!({ "temperature": t });
            }
            ("/api/chat", body)
        }
    }
}

/// Text of a complete (non-streamed) provider response.
fn full_text(provider: Provider, v: &Value) -> Option<String> {
    match provider {
        Provider::OpenAi => v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(str::to_string),
        Provider::Anthropic => v.get("content").and_then(Value::as_array).map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<String>()
        }),
        Provider::Ollama => v
            .pointer("/message/content")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn sse(v: &Value) -> Bytes {
    Bytes::from(format!("data: {v}\n\n"))
}

pub async fn chat(
    state: &Arc<AppState>,
    config: &Arc<Config>,
    caller: &Caller,
    req: &ApiRequest,
) -> Result<ApiResponse, MoxyError> {
    let body: ChatRequest = serde_json::from_slice(&req.body)
        .map_err(|e| invalid(format!("invalid chat request: {e}")))?;
    if body.messages.is_empty() {
        return Err(invalid("at least one message is required"));
    }
    let route = config
        .route_by_path(&body.route)
        .ok_or_else(MoxyError::route_not_found)?;
    let ai = route
        .ai()
        .cloned()
        .ok_or_else(|| invalid("this route has no <ai> element and cannot be used for chat"))?;
    let model = ai
        .model
        .clone()
        .or_else(|| body.model.clone().filter(|m| !m.trim().is_empty()))
        .ok_or_else(|| invalid("this route does not fix a model; choose one"))?;

    let (suffix, provider_body) = provider_request(&ai, &model, &body);
    let payload =
        serde_json::to_vec(&provider_body).map_err(|e| MoxyError::internal(e.to_string()))?;

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept", HeaderValue::from_static("text/event-stream"));
    if ai.provider == Provider::Anthropic {
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }

    // Full request phase: rate limits and token budgets apply to the signed-in user.
    let plan = middleware::run_request_phase(
        RequestInput {
            route: &route,
            caller,
            client_ip: req.client_ip,
            headers: &headers,
            suffix,
            content_length: Some(payload.len() as u64),
        },
        &state.limiter,
        state.kv.as_ref(),
    )?;

    let response = send_with_failover(state, &route, &plan, &payload).await?;
    let status = response.status();
    if !status.is_success() {
        // Upstream HTTP errors pass through unchanged.
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = response.bytes().await.unwrap_or_default();
        return Ok(ApiResponse::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            Body::Full(bytes),
        )
        .header("content-type", &ct));
    }

    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let st = state.clone();
    let route_path = route.path().to_string();
    let counters = plan.budget_counters.clone();
    let timeout = plan.timeout;
    tokio::spawn(async move {
        let usage = pump(response, ai.provider, timeout, &tx).await;
        let total = usage.map(|u| u.total_tokens).unwrap_or(0);
        if !counters.is_empty() && total > 0 {
            let kv = st.kv.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = budget::record(kv.as_ref(), &route_path, &counters, total) {
                    log::warn!("failed to record token usage: {e:#}");
                }
            })
            .await;
        }
        let _ = tx
            .send(sse(
                &json!({ "type": "done", "model": model, "usage": usage }),
            ))
            .await;
    });

    Ok(ApiResponse::new(StatusCode::OK, Body::Stream(rx))
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no"))
}

/// Forward the provider stream to the browser as normalised events and return the usage.
async fn pump(
    response: reqwest::Response,
    provider: Provider,
    timeout: Duration,
    tx: &mpsc::Sender<Bytes>,
) -> Option<crate::middleware::ai::Usage> {
    let mut tracker = UsageTracker::new(provider);
    let mut splitter = EventSplitter::default();
    let mut raw = Vec::new();
    let mut events = 0usize;
    let mut stream = response.bytes_stream();
    let mut out: Vec<Bytes> = Vec::new();
    loop {
        let next = match tokio::time::timeout(timeout, stream.next()).await {
            Ok(n) => n,
            Err(_) => {
                let _ = tx
                    .send(sse(
                        &json!({"type": "error", "message": "the upstream stopped responding"}),
                    ))
                    .await;
                break;
            }
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(sse(&json!({"type": "error", "message": format!("upstream stream failed: {e}")}))).await;
                break;
            }
        };
        if raw.len() < 4 * 1024 * 1024 {
            raw.extend_from_slice(&chunk);
        }
        splitter.push(&chunk, |v| {
            events += 1;
            tracker.observe(&v);
            if let Some(msg) = stream_error(&v) {
                out.push(sse(&json!({"type": "error", "message": msg})));
            } else if let Some(text) = delta_text(provider, &v) {
                out.push(sse(&json!({"type": "delta", "text": text})));
            }
        });
        for b in out.drain(..) {
            if tx.send(b).await.is_err() {
                // the browser went away; keep the usage seen so far
                return tracker.usage();
            }
        }
    }
    splitter.finish(|v| {
        events += 1;
        tracker.observe(&v);
        if let Some(text) = delta_text(provider, &v) {
            out.push(sse(&json!({"type": "delta", "text": text})));
        }
    });
    // A provider that ignored `stream: true` sent one JSON document.
    if events <= 1
        && out.is_empty()
        && let Ok(v) = serde_json::from_slice::<Value>(&raw)
    {
        tracker.observe(&v);
        if let Some(text) = full_text(provider, &v) {
            out.push(sse(&json!({"type": "delta", "text": text})));
        }
    }
    for b in out {
        let _ = tx.send(b).await;
    }
    tracker.usage()
}

/// Send to the selected upstream, failing over to others on connection errors.
async fn send_with_failover(
    state: &Arc<AppState>,
    route: &Arc<Route>,
    plan: &middleware::RequestPlan,
    payload: &[u8],
) -> Result<reqwest::Response, MoxyError> {
    let mut tried = Vec::new();
    let mut last_err = MoxyError::upstream_unavailable("no upstream available");
    while let Some(idx) = state.balancer.select(route, &tried) {
        tried.push(idx);
        let base = &route.destination.upstreams()[idx];
        let url = lb::upstream_url(base, &plan.suffix, None);
        let mut headers = plan.headers.clone();
        headers.remove(http::header::HOST);
        headers.remove(http::header::CONTENT_LENGTH);
        let request = state
            .http
            .post(&url)
            .headers(headers)
            .body(payload.to_vec())
            .send();
        match tokio::time::timeout(plan.timeout, request).await {
            Err(_) => {
                state.balancer.mark_failed(base);
                return Err(MoxyError::upstream_timeout());
            }
            Ok(Err(e)) if e.is_timeout() => {
                state.balancer.mark_failed(base);
                return Err(MoxyError::upstream_timeout());
            }
            Ok(Err(e)) => {
                log::warn!("upstream {url} unavailable: {e}");
                state.balancer.mark_failed(base);
                last_err = MoxyError::upstream_unavailable(format!(
                    "could not connect to the upstream: {e}"
                ));
            }
            Ok(Ok(resp)) => {
                if resp.status().is_server_error() {
                    state.balancer.mark_failed(base);
                }
                return Ok(resp);
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ChatRequest {
        ChatRequest {
            route: "/x/*".into(),
            model: None,
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: "be brief".into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: "hi".into(),
                },
            ],
            max_tokens: None,
            temperature: Some(0.5),
        }
    }

    #[test]
    fn builds_provider_requests() {
        let ai = AiConfig {
            provider: Provider::OpenAi,
            model: None,
        };
        let (suffix, body) = provider_request(&ai, "gpt", &req());
        assert_eq!(suffix, "/chat/completions");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);

        let ai = AiConfig {
            provider: Provider::Anthropic,
            model: None,
        };
        let (suffix, body) = provider_request(&ai, "claude", &req());
        assert_eq!(suffix, "/messages");
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["max_tokens"], 1024);

        let ai = AiConfig {
            provider: Provider::Ollama,
            model: None,
        };
        let (suffix, body) = provider_request(&ai, "llama3", &req());
        assert_eq!(suffix, "/api/chat");
        assert_eq!(body["options"]["temperature"], 0.5);
    }

    #[test]
    fn extracts_full_text() {
        assert_eq!(
            full_text(
                Provider::OpenAi,
                &json!({"choices":[{"message":{"content":"hey"}}]})
            )
            .as_deref(),
            Some("hey")
        );
        assert_eq!(
            full_text(
                Provider::Anthropic,
                &json!({"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]})
            )
            .as_deref(),
            Some("ab")
        );
    }
}
