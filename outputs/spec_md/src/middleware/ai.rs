//! The `<ai>` middleware: provider-aware request rewriting and token counting.

use crate::config::{AiConfig, Provider};
use bytes::Bytes;
use serde_json::Value;

/// What the ai middleware learned about a request body.
#[derive(Debug, Clone, Default)]
pub struct AiRequestInfo {
    /// The request asked for a streamed response.
    pub stream: bool,
    /// The model the request will use (after any override).
    pub model: Option<String>,
}

/// Rewrite the model name (when the route overrides it) and, when tokens
/// must be counted on a streamed OpenAI request, ask for usage in the stream.
pub fn transform_request(ai: &AiConfig, body: &[u8], ensure_usage: bool) -> (Bytes, AiRequestInfo) {
    let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(body) else {
        return (Bytes::copy_from_slice(body), AiRequestInfo::default());
    };
    let mut changed = false;
    if let Some(model) = &ai.model
        && obj.get("model").and_then(Value::as_str) != Some(model.as_str())
    {
        obj.insert("model".into(), Value::String(model.clone()));
        changed = true;
    }
    let stream = match ai.provider {
        // Ollama streams unless told otherwise.
        Provider::Ollama => obj.get("stream").and_then(Value::as_bool) != Some(false),
        _ => obj.get("stream").and_then(Value::as_bool) == Some(true),
    };
    if ensure_usage && stream && ai.provider == Provider::OpenAi {
        let opts = obj
            .entry("stream_options")
            .or_insert_with(|| Value::Object(Default::default()));
        if let Value::Object(o) = opts
            && o.get("include_usage").and_then(Value::as_bool) != Some(true)
        {
            o.insert("include_usage".into(), Value::Bool(true));
            changed = true;
        }
    }
    let info = AiRequestInfo {
        stream,
        model: obj.get("model").and_then(Value::as_str).map(str::to_string),
    };
    let out = if changed {
        Bytes::from(serde_json::to_vec(&Value::Object(obj)).unwrap_or_else(|_| body.to_vec()))
    } else {
        Bytes::copy_from_slice(body)
    };
    (out, info)
}

/// Accumulates token usage from provider response bodies or stream events.
#[derive(Debug, Clone)]
pub struct UsageTracker {
    provider: Provider,
    input: u64,
    output: u64,
    total: Option<u64>,
    seen: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

fn num(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

impl UsageTracker {
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            input: 0,
            output: 0,
            total: None,
            seen: false,
        }
    }

    /// Observe one JSON document: a whole response body or one stream event.
    pub fn observe(&mut self, v: &Value) {
        match self.provider {
            Provider::OpenAi => {
                let usage = v.get("usage").filter(|u| u.is_object()).or_else(|| {
                    v.get("response")
                        .and_then(|r| r.get("usage"))
                        .filter(|u| u.is_object())
                });
                if let Some(u) = usage {
                    self.seen = true;
                    self.input = num(u, "prompt_tokens")
                        .or(num(u, "input_tokens"))
                        .unwrap_or(0);
                    self.output = num(u, "completion_tokens")
                        .or(num(u, "output_tokens"))
                        .unwrap_or(0);
                    self.total = num(u, "total_tokens");
                }
            }
            Provider::Anthropic => {
                let usage = v.get("usage").filter(|u| u.is_object()).or_else(|| {
                    v.get("message")
                        .and_then(|m| m.get("usage"))
                        .filter(|u| u.is_object())
                });
                if let Some(u) = usage {
                    self.seen = true;
                    let input = num(u, "input_tokens").unwrap_or(0)
                        + num(u, "cache_creation_input_tokens").unwrap_or(0)
                        + num(u, "cache_read_input_tokens").unwrap_or(0);
                    // message_start carries input, message_delta carries cumulative output
                    self.input = self.input.max(input);
                    self.output = self.output.max(num(u, "output_tokens").unwrap_or(0));
                }
            }
            Provider::Ollama => {
                if v.get("prompt_eval_count").is_some() || v.get("eval_count").is_some() {
                    self.seen = true;
                    self.input = num(v, "prompt_eval_count").unwrap_or(0);
                    self.output = num(v, "eval_count").unwrap_or(0);
                }
            }
        }
    }

