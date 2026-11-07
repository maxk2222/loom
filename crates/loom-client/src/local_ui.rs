//! Embedded minimal admin UI (client variant of the shared React app).
//! Built from `frontend/` with VITE_APP_VARIANT=client; embedded via rust-embed.
//! When the assets are missing (dev without frontend build), serves a stub page.

use axum::{
    response::{Html, IntoResponse},
    routing::get,
    Router,
};

#[derive(rust_embed::Embed)]
#[folder = "../../frontend/dist-client/"]
#[include = "*"]
#[exclude = "*.map"]
struct ClientUiAssets;

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/", get(index))
        .route("/{*path}", get(asset))
        .fallback(get(index))
}

async fn index() -> Html<String> {
    Html(serve_asset("index.html"))
}

async fn asset(axum::extract::Path(path): axum::extract::Path<String>) -> axum::response::Response {
    let p = path.trim_start_matches('/');
    if p.is_empty() {
        return index().await.into_response();
    }
    match ClientUiAssets::get(p) {
        Some(content) => axum::response::Response::builder()
            .header("content-type", mime_from_path(p))
            .body(axum::body::Body::from(content.data.into_owned()))
            .unwrap(),
        // SPA fallback: serve index.html for client-side routes.
        None => index().await.into_response(),
    }
}

fn serve_asset(path: &str) -> String {
    match ClientUiAssets::get(path) {
        Some(c) => String::from_utf8_lossy(&c.data).into_owned(),
        None => STUB.to_string(),
    }
}

fn mime_from_path(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "text/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

const STUB: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>LOOM Client</title>
<style>body{background:#0b0f14;color:#dbe4ee;font-family:system-ui;margin:3rem}</style></head>
<body><h1>LOOM Client</h1><p>Embedded UI assets not built yet. Build the client variant:
<code>cd frontend && VITE_APP_VARIANT=client npm run build</code></p></body></html>"#;
