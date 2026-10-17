//! `POST /api/chat`: send a chat through a route's full middleware pipeline and stream
//! the reply back as server-sent events.
//!
//! Events (each `data: <json>\n\n`):
//! - `{"type":"delta","text":"..."}` for each piece of the reply,
//! - `{"type":"error","status":<u16>,"message":"..."}` if the upstream fails,
//! - `{"type":"done","usage":{"input":N,"output":M}}` at the end.

use crate::ai::{self, StreamEvent, UsageTracker};
use crate::auth::Identity;
use crate::config::HeaderOpR;
use crate::error::MoxyError;
use crate::http::{Req, Resp};
use crate::pipeline::{self, Caller, RequestInfo};
use crate::state::AppState;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatRequest {
    route: String,
    messages: Vec<Value>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
}

pub fn apply_header_ops(h: &mut HeaderMap, ops: &[HeaderOpR]) {
    for op in ops {
        match op {
            HeaderOpR::Set(n, v) | HeaderOpR::Add(n, v) => {
                let (Ok(name), Ok(value)) =
                    (HeaderName::from_bytes(n.as_bytes()), HeaderValue::from_str(v))
                else {
                    continue;
                };
                if matches!(op, HeaderOpR::Set(..)) {
                    h.insert(name, value);
                } else {
                    h.append(name, value);
                }
            }
            HeaderOpR::Remove(n) => {
                if let Ok(name) = HeaderName::from_bytes(n.as_bytes()) {
                    h.remove(name);
                }
            }
        }
    }
}

fn sse(v: &Value) -> Bytes {
    Bytes::from(format!("data: {v}\n\n"))
}

pub async fn chat(state: &Arc<AppState>, req: &Req, ident: Identity) -> Resp {
    match chat_inner(state, req, ident).await {
        Ok(r) => r,
        Err(e) => Resp::error(&e),
    }
}

async fn chat_inner(
    state: &Arc<AppState>,
    req: &Req,
    ident: Identity,
) -> Result<Resp, MoxyError> {
    let body: ChatRequest = serde_json::from_slice(&req.body)
        .map_err(|e| MoxyError::validation(vec![format!("invalid chat request: {e}")]))?;
    if body.messages.is_empty() {
        return Err(MoxyError::validation(vec!["messages must not be empty".into()]));
    }
    let active = state.active();
    let route = active
        .resolved
        .route_by_path(&body.route)
        .ok_or_else(|| MoxyError::route_not_found(&body.route))?;
    let ai_spec = route.ai.clone().ok_or_else(|| {
        MoxyError::validation(vec![format!(
            "route {} has no <ai> middleware and cannot be used for chat",
            route.path
        )])
    })?;
    let model = ai_spec.model.clone().or(body.model).ok_or_else(|| {
        MoxyError::validation(vec![
            "no model: set <ai model> on the route or send \"model\"".into(),
        ])
    })?;
    let (suffix, payload, extra_headers) =
        ai::build_chat_request(ai_spec.provider, &model, &body.messages, body.max_tokens);
    let payload = Bytes::from(serde_json::to_vec(&payload).unwrap_or_default());

    let plan = pipeline::request_phase(
        state,
        &route,
        RequestInfo {
            suffix,
            content_length: Some(payload.len() as u64),
            caller: Caller {
                identity: Some(&ident),
                ip: req.client_ip,
            },
        },
    )?;

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept-encoding", HeaderValue::from_static("identity"));
    for (k, v) in extra_headers {
        if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
            headers.insert(k, v);
        }
    }
    apply_header_ops(&mut headers, &plan.header_ops);

    // Try upstreams until one answers (load balancing / failover).
    let mut tried = Vec::new();
    let mut last_err = None;
    let resp = loop {
        let Some(i) = state.lb.select(&route, &tried) else {
            return Err(last_err
                .unwrap_or_else(|| MoxyError::upstream_unavailable("no upstream available")));
        };
        tried.push(i);
        let base = &route.upstream.urls[i];
        let url = pipeline::target_url(base, &plan.suffix, None);
        let send = state
            .http
            .post(&url)
            .headers(headers.clone())
            .body(payload.clone())
            .send();
        match tokio::time::timeout(plan.timeout, send).await {
            Ok(Ok(r)) => break r,
            Ok(Err(e)) => {
                state.lb.mark_down(base.as_str());
                last_err = Some(if e.is_timeout() {
                    MoxyError::upstream_timeout(format!("upstream {} timed out", base))
                } else {
                    MoxyError::upstream_unavailable(format!("upstream {} is unreachable", base))
                });
            }
            Err(_) => {
                state.lb.mark_down(base.as_str());
                last_err = Some(MoxyError::upstream_timeout(format!(
                    "upstream {} timed out",
                    base
                )));
            }
        }
    };

    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let state = state.clone();
    let provider = ai_spec.provider;
    tokio::spawn(async move {
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let text: String = text.chars().take(2000).collect();
            let _ = tx
                .send(sse(&json!({"type": "error", "status": status.as_u16(), "message": text})))
                .await;
            return;
        }
        let mut tracker = UsageTracker::new(provider, true);
        let mut stream = resp.bytes_stream();
        let mut client_gone = false;
        loop {
            match tokio::time::timeout(plan.timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    for ev in tracker.feed(&chunk) {
                        if let StreamEvent::Delta(t) = ev {
                            if !client_gone
                                && tx.send(sse(&json!({"type": "delta", "text": t}))).await.is_err()
                            {
                                client_gone = true;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    let _ = tx
                        .send(sse(&json!({"type": "error", "status": 502, "message": format!("upstream stream failed: {e}")})))
                        .await;
                    break;
                }
                Err(_) => {
                    let _ = tx
                        .send(sse(&json!({"type": "error", "status": 504, "message": "upstream timed out"})))
                        .await;
                    break;
                }
            }
        }
        let (evs, total) = tracker.finish();
        for ev in evs {
            if let StreamEvent::Delta(t) = ev {
                let _ = tx.send(sse(&json!({"type": "delta", "text": t}))).await;
            }
        }
        pipeline::record_usage(&*state.kv, &plan.budgets, total);
        let _ = tx
            .send(sse(&json!({
                "type": "done",
                "usage": {"input": tracker.input, "output": tracker.output}
            })))
            .await;
    });
    Ok(Resp::sse(rx))
}
