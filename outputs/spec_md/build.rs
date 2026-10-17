//! Embeds the SvelteKit build output (`ui/build`) into the binary.
//!
//! Run `npm ci && npm run build` in `ui/` (or `make ui`) before `cargo build`.
//! When the build output is missing, a minimal placeholder page is embedded
//! instead so the server still compiles and runs.

use std::fs;
use std::path::{Path, PathBuf};

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Moxy</title></head>
<body><h1>Moxy</h1><p>The web interface was not built. Run <code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code> and rebuild Moxy.</p>
<p>The JSON API is available at <a href="/api/endpoints">/api/endpoints</a>.</p></body></html>
"#;

fn collect(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, base, out);
        } else {
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((format!("/{rel}"), path));
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let build = manifest.join("ui").join("build");
    println!("cargo:rerun-if-changed=ui/build");

    let index = ["200.html", "index.html"]
        .iter()
        .map(|n| build.join(n))
        .find(|p| p.exists());

    let mut code = String::new();
    match index {
        Some(index) => {
            code.push_str(&format!(
                "/// The single-page application shell served for `/`, `/chat` and `/admin`.\npub static INDEX_HTML: &[u8] = include_bytes!({:?});\n",
                index.to_string_lossy()
            ));
        }
        None => {
            println!(
                "cargo:warning=ui/build not found; embedding a placeholder page (run `make ui`)"
            );
            let placeholder = out_dir.join("placeholder.html");
            fs::write(&placeholder, PLACEHOLDER).unwrap();
            code.push_str(&format!(
                "/// The single-page application shell served for `/`, `/chat` and `/admin`.\npub static INDEX_HTML: &[u8] = include_bytes!({:?});\n",
                placeholder.to_string_lossy()
            ));
        }
    }

    let mut assets = Vec::new();
    let app_dir = build.join("_app");
    if app_dir.is_dir() {
        collect(&app_dir, &build, &mut assets);
    }
    assets.sort_by(|a, b| a.0.cmp(&b.0));
    code.push_str("/// Static assets served under `/_app`, sorted by path.\npub static ASSETS: &[(&str, &[u8])] = &[\n");
    for (url, path) in &assets {
        println!("cargo:rerun-if-changed={}", path.display());
        code.push_str(&format!(
            "    ({url:?}, include_bytes!({:?})),\n",
            path.to_string_lossy()
        ));
    }
    code.push_str("];\n");
    fs::write(out_dir.join("ui_assets.rs"), code).unwrap();
}
