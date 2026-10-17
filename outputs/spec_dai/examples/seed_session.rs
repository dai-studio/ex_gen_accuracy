//! Development helper: create a signed-in browser session without an identity provider.
//!
//! Usage (server stopped, same MOXY_DATA_DIR as the server):
//!     cargo run --example seed_session -- <email> [sub]
//! Prints the session id; set it as the `moxy_session` cookie in the browser.

use moxy::store::{KvStore, RedbStore};
use serde_json::json;

fn main() {
    let mut args = std::env::args().skip(1);
    let email = args.next().expect("usage: seed_session <email> [sub]");
    let sub = args.next().unwrap_or_else(|| format!("dev-{email}"));
    let dir = std::env::var("MOXY_DATA_DIR").unwrap_or_else(|_| "./data".into());
    std::fs::create_dir_all(&dir).expect("create data dir");
    let kv = RedbStore::open(&std::path::Path::new(&dir).join("moxy.redb")).expect("open store");
    let now = chrono::Utc::now();
    kv.put_raw(
        &format!("user#{sub}"),
        &json!({"sub": sub, "email": email, "name": email, "role": "user",
                "createdAt": now, "lastLoginAt": now, "emailVerified": true}),
    )
    .unwrap();
    let id = moxy::util::random_b64(32);
    kv.put_raw(
        &format!("session#{id}"),
        &json!({"id": id, "userSub": sub, "csrfToken": moxy::util::random_b64(32),
                "createdAt": now, "expiresAt": now + chrono::Duration::hours(24)}),
    )
    .unwrap();
    println!("{id}");
}
