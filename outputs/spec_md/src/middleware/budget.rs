//! Token budgets, counted per window in the key-value store under
//! `usage#<api key id or user subject>#<route path>#<window start>`.

use crate::config::{BudgetWindow, TokenBudgetConfig};
use crate::error::MoxyError;
use crate::kv::KvStore;
use anyhow::Result;
use chrono::{DateTime, Utc};
use http::StatusCode;
use serde_json::json;

/// Start of the window containing `now` (unix seconds, aligned to the epoch).
pub fn window_start(window: BudgetWindow, now: i64) -> i64 {
    now - now.rem_euclid(window.seconds())
}

pub fn usage_key(identity: &str, route_path: &str, start: i64) -> String {
    format!("usage#{identity}#{route_path}#{start}")
}

pub fn budget_exceeded(retry_after_secs: i64) -> MoxyError {
    MoxyError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "token_budget_exceeded",
        "the token budget for the current window has been used up",
    )
    .with_header("retry-after", retry_after_secs.max(1).to_string())
}

pub fn used_tokens(kv: &dyn KvStore, key: &str) -> Result<u64> {
    Ok(kv
        .get(key)?
        .and_then(|v| v.get("tokens").and_then(|t| t.as_u64()))
        .unwrap_or(0))
}

/// Reject when the budget for the current window has been used up.
pub fn check(
    kv: &dyn KvStore,
    route_path: &str,
    cfg: &TokenBudgetConfig,
    identity: &str,
) -> Result<(), MoxyError> {
    check_at(kv, route_path, cfg, identity, Utc::now().timestamp())
}

fn check_at(
    kv: &dyn KvStore,
    route_path: &str,
    cfg: &TokenBudgetConfig,
    identity: &str,
    now: i64,
) -> Result<(), MoxyError> {
    let start = window_start(cfg.window, now);
    let used = used_tokens(kv, &usage_key(identity, route_path, start))?;
    if used >= cfg.tokens {
        return Err(budget_exceeded(start + cfg.window.seconds() - now));
    }
    Ok(())
}

/// Add `tokens` to every distinct (identity, window) counter.
pub fn record(
    kv: &dyn KvStore,
    route_path: &str,
    counters: &[(String, BudgetWindow)],
    tokens: u64,
) -> Result<()> {
    if tokens == 0 {
        return Ok(());
    }
    let now = Utc::now().timestamp();
    let mut done = std::collections::HashSet::new();
    for (identity, window) in counters {
        let start = window_start(*window, now);
        let key = usage_key(identity, route_path, start);
        if !done.insert(key.clone()) {
            continue;
        }
        let expires: Option<DateTime<Utc>> = DateTime::from_timestamp(start + window.seconds(), 0);
        kv.update(&key, &mut |cur| {
            let used = cur
                .and_then(|v| v.get("tokens").and_then(|t| t.as_u64()))
                .unwrap_or(0);
            Some((json!({ "tokens": used + tokens }), expires))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CountKey;
    use crate::kv::MemoryStore;

    #[test]
    fn windows_align() {
        assert_eq!(window_start(BudgetWindow::Hour, 7300), 7200);
        assert_eq!(window_start(BudgetWindow::Day, 86_400 * 3 + 5), 86_400 * 3);
        assert_eq!(window_start(BudgetWindow::ThirtyDays, 100), 0);
    }

    #[test]
    fn records_and_rejects() {
        let kv = MemoryStore::new();
        let cfg = TokenBudgetConfig {
            tokens: 100,
            window: BudgetWindow::Hour,
            key: CountKey::ApiKey,
        };
        assert!(check(&kv, "/a/*", &cfg, "k1").is_ok());
        record(&kv, "/a/*", &[("k1".into(), BudgetWindow::Hour)], 60).unwrap();
        assert!(check(&kv, "/a/*", &cfg, "k1").is_ok());
        // duplicate counters in one call are only counted once
        record(
            &kv,
            "/a/*",
            &[
                ("k1".into(), BudgetWindow::Hour),
                ("k1".into(), BudgetWindow::Hour),
            ],
            40,
        )
        .unwrap();
        let err = check(&kv, "/a/*", &cfg, "k1").unwrap_err();
        assert_eq!(err.kind, "token_budget_exceeded");
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(err.headers.iter().any(|(n, _)| *n == "retry-after"));
        // other identities and routes are unaffected
        assert!(check(&kv, "/a/*", &cfg, "k2").is_ok());
        assert!(check(&kv, "/b/*", &cfg, "k1").is_ok());
        let start = window_start(BudgetWindow::Hour, Utc::now().timestamp());
        assert_eq!(
            used_tokens(&kv, &usage_key("k1", "/a/*", start)).unwrap(),
            100
        );
    }
}
