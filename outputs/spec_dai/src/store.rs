//! The KV Store: one table, Single Table Design, JSON values.
//!
//! Key prefixes: `user#`, `session#`, `auth#state#`, `apikey#`, `cache#`, `usage#`.
//! Every value may carry an `expiresAt` (RFC 3339). Expired entries are ignored on read
//! and removed by [`KvStore::sweep_expired`].

use chrono::{DateTime, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub type KvResult<T> = Result<T, String>;

pub trait KvStore: Send + Sync {
    fn get_raw(&self, key: &str) -> KvResult<Option<Value>>;
    fn put_raw(&self, key: &str, value: &Value) -> KvResult<()>;
    fn delete(&self, key: &str) -> KvResult<bool>;
    /// All live entries whose key starts with `prefix`, in key order.
    fn scan_prefix(&self, prefix: &str) -> KvResult<Vec<(String, Value)>>;
    /// Atomic read-modify-write. `f` receives the current live value (or None) and
    /// returns the new value (None deletes).
    fn update(
        &self,
        key: &str,
        f: &mut dyn FnMut(Option<Value>) -> Option<Value>,
    ) -> KvResult<Option<Value>>;
    /// Delete all expired entries; returns how many were removed.
    fn sweep_expired(&self) -> KvResult<usize>;
}

pub fn is_expired(v: &Value, now: DateTime<Utc>) -> bool {
    v.get("expiresAt")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|t| t.with_timezone(&Utc) <= now)
}

fn live(v: Value) -> Option<Value> {
    (!is_expired(&v, Utc::now())).then_some(v)
}

/// Typed helpers on top of the raw JSON interface.
pub trait KvExt {
    fn get<T: DeserializeOwned>(&self, key: &str) -> KvResult<Option<T>>;
    fn put<T: Serialize>(&self, key: &str, value: &T) -> KvResult<()>;
}

impl<S: KvStore + ?Sized> KvExt for S {
    fn get<T: DeserializeOwned>(&self, key: &str) -> KvResult<Option<T>> {
        match self.get_raw(key)? {
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| format!("decoding {key}: {e}")),
            None => Ok(None),
        }
    }
    fn put<T: Serialize>(&self, key: &str, value: &T) -> KvResult<()> {
        let v = serde_json::to_value(value).map_err(|e| e.to_string())?;
        self.put_raw(key, &v)
    }
}

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("moxy");

/// redb-backed store in a single file.
pub struct RedbStore {
    db: Database,
}

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

impl RedbStore {
    pub fn open(path: &Path) -> KvResult<Self> {
        let db = Database::create(path).map_err(e)?;
        // Make sure the table exists so read transactions never fail on a fresh file.
        let tx = db.begin_write().map_err(e)?;
        tx.open_table(TABLE).map_err(e)?;
        tx.commit().map_err(e)?;
        Ok(RedbStore { db })
    }
}

impl KvStore for RedbStore {
    fn get_raw(&self, key: &str) -> KvResult<Option<Value>> {
        let tx = self.db.begin_read().map_err(e)?;
        let t = tx.open_table(TABLE).map_err(e)?;
        let Some(v) = t.get(key).map_err(e)? else {
            return Ok(None);
        };
        let v: Value = serde_json::from_slice(v.value()).map_err(e)?;
        Ok(live(v))
    }

    fn put_raw(&self, key: &str, value: &Value) -> KvResult<()> {
        let bytes = serde_json::to_vec(value).map_err(e)?;
        let tx = self.db.begin_write().map_err(e)?;
        {
            let mut t = tx.open_table(TABLE).map_err(e)?;
            t.insert(key, bytes.as_slice()).map_err(e)?;
        }
        tx.commit().map_err(e)
    }

    fn delete(&self, key: &str) -> KvResult<bool> {
        let tx = self.db.begin_write().map_err(e)?;
        let existed = {
            let mut t = tx.open_table(TABLE).map_err(e)?;
            t.remove(key).map_err(e)?.is_some()
        };
        tx.commit().map_err(e)?;
        Ok(existed)
    }

    fn scan_prefix(&self, prefix: &str) -> KvResult<Vec<(String, Value)>> {
        let tx = self.db.begin_read().map_err(e)?;
        let t = tx.open_table(TABLE).map_err(e)?;
        let mut out = Vec::new();
        for item in t.range(prefix..).map_err(e)? {
            let (k, v) = item.map_err(e)?;
            let k = k.value();
            if !k.starts_with(prefix) {
                break;
            }
            let v: Value = serde_json::from_slice(v.value()).map_err(e)?;
            if let Some(v) = live(v) {
                out.push((k.to_string(), v));
            }
        }
        Ok(out)
    }

