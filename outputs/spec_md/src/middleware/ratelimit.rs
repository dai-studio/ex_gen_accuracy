//! In-memory fixed-window rate limiting. Counters reset when the server restarts.

use crate::config::RateLimitConfig;
use crate::error::MoxyError;
use http::StatusCode;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, Window>>,
}

struct Window {
    start_ms: u64,
    end_ms: u64,
    count: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn rate_limited(retry_after_secs: u64) -> MoxyError {
    MoxyError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "rate limit exceeded; retry later",
    )
    .with_header("retry-after", retry_after_secs.max(1).to_string())
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one request against `key`, rejecting it once the window's limit is reached.
    pub fn check(&self, key: &str, cfg: &RateLimitConfig) -> Result<(), MoxyError> {
        self.check_at(key, cfg, now_ms())
    }

    fn check_at(&self, key: &str, cfg: &RateLimitConfig, now: u64) -> Result<(), MoxyError> {
        let per = cfg.per.as_millis().max(1) as u64;
        let start = now - now % per;
        let mut map = self.windows.lock().unwrap();
        let w = map.entry(key.to_string()).or_insert(Window {
            start_ms: start,
            end_ms: start + per,
            count: 0,
        });
        if w.start_ms != start {
            w.start_ms = start;
            w.end_ms = start + per;
            w.count = 0;
        }
        if w.count >= cfg.requests {
            let retry = (w.end_ms.saturating_sub(now)).div_ceil(1000);
            return Err(rate_limited(retry));
        }
        w.count += 1;
        Ok(())
    }

    /// Drop windows that have ended.
    pub fn cleanup(&self) {
        let now = now_ms();
        self.windows.lock().unwrap().retain(|_, w| w.end_ms > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CountKey;
    use std::time::Duration;

    #[test]
    fn limits_within_window_and_resets() {
        let rl = RateLimiter::new();
        let cfg = RateLimitConfig {
            requests: 2,
            per: Duration::from_secs(60),
            key: CountKey::Ip,
        };
        let t0 = 120_000; // start of a window
        assert!(rl.check_at("k", &cfg, t0).is_ok());
        assert!(rl.check_at("k", &cfg, t0 + 1).is_ok());
        let err = rl.check_at("k", &cfg, t0 + 10_000).unwrap_err();
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(err.kind, "rate_limited");
        assert_eq!(err.headers[0], ("retry-after", "50".to_string()));
        // other keys are independent
        assert!(rl.check_at("other", &cfg, t0 + 10_000).is_ok());
        // next window
        assert!(rl.check_at("k", &cfg, t0 + 60_000).is_ok());
    }
}
