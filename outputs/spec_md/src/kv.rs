//! The embedded key-value store (Single Table Design).
//!
//! Every entity lives in one table and is distinguished by its key prefix:
//! `user#`, `session#`, `auth#state#`, `apikey#`, `cache#` and `usage#`.
//! Values are JSON and may carry an optional expiry; expired entries are
//! ignored on read and removed by a periodic sweep.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, RwLock};

/// The read-modify-write closure of [`KvStore::update`]: receives the current
/// value and returns the new value with its expiry, or `None` to delete.
pub type UpdateFn<'a> = dyn FnMut(Option<Value>) -> Option<(Value, Option<DateTime<Utc>>)> + 'a;

/// Storage abstraction so the engine can be replaced without touching callers.
pub trait KvStore: Send + Sync {
    /// Read a value, ignoring expired entries.
    fn get(&self, key: &str) -> Result<Option<Value>>;
    /// Write a value with an optional expiry time.
    fn put(&self, key: &str, value: Value, expires_at: Option<DateTime<Utc>>) -> Result<()>;
    /// Delete a key; returns whether it existed.
    fn delete(&self, key: &str) -> Result<bool>;
    /// Atomically read and delete a value (expired entries count as absent).
    fn take(&self, key: &str) -> Result<Option<Value>>;
    /// All unexpired entries whose key starts with `prefix`, in key order.
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Value)>>;
    /// Atomic read-modify-write. `f` receives the current unexpired value and
    /// returns the new value and expiry, or `None` to delete the key.
    fn update(&self, key: &str, f: &mut UpdateFn) -> Result<()>;
    /// Remove all expired entries; returns how many were removed.
    fn sweep_expired(&self) -> Result<usize>;
    /// Delete every entry under `prefix` for which `pred` returns true.
    fn delete_where(&self, prefix: &str, pred: &dyn Fn(&Value) -> bool) -> Result<usize>;
    /// Flush and release the store.
    fn close(&self) -> Result<()>;
}

/// Typed helpers on top of [`KvStore`].
pub trait KvStoreExt {
    fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>>;
    fn put_json<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<()>;
}

impl<S: KvStore + ?Sized> KvStoreExt for S {
    fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.get(key)? {
            Some(v) => Ok(Some(
                serde_json::from_value(v).with_context(|| format!("decoding {key}"))?,
            )),
            None => Ok(None),
        }
    }

    fn put_json<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        self.put(key, serde_json::to_value(value)?, expires_at)
    }
}

/// The stored envelope: the JSON value plus its optional expiry (unix milliseconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    v: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exp: Option<i64>,
}

impl Envelope {
    fn new(v: Value, expires_at: Option<DateTime<Utc>>) -> Self {
        Self {
            v,
            exp: expires_at.map(|t| t.timestamp_millis()),
        }
    }

    fn expired(&self, now_ms: i64) -> bool {
        self.exp.is_some_and(|e| e <= now_ms)
    }

    fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("envelope serialises")
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Upper bound for a prefix range scan.
fn prefix_end(prefix: &str) -> String {
    let mut end = prefix.to_string();
    end.push(char::MAX);
    end
}

// ---------------------------------------------------------------------------
// redb implementation
// ---------------------------------------------------------------------------

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("moxy");

pub const STORE_FILE: &str = "moxy.redb";

pub struct RedbStore {
    db: RwLock<Option<Database>>,
}

impl RedbStore {
    /// Open (or create) `moxy.redb` inside `dir`, creating the directory if needed.
    pub fn open_dir(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating data directory {}", dir.display()))?;
        Self::open(&dir.join(STORE_FILE))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::create(path)
            .with_context(|| format!("opening key-value store {}", path.display()))?;
        let txn = db.begin_write()?;
        txn.open_table(TABLE)?;
        txn.commit()?;
        Ok(Self {
            db: RwLock::new(Some(db)),
        })
    }