    pub fn usage(&self) -> Option<Usage> {
        self.seen.then(|| Usage {
            input_tokens: self.input,
            output_tokens: self.output,
            total_tokens: self.total.unwrap_or(self.input + self.output),
        })
    }
}

/// The text delta carried by one streamed event, if any.
pub fn delta_text(provider: Provider, v: &Value) -> Option<String> {
    let s = match provider {
        Provider::OpenAi => v
            .get("choices")?
            .get(0)?
            .get("delta")?
            .get("content")?
            .as_str()?,
        Provider::Anthropic => {
            if v.get("type")?.as_str()? != "content_block_delta" {
                return None;
            }
            let delta = v.get("delta")?;
            if delta.get("type")?.as_str()? != "text_delta" {
                return None;
            }
            delta.get("text")?.as_str()?
        }
        Provider::Ollama => v.get("message")?.get("content")?.as_str()?,
    };
    (!s.is_empty()).then(|| s.to_string())
}

/// An error reported inside a stream, if this event is one.
pub fn stream_error(v: &Value) -> Option<String> {
    let err = v.get("error")?;
    Some(
        err.get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string()),
    )
}

/// Splits a byte stream into JSON events: SSE `data:` lines or NDJSON lines.
#[derive(Default)]
pub struct EventSplitter {
    buf: Vec<u8>,
}

impl EventSplitter {
    pub fn push(&mut self, chunk: &[u8], mut on_event: impl FnMut(Value)) {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Some(v) = parse_line(&line) {
                on_event(v);
            }
        }
    }

    pub fn finish(&mut self, mut on_event: impl FnMut(Value)) {
        let line = std::mem::take(&mut self.buf);
        if let Some(v) = parse_line(&line) {
            on_event(v);
        }
    }
}

