//! `<ai>` middleware support: provider request/response body handling.
//!
//! - rewrite the model name in request bodies,
//! - count token usage from (streamed or buffered) responses,
//! - build chat requests for `/api/chat` and extract text deltas from replies.

use crate::config::Provider;
use serde_json::{Value, json};

/// Max bytes of a non-streamed response kept for usage parsing.
const MAX_BUFFERED: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Delta(String),
    /// Latest known token counts (values replace earlier ones).
    Usage {
        input: Option<u64>,
        output: Option<u64>,
    },
}

/// Does the request body ask for a streamed reply?
pub fn wants_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Rewrite the request body: set `model` (if `model` is given) and, for streamed OpenAI
/// requests, ask for a final usage chunk (if `include_usage`). Returns None if the body
/// is not a JSON object or nothing changed.
pub fn rewrite_request(
    provider: Provider,
    body: &[u8],
    model: Option<&str>,
    include_usage: bool,
) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object_mut()?;
    let mut changed = false;
    if let Some(m) = model {
        if obj.get("model").and_then(Value::as_str) != Some(m) {
            obj.insert("model".into(), json!(m));
            changed = true;
        }
    }
    if include_usage
        && provider == Provider::OpenAi
        && obj.get("stream").and_then(Value::as_bool) == Some(true)
    {
        let so = obj
            .entry("stream_options")
            .or_insert_with(|| json!({}));
        if let Some(so) = so.as_object_mut() {
            if so.get("include_usage").and_then(Value::as_bool) != Some(true) {
                so.insert("include_usage".into(), json!(true));
                changed = true;
            }
        }
    }
    changed.then(|| serde_json::to_vec(&v).unwrap_or_default())
}

/// Token usage from a complete (non-streamed) response body.
pub fn usage_from_json(provider: Provider, v: &Value) -> Option<(u64, u64)> {
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    match provider {
        Provider::OpenAi => {
            let u = v.get("usage")?;
            match (n(u, "prompt_tokens"), n(u, "completion_tokens")) {
                (None, None) => n(u, "total_tokens").map(|t| (t, 0)),
                (i, o) => Some((i.unwrap_or(0), o.unwrap_or(0))),
            }
        }
        Provider::Anthropic => {
            let u = v.get("usage")?;
            Some((n(u, "input_tokens")?, n(u, "output_tokens").unwrap_or(0)))
        }
        Provider::Ollama => {
            let i = n(v, "prompt_eval_count");
            let o = n(v, "eval_count");
            (i.is_some() || o.is_some()).then(|| (i.unwrap_or(0), o.unwrap_or(0)))
        }
    }
}

/// Parse one line of a streamed reply (SSE for OpenAI/Anthropic, NDJSON for Ollama).
pub fn parse_stream_line(provider: Provider, line: &str) -> Vec<StreamEvent> {
    let line = line.trim();
    let payload = match provider {
        Provider::Ollama => line,
        _ => match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => return Vec::new(),
        },
    };
    if payload.is_empty() || payload == "[DONE]" {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match provider {
        Provider::OpenAi => {
            if let Some(t) = v
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                out.push(StreamEvent::Delta(t.to_string()));
            }
            if let Some((i, o)) = v
                .get("usage")
                .filter(|u| u.is_object())
                .and_then(|_| usage_from_json(provider, &v))
            {
                out.push(StreamEvent::Usage {
                    input: Some(i),
                    output: Some(o),
                });
            }
        }
        Provider::Anthropic => match v.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => {
                if let Some(t) = v.pointer("/delta/text").and_then(Value::as_str) {
                    out.push(StreamEvent::Delta(t.to_string()));
                }
            }
            Some("message_start") => {
                if let Some(u) = v.pointer("/message/usage") {
                    out.push(StreamEvent::Usage {
                        input: u.get("input_tokens").and_then(Value::as_u64),
                        output: u.get("output_tokens").and_then(Value::as_u64),
                    });
                }
            }
            Some("message_delta") => {
                if let Some(u) = v.get("usage") {
                    out.push(StreamEvent::Usage {
                        input: u.get("input_tokens").and_then(Value::as_u64),
                        output: u.get("output_tokens").and_then(Value::as_u64),
                    });
                }
            }
            _ => {}
        },
        Provider::Ollama => {
            if let Some(t) = v
                .pointer("/message/content")
                .or_else(|| v.get("response"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                out.push(StreamEvent::Delta(t.to_string()));
            }
            if v.get("done").and_then(Value::as_bool) == Some(true) {
                if let Some((i, o)) = usage_from_json(provider, &v) {
                    out.push(StreamEvent::Usage {
                        input: Some(i),
                        output: Some(o),
                    });
                }
            }
        }
    }
    out
}

/// Incrementally tracks token usage (and text deltas) from a response body.
#[derive(Debug)]
pub struct UsageTracker {
    provider: Provider,
    streamed: bool,
    line: Vec<u8>,
    body: Vec<u8>,
    overflow: bool,
    pub input: u64,
    pub output: u64,
}

impl UsageTracker {
    pub fn new(provider: Provider, streamed: bool) -> Self {
        UsageTracker {
            provider,
            streamed,
            line: Vec::new(),
            body: Vec::new(),
            overflow: false,
            input: 0,
            output: 0,
        }
    }

    fn apply(&mut self, evs: &[StreamEvent]) {
        for e in evs {
            if let StreamEvent::Usage { input, output } = e {
                if let Some(i) = input {
                    self.input = *i;
                }
                if let Some(o) = output {
                    self.output = *o;
                }
            }
        }
    }

