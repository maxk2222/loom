//! Embedded dashboard assets (server variant build of frontend).

use axum::{
    response::{Html, IntoResponse},
    routing::get,
    Router,
};

#[derive(rust_embed::Embed)]
#[folder = "../../frontend/dist/"]
#[include = "*"]
#[exclude = "*.map"]
struct Dash;

/// Mount at app level (not via `nest`) — an axum nest can let `/ui/` fall
/// through into the admin router's middleware before route resolution.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/ui", get(index))
        .route("/ui/", get(index))
        .route("/ui/{*path}", get(asset))
}

pub async fn index() -> Html<String> {
    Html(serve("index.html"))
}

async fn asset(axum::extract::Path(p): axum::extract::Path<String>) -> axum::response::Response {
    let p = p.trim_start_matches('/');
    match Dash::get(p) {
        Some(c) => axum::response::Response::builder()
            .header("content-type", mime(p))
            .body(axum::body::Body::from(c.data.into_owned()))
            .unwrap(),
        None => index().await.into_response(),
    }
}

fn serve(p: &str) -> String {
    Dash::get(p)
        .map(|c| String::from_utf8_lossy(&c.data).into_owned())
        .unwrap_or_else(|| "<!doctype html><title>LOOM</title><body style='background:#0b0f14;color:#dbe4ee;font-family:system-ui'>Dashboard not built — run `cd frontend && npm run build`</body>".into())
}

fn mime(p: &str) -> &'static str {
    if p.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if p.ends_with(".js") || p.ends_with(".mjs") {
        "text/javascript"
    } else if p.ends_with(".css") {
        "text/css"
    } else if p.ends_with(".svg") {
        "image/svg+xml"
    } else if p.ends_with(".png") {
        "image/png"
    } else if p.ends_with(".woff2") {
        "font/woff2"
    } else if p.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}