fn parse_line(line: &[u8]) -> Option<Value> {
    let s = std::str::from_utf8(line).ok()?.trim();
    let payload = s.strip_prefix("data:").map(str::trim).unwrap_or(s);
    if !payload.starts_with('{') {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Watches a proxied response body and extracts token usage without altering it.
pub struct BodyObserver {
    tracker: UsageTracker,
    mode: ObserveMode,
}

enum ObserveMode {
    Events(EventSplitter),
    Whole { buf: Vec<u8>, overflow: bool },
}

const MAX_OBSERVED_BODY: usize = 16 * 1024 * 1024;

impl BodyObserver {
    /// Streams (`text/event-stream`, NDJSON) are scanned line by line; anything else is parsed whole.
    pub fn new(provider: Provider, content_type: &str) -> Self {
        let ct = content_type.to_ascii_lowercase();
        let streaming =
            ct.contains("event-stream") || ct.contains("ndjson") || ct.contains("jsonl");
        Self {
            tracker: UsageTracker::new(provider),
            mode: if streaming {
                ObserveMode::Events(EventSplitter::default())
            } else {
                ObserveMode::Whole {
                    buf: Vec::new(),
                    overflow: false,
                }
            },
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        match &mut self.mode {
            ObserveMode::Events(split) => {
                let tracker = &mut self.tracker;
                split.push(chunk, |v| tracker.observe(&v));
            }
            ObserveMode::Whole { buf, overflow } => {
                if buf.len() + chunk.len() > MAX_OBSERVED_BODY {
                    *overflow = true;
                    buf.clear();
                } else if !*overflow {
                    buf.extend_from_slice(chunk);
                }
            }
        }
    }

    pub fn finish(mut self) -> Option<Usage> {
        match &mut self.mode {
            ObserveMode::Events(split) => {
                let tracker = &mut self.tracker;
                split.finish(|v| tracker.observe(&v));
            }
            ObserveMode::Whole { buf, overflow } => {
                if !*overflow && let Ok(v) = serde_json::from_slice::<Value>(buf) {
                    self.tracker.observe(&v);
                }
            }
        }
        self.tracker.usage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ai(provider: Provider, model: Option<&str>) -> AiConfig {
        AiConfig {
            provider,
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn overrides_model_and_requests_usage() {
        let body = br#"{"model":"gpt-x","stream":true,"messages":[]}"#;
        let (out, info) = transform_request(&ai(Provider::OpenAi, Some("gpt-4o-mini")), body, true);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-4o-mini");
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert!(info.stream);
        assert_eq!(info.model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn leaves_body_untouched_when_nothing_to_do() {
        let body = br#"{"model": "m", "messages": []}"#;
        let (out, info) = transform_request(&ai(Provider::OpenAi, None), body, true);
        assert_eq!(&out[..], &body[..]);
        assert!(!info.stream);
        let (out, _) = transform_request(&ai(Provider::OpenAi, Some("x")), b"not json", true);
        assert_eq!(&out[..], b"not json");
    }

    #[test]
    fn ollama_streams_by_default() {
        let (_, info) =
            transform_request(&ai(Provider::Ollama, None), br#"{"model":"llama3"}"#, false);
        assert!(info.stream);
        let (_, info) =
            transform_request(&ai(Provider::Ollama, None), br#"{"stream":false}"#, false);
        assert!(!info.stream);
    }

    #[test]
    fn openai_usage() {
        let mut t = UsageTracker::new(Provider::OpenAi);
        t.observe(
            &json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}),
        );
        assert_eq!(t.usage().unwrap().total_tokens, 15);
        let mut t = UsageTracker::new(Provider::OpenAi);
        t.observe(&json!({"type": "response.completed", "response": {"usage": {"input_tokens": 3, "output_tokens": 4}}}));
        assert_eq!(t.usage().unwrap().total_tokens, 7);
        let mut t = UsageTracker::new(Provider::OpenAi);
        t.observe(&json!({"usage": null, "choices": []}));
        assert!(t.usage().is_none());
    }

    #[test]
    fn anthropic_stream_usage() {
        let mut t = UsageTracker::new(Provider::Anthropic);
        t.observe(&json!({"type": "message_start", "message": {"usage": {"input_tokens": 25, "output_tokens": 1}}}));
        t.observe(&json!({"type": "message_delta", "usage": {"output_tokens": 15}}));
        let u = t.usage().unwrap();
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.total_tokens),
            (25, 15, 40)
        );
    }

    #[test]
    fn ollama_usage() {
        let mut t = UsageTracker::new(Provider::Ollama);
        t.observe(&json!({"message": {"content": "hi"}, "done": false}));
        assert!(t.usage().is_none());
        t.observe(&json!({"done": true, "prompt_eval_count": 7, "eval_count": 3}));
        assert_eq!(t.usage().unwrap().total_tokens, 10);
    }

    #[test]
    fn deltas() {
        assert_eq!(
            delta_text(
                Provider::OpenAi,
                &json!({"choices": [{"delta": {"content": "Hel"}}]})
            )
            .as_deref(),
            Some("Hel")
        );
        assert_eq!(
            delta_text(
                Provider::Anthropic,
                &json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "lo"}})
            )
            .as_deref(),
            Some("lo")
        );
        assert_eq!(
            delta_text(Provider::Ollama, &json!({"message": {"content": "!"}})).as_deref(),
            Some("!")
        );
        assert_eq!(delta_text(Provider::OpenAi, &json!({"choices": []})), None);
    }

    #[test]
    fn observer_handles_split_sse() {
        let mut o = BodyObserver::new(Provider::OpenAi, "text/event-stream; charset=utf-8");
        o.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"choices\":[],\"usa",
        );
        o.feed(b"ge\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n");
        assert_eq!(o.finish().unwrap().total_tokens, 5);
    }

    #[test]
    fn observer_parses_whole_json() {
        let mut o = BodyObserver::new(Provider::Anthropic, "application/json");
        o.feed(b"{\"usage\":{\"input_tokens\":4,");
        o.feed(b"\"output_tokens\":6}}");
        assert_eq!(o.finish().unwrap().total_tokens, 10);
    }
}