    /// Feed a body chunk; returns events from completed lines (streamed mode only).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        if !self.streamed {
            if self.body.len() + chunk.len() > MAX_BUFFERED {
                self.overflow = true;
            } else {
                self.body.extend_from_slice(chunk);
            }
            return Vec::new();
        }
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' {
                let line = String::from_utf8_lossy(&self.line).into_owned();
                self.line.clear();
                out.extend(parse_stream_line(self.provider, &line));
            } else if self.line.len() < MAX_BUFFERED {
                self.line.push(b);
            }
        }
        self.apply(&out);
        out
    }

    /// End of body; returns any final events and the total (input + output) tokens.
    pub fn finish(&mut self) -> (Vec<StreamEvent>, u64) {
        let mut out = Vec::new();
        if self.streamed {
            if !self.line.is_empty() {
                let line = String::from_utf8_lossy(&self.line).into_owned();
                self.line.clear();
                out = parse_stream_line(self.provider, &line);
                self.apply(&out);
            }
        } else if !self.overflow {
            if let Ok(v) = serde_json::from_slice::<Value>(&self.body) {
                if let Some((i, o)) = usage_from_json(self.provider, &v) {
                    self.input = i;
                    self.output = o;
                }
            }
        }
        (out, self.input + self.output)
    }
}

/// Build a streaming chat request for `/api/chat`.
/// Returns (path suffix relative to the upstream URL, JSON body, extra headers).
pub fn build_chat_request(
    provider: Provider,
    model: &str,
    messages: &[Value],
    max_tokens: Option<u64>,
) -> (String, Value, Vec<(String, String)>) {
    match provider {
        Provider::OpenAi => {
            let mut body = json!({
                "model": model,
                "messages": messages,
                "stream": true,
                "stream_options": {"include_usage": true},
            });
            if let Some(m) = max_tokens {
                body["max_tokens"] = json!(m);
            }
            ("/chat/completions".into(), body, vec![])
        }
        Provider::Anthropic => {
            let mut system = Vec::new();
            let mut msgs = Vec::new();
            for m in messages {
                if m.get("role").and_then(Value::as_str) == Some("system") {
                    if let Some(c) = m.get("content").and_then(Value::as_str) {
                        system.push(c.to_string());
                    }
                } else {
                    msgs.push(m.clone());
                }
            }
            let mut body = json!({
                "model": model,
                "messages": msgs,
                "max_tokens": max_tokens.unwrap_or(1024),
                "stream": true,
            });
            if !system.is_empty() {
                body["system"] = json!(system.join("\n\n"));
            }
            (
                "/messages".into(),
                body,
                vec![("anthropic-version".into(), "2023-06-01".into())],
            )
        }
        Provider::Ollama => {
            let mut body = json!({"model": model, "messages": messages, "stream": true});
            if let Some(m) = max_tokens {
                body["options"] = json!({"num_predict": m});
            }
            ("/api/chat".into(), body, vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_rewrite() {
        let out = rewrite_request(
            Provider::OpenAi,
            br#"{"model":"a","stream":true}"#,
            Some("b"),
            true,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "b");
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert!(rewrite_request(Provider::OpenAi, br#"{"model":"b"}"#, Some("b"), true).is_none());
        assert!(rewrite_request(Provider::OpenAi, b"not json", Some("b"), true).is_none());
    }

    #[test]
    fn openai_usage() {
        let v = json!({"usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}});
        assert_eq!(usage_from_json(Provider::OpenAi, &v), Some((3, 4)));
        let mut t = UsageTracker::new(Provider::OpenAi, true);
        let mut evs = t.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n");
        evs.extend(t.feed(b"\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n"));
        let text: String = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert_eq!(t.finish().1, 7);
    }

    #[test]
    fn anthropic_usage_stream() {
        let mut t = UsageTracker::new(Provider::Anthropic, true);
        t.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n");
        let evs = t.feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n");
        assert_eq!(evs, vec![StreamEvent::Delta("Hi".into())]);
        t.feed(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":6}}");
        assert_eq!(t.finish().1, 16);
    }

    #[test]
    fn ollama_usage() {
        let mut t = UsageTracker::new(Provider::Ollama, true);
        t.feed(b"{\"message\":{\"content\":\"a\"},\"done\":false}\n{\"message\":{\"content\":\"\"},\"done\":true,\"prompt_eval_count\":2,\"eval_count\":3}\n");
        assert_eq!(t.finish().1, 5);
        let mut b = UsageTracker::new(Provider::Ollama, false);
        b.feed(br#"{"done":true,"prompt_eval_count":1,"#);
        b.feed(br#""eval_count":1}"#);
        assert_eq!(b.finish().1, 2);
    }

    #[test]
    fn chat_requests() {
        let msgs = vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "hi"}),
        ];
        let (p, b, h) = build_chat_request(Provider::Anthropic, "claude", &msgs, None);
        assert_eq!(p, "/messages");
        assert_eq!(b["system"], "be brief");
        assert_eq!(b["messages"].as_array().unwrap().len(), 1);
        assert_eq!(h[0].0, "anthropic-version");
        let (p, b, _) = build_chat_request(Provider::OpenAi, "gpt", &msgs, Some(5));
        assert_eq!(p, "/chat/completions");
        assert_eq!(b["max_tokens"], 5);
        assert_eq!(build_chat_request(Provider::Ollama, "m", &msgs, None).0, "/api/chat");
    }
}
