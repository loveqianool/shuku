//! Serve the embedded frontend dist (include_dir) with SPA fallback.
//! If `frontend/dist` was absent at build time (no `frontend_dist` cfg),
//! serve a placeholder page instead of failing the build.
//! TODO(main agent): build `frontend/dist` before release builds so the real
//! PWA is embedded.

use crate::api::AppState;
use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

#[cfg(frontend_dist)]
static DIST: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../frontend/dist");

#[allow(dead_code)]
const PLACEHOLDER: &str = r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><title>legado-server</title></head>
<body><h1>legado-server</h1><p>Frontend dist not embedded in this build (placeholder). API is live at <code>/api/health</code>.</p></body></html>"#;

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "webmanifest" => "application/manifest+json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

pub async fn serve(State(_state): State<AppState>, uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") {
        return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({ "error": "unknown api route" }))).into_response();
    }
    #[cfg(frontend_dist)]
    {
        let rel = path.trim_start_matches('/');
        let file = DIST.get_file(rel).or_else(|| {
            // SPA fallback: unknown non-asset path -> index.html
            if !rel.contains('.') {
                DIST.get_file("index.html")
            } else {
                None
            }
        });
        if let Some(f) = file {
            let body = f.contents().to_vec();
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type(f.path().to_str().unwrap_or("index.html")))],
                body,
            )
                .into_response();
        }
        if rel.is_empty() {
            if let Some(f) = DIST.get_file("index.html") {
                return (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    f.contents().to_vec(),
                )
                    .into_response();
            }
        }
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    #[cfg(not(frontend_dist))]
    {
        let _ = path;
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            PLACEHOLDER,
        )
            .into_response()
    }
}