    fn with_db<T>(&self, f: impl FnOnce(&Database) -> Result<T>) -> Result<T> {
        let guard = self.db.read().expect("kv lock poisoned");
        match guard.as_ref() {
            Some(db) => f(db),
            None => anyhow::bail!("key-value store is closed"),
        }
    }
}

impl KvStore for RedbStore {
    fn get(&self, key: &str) -> Result<Option<Value>> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let table = txn.open_table(TABLE)?;
            let Some(raw) = table.get(key)? else {
                return Ok(None);
            };
            Ok(Envelope::decode(raw.value())
                .filter(|e| !e.expired(now_ms()))
                .map(|e| e.v))
        })
    }

    fn put(&self, key: &str, value: Value, expires_at: Option<DateTime<Utc>>) -> Result<()> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(TABLE)?;
                let bytes = Envelope::new(value, expires_at).encode();
                table.insert(key, bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    fn delete(&self, key: &str) -> Result<bool> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let existed = {
                let mut table = txn.open_table(TABLE)?;
                table.remove(key)?.is_some()
            };
            txn.commit()?;
            Ok(existed)
        })
    }

    fn take(&self, key: &str) -> Result<Option<Value>> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let value = {
                let mut table = txn.open_table(TABLE)?;
                let removed = table.remove(key)?;
                removed
                    .and_then(|raw| Envelope::decode(raw.value()))
                    .filter(|e| !e.expired(now_ms()))
                    .map(|e| e.v)
            };
            txn.commit()?;
            Ok(value)
        })
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Value)>> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let table = txn.open_table(TABLE)?;
            let end = prefix_end(prefix);
            let now = now_ms();
            let mut out = Vec::new();
            for item in table.range(prefix..end.as_str())? {
                let (k, v) = item?;
                if let Some(env) = Envelope::decode(v.value())
                    && !env.expired(now)
                {
                    out.push((k.value().to_string(), env.v));
                }
            }
            Ok(out)
        })
    }

    fn update(&self, key: &str, f: &mut UpdateFn) -> Result<()> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(TABLE)?;
                let current = table
                    .get(key)?
                    .and_then(|raw| Envelope::decode(raw.value()))
                    .filter(|e| !e.expired(now_ms()))
                    .map(|e| e.v);
                match f(current) {
                    Some((v, exp)) => {
                        let bytes = Envelope::new(v, exp).encode();
                        table.insert(key, bytes.as_slice())?;
                    }
                    None => {
                        table.remove(key)?;
                    }
                }
            }
            txn.commit()?;
            Ok(())
        })
    }

    fn sweep_expired(&self) -> Result<usize> {
        self.with_db(|db| {
            let now = now_ms();
            let txn = db.begin_write()?;
            let removed = {
                let mut table = txn.open_table(TABLE)?;
                let mut n = 0usize;
                table.retain(|_, v| {
                    let keep = !Envelope::decode(v).is_some_and(|e| e.expired(now));
                    if !keep {
                        n += 1;
                    }
                    keep
                })?;
                n
            };
            txn.commit()?;
            Ok(removed)
        })
    }

    fn delete_where(&self, prefix: &str, pred: &dyn Fn(&Value) -> bool) -> Result<usize> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let removed = {
                let mut table = txn.open_table(TABLE)?;
                let end = prefix_end(prefix);
                let mut n = 0usize;
                table.retain_in(prefix..end.as_str(), |_, v| {
                    let doomed = Envelope::decode(v).is_some_and(|e| pred(&e.v));
                    if doomed {
                        n += 1;
                    }
                    !doomed
                })?;
                n
            };
            txn.commit()?;
            Ok(removed)
        })
    }

    fn close(&self) -> Result<()> {
        let mut guard = self.db.write().expect("kv lock poisoned");
        // Dropping the database releases the file lock; every commit is already durable.
        guard.take();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// In-memory implementation (tests and tooling)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MemoryStore {
    map: Mutex<BTreeMap<String, Envelope>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KvStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<Value>> {
        let map = self.map.lock().unwrap();
        Ok(map
            .get(key)
            .filter(|e| !e.expired(now_ms()))
            .map(|e| e.v.clone()))
    }

    fn put(&self, key: &str, value: Value, expires_at: Option<DateTime<Utc>>) -> Result<()> {
        self.map
            .lock()
            .unwrap()
            .insert(key.to_string(), Envelope::new(value, expires_at));
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<bool> {
        Ok(self.map.lock().unwrap().remove(key).is_some())
    }

    fn take(&self, key: &str) -> Result<Option<Value>> {
        Ok(self
            .map
            .lock()
            .unwrap()
            .remove(key)
            .filter(|e| !e.expired(now_ms()))
            .map(|e| e.v))
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Value)>> {
        let map = self.map.lock().unwrap();
        let now = now_ms();
        Ok(map
            .range(prefix.to_string()..prefix_end(prefix))
            .filter(|(_, e)| !e.expired(now))
            .map(|(k, e)| (k.clone(), e.v.clone()))
            .collect())
    }

    fn update(&self, key: &str, f: &mut UpdateFn) -> Result<()> {
        let mut map = self.map.lock().unwrap();
        let current = map
            .get(key)
            .filter(|e| !e.expired(now_ms()))
            .map(|e| e.v.clone());
        match f(current) {
            Some((v, exp)) => {
                map.insert(key.to_string(), Envelope::new(v, exp));
            }
            None => {
                map.remove(key);
            }
        }
        Ok(())
    }

    fn sweep_expired(&self) -> Result<usize> {
        let mut map = self.map.lock().unwrap();
        let now = now_ms();
        let before = map.len();
        map.retain(|_, e| !e.expired(now));
        Ok(before - map.len())
    }

    fn delete_where(&self, prefix: &str, pred: &dyn Fn(&Value) -> bool) -> Result<usize> {
        let mut map = self.map.lock().unwrap();
        let before = map.len();
        map.retain(|k, e| !(k.starts_with(prefix) && pred(&e.v)));
        Ok(before - map.len())
    }

    fn close(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;

    fn exercise(store: &dyn KvStore) {
        store.put("user#a", json!({"n": 1}), None).unwrap();
        store.put("user#b", json!({"n": 2}), None).unwrap();
        store.put("session#x", json!({"s": true}), None).unwrap();
        store
            .put(
                "user#old",
                json!({"n": 0}),
                Some(Utc::now() - Duration::seconds(1)),
            )
            .unwrap();

        assert_eq!(store.get("user#a").unwrap(), Some(json!({"n": 1})));
        assert_eq!(
            store.get("user#old").unwrap(),
            None,
            "expired entries are ignored"
        );
        assert_eq!(store.get("missing").unwrap(), None);

        let users = store.scan_prefix("user#").unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].0, "user#a");

        store
            .update("usage#k", &mut |cur| {
                let n = cur.and_then(|v| v["t"].as_u64()).unwrap_or(0);
                Some((json!({"t": n + 5}), None))
            })
            .unwrap();
        store
            .update("usage#k", &mut |cur| {
                let n = cur.and_then(|v| v["t"].as_u64()).unwrap_or(0);
                Some((json!({"t": n + 5}), None))
            })
            .unwrap();
        assert_eq!(store.get("usage#k").unwrap(), Some(json!({"t": 10})));

        assert_eq!(store.take("session#x").unwrap(), Some(json!({"s": true})));
        assert_eq!(store.take("session#x").unwrap(), None);

        assert_eq!(store.sweep_expired().unwrap(), 1);
        assert!(store.delete("user#b").unwrap());
        assert!(!store.delete("user#b").unwrap());

        store
            .put("cache#1", json!({"route": "/a/*"}), None)
            .unwrap();
        store
            .put("cache#2", json!({"route": "/b/*"}), None)
            .unwrap();
        let n = store
            .delete_where("cache#", &|v| v["route"] == "/a/*")
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.scan_prefix("cache#").unwrap().len(), 1);
    }

    #[test]
    fn memory_store() {
        exercise(&MemoryStore::new());
    }

    #[test]
    fn redb_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open_dir(&dir.path().join("data")).unwrap();
        exercise(&store);
        assert!(dir.path().join("data").join(STORE_FILE).exists());
        store.close().unwrap();
        assert!(store.get("user#a").is_err());
        // the file can be reopened after close
        let again = RedbStore::open_dir(&dir.path().join("data")).unwrap();
        assert_eq!(again.get("user#a").unwrap(), Some(json!({"n": 1})));
    }
}