    fn update(
        &self,
        key: &str,
        f: &mut dyn FnMut(Option<Value>) -> Option<Value>,
    ) -> KvResult<Option<Value>> {
        let tx = self.db.begin_write().map_err(e)?;
        let new = {
            let mut t = tx.open_table(TABLE).map_err(e)?;
            let cur = match t.get(key).map_err(e)? {
                Some(v) => serde_json::from_slice::<Value>(v.value())
                    .ok()
                    .and_then(live),
                None => None,
            };
            let new = f(cur);
            match &new {
                Some(v) => {
                    let bytes = serde_json::to_vec(v).map_err(e)?;
                    t.insert(key, bytes.as_slice()).map_err(e)?;
                }
                None => {
                    t.remove(key).map_err(e)?;
                }
            }
            new
        };
        tx.commit().map_err(e)?;
        Ok(new)
    }

    fn sweep_expired(&self) -> KvResult<usize> {
        let now = Utc::now();
        let tx = self.db.begin_write().map_err(e)?;
        let n = {
            let mut t = tx.open_table(TABLE).map_err(e)?;
            let mut expired = Vec::new();
            for item in t.iter().map_err(e)? {
                let (k, v) = item.map_err(e)?;
                if let Ok(v) = serde_json::from_slice::<Value>(v.value()) {
                    if is_expired(&v, now) {
                        expired.push(k.value().to_string());
                    }
                }
            }
            for k in &expired {
                t.remove(k.as_str()).map_err(e)?;
            }
            expired.len()
        };
        tx.commit().map_err(e)?;
        Ok(n)
    }
}

/// In-memory store (tests, tooling).
#[derive(Default)]
pub struct MemStore {
    map: parking_lot::Mutex<BTreeMap<String, Value>>,
}

impl KvStore for MemStore {
    fn get_raw(&self, key: &str) -> KvResult<Option<Value>> {
        Ok(self.map.lock().get(key).cloned().and_then(live))
    }
    fn put_raw(&self, key: &str, value: &Value) -> KvResult<()> {
        self.map.lock().insert(key.to_string(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &str) -> KvResult<bool> {
        Ok(self.map.lock().remove(key).is_some())
    }
    fn scan_prefix(&self, prefix: &str) -> KvResult<Vec<(String, Value)>> {
        Ok(self
            .map
            .lock()
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .filter_map(|(k, v)| live(v.clone()).map(|v| (k.clone(), v)))
            .collect())
    }
    fn update(
        &self,
        key: &str,
        f: &mut dyn FnMut(Option<Value>) -> Option<Value>,
    ) -> KvResult<Option<Value>> {
        let mut m = self.map.lock();
        let cur = m.get(key).cloned().and_then(live);
        let new = f(cur);
        match &new {
            Some(v) => {
                m.insert(key.to_string(), v.clone());
            }
            None => {
                m.remove(key);
            }
        }
        Ok(new)
    }
    fn sweep_expired(&self) -> KvResult<usize> {
        let now = Utc::now();
        let mut m = self.map.lock();
        let before = m.len();
        m.retain(|_, v| !is_expired(v, now));
        Ok(before - m.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exercise(s: &dyn KvStore) {
        let past = (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        let future = (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
        s.put_raw("user#a", &json!({"sub": "a"})).unwrap();
        s.put_raw("user#b", &json!({"sub": "b", "expiresAt": future})).unwrap();
        s.put_raw("user#c", &json!({"sub": "c", "expiresAt": past})).unwrap();
        s.put_raw("usex", &json!({})).unwrap();
        assert_eq!(s.get_raw("user#a").unwrap().unwrap()["sub"], "a");
        assert!(s.get_raw("user#c").unwrap().is_none(), "expired is ignored");
        let keys: Vec<String> = s
            .scan_prefix("user#")
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, vec!["user#a", "user#b"]);
        let v = s
            .update("usage#k", &mut |cur| {
                let n = cur.and_then(|v| v["tokens"].as_u64()).unwrap_or(0);
                Some(json!({"tokens": n + 5}))
            })
            .unwrap();
        assert_eq!(v.unwrap()["tokens"], 5);
        s.update("usage#k", &mut |cur| {
            let n = cur.and_then(|v| v["tokens"].as_u64()).unwrap_or(0);
            Some(json!({"tokens": n + 5}))
        })
        .unwrap();
        assert_eq!(s.get_raw("usage#k").unwrap().unwrap()["tokens"], 10);
        assert_eq!(s.sweep_expired().unwrap(), 1);
        assert!(s.delete("user#a").unwrap());
        assert!(!s.delete("user#a").unwrap());
    }

    #[test]
    fn mem_store() {
        exercise(&MemStore::default());
    }

    #[test]
    fn redb_store() {
        let dir = tempfile::tempdir().unwrap();
        let s = RedbStore::open(&dir.path().join("moxy.redb")).unwrap();
        exercise(&s);
    }
}
