//! The SvelteKit single-page application, embedded into the binary at build time.

include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

/// Look up an embedded asset by its URL path (for example `/_app/immutable/...`).
pub fn asset(path: &str) -> Option<&'static [u8]> {
    ASSETS
        .binary_search_by(|(p, _)| (*p).cmp(path))
        .ok()
        .map(|i| ASSETS[i].1)
}

pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_embedded() {
        assert!(std::str::from_utf8(INDEX_HTML).unwrap().contains("<html"));
        assert!(
            ASSETS.windows(2).all(|w| w[0].0 < w[1].0),
            "assets are sorted"
        );
        for (path, _) in ASSETS {
            assert!(path.starts_with("/_app/"));
            assert!(asset(path).is_some());
        }
        assert!(asset("/_app/nope.js").is_none());
        assert_eq!(content_type("/_app/x.js"), "text/javascript; charset=utf-8");
    }
}
