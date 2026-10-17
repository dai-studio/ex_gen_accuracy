//! The SvelteKit single-page app, embedded in the binary from `ui/build`.

use crate::error::MoxyError;
use crate::http::Resp;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "ui/build"]
struct Assets;

const PLACEHOLDER: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Moxy</title></head>
<body><h1>Moxy</h1><p>The web UI has not been built. Run <code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code> and rebuild Moxy.
The JSON API is available under <a href="/api/endpoints">/api</a>.</p></body></html>"#;

/// The SPA entry page, served for `/`, `/chat` and `/admin`.
pub fn index() -> Resp {
    match Assets::get("index.html") {
        Some(f) => Resp::bytes(200, "text/html; charset=utf-8", f.data.into_owned())
            .header("cache-control", "no-cache"),
        None => Resp::bytes(200, "text/html; charset=utf-8", PLACEHOLDER),
    }
}

/// A static asset under `/_app/`.
pub fn asset(rel: &str) -> Resp {
    let path = format!("_app/{rel}");
    match Assets::get(&path) {
        Some(f) => {
            let mime = f.metadata.mimetype().to_string();
            let cache = if rel.starts_with("immutable/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            Resp::bytes(200, &mime, f.data.into_owned()).header("cache-control", cache)
        }
        None => MoxyError::not_found(format!("no asset /{path}")).into(),
    }
}
