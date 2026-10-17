//! Process startup and shutdown behaviour.

mod common;

use common::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn refuses_to_start_with_invalid_or_missing_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("moxy.xml");
    std::fs::write(&cfg, r#"<moxy version="1"><route path="/x/*" /></moxy>"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_moxy"))
        .env("MOXY_CONFIG", &cfg)
        .env("MOXY_DATA_DIR", dir.path().join("data"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no <upstream>"));

    let out = Command::new(env!("CARGO_BIN_EXE_moxy"))
        .env("MOXY_CONFIG", dir.path().join("missing.xml"))
        .env("MOXY_DATA_DIR", dir.path().join("data"))
        .output()
        .unwrap();
    assert!(!out.status.success());

    // Unresolved ${NAME} is a validation error.
    std::fs::write(
        &cfg,
        r#"<moxy version="1"><route path="/x/*"><upstream url="${MOXY_TEST_UNSET_VAR}" /></route></moxy>"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_moxy"))
        .arg("check")
        .env("MOXY_CONFIG", &cfg)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("MOXY_TEST_UNSET_VAR"));
}

#[test]
fn data_dir_is_created_and_sigterm_shuts_down() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("moxy.xml");
    let port = free_port();
    std::fs::write(
        &cfg,
        format!(r#"<moxy version="1"><server port="{port}" /><route path="/x/*"><upstream url="http://127.0.0.1:1" /></route></moxy>"#),
    )
    .unwrap();
    let data = dir.path().join("nested/data");
    let mut child = Command::new(env!("CARGO_BIN_EXE_moxy"))
        .env("MOXY_CONFIG", &cfg)
        .env("MOXY_DATA_DIR", &data)
        .env("MOXY_LOG", "error")
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(start.elapsed() < Duration::from_secs(10), "did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(data.join("moxy.redb").exists());

    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status:?}");
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "idle server shuts down promptly");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn cli_creates_api_keys() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_moxy"))
        .args(["keys", "create", "--name", "ci"])
        .env("MOXY_DATA_DIR", dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(key.starts_with("moxy_"));
    let out = Command::new(env!("CARGO_BIN_EXE_moxy"))
        .args(["keys", "list"])
        .env("MOXY_DATA_DIR", dir.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("\tci\t"));
}
