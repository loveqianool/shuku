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
const PLACEHOLDER: &str = r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><title>shuku</title></head>
<body><h1>shuku</h1><p>Frontend dist not embedded in this build (placeholder). API is live at <code>/api/health</code>.</p></body></html>"#;

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

/// Cache-Control for embedded frontend files.
///
/// The stale-frontend bug (2026-11-12): the backend sent no Cache-Control
/// at all, so Cloudflare (which fronts the site) cached `sw.js` by
/// extension and the browser cached it heuristically. The service worker
/// update check then kept fetching the old `sw.js`, never saw the new
/// precache manifest, and the old frontend stayed forever.
/// - `sw.js`, `index.html`, `manifest.webmanifest`: `no-cache` — must
///   revalidate every time, so a new deploy is picked up on next visit.
/// - `assets/*` (Vite content-hashes filenames, e.g. `index-abc123.js`):
///   `immutable`, safe to cache a year; the name changes when content does.
/// - Everything else (icons, favicon): short cache.
fn cache_control(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name == "sw.js" || name == "index.html" || name == "manifest.webmanifest" || name == "registerSW.js" {
        "no-cache"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
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
            let p = f.path().to_str().unwrap_or("index.html");
            return (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, content_type(p)),
                    (header::CACHE_CONTROL, cache_control(p)),
                ],
                body,
            )
                .into_response();
        }
        if rel.is_empty() {
            if let Some(f) = DIST.get_file("index.html") {
                return (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
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

#[cfg(test)]
mod tests {
    use super::cache_control;

    #[test]
    fn cache_headers() {
        // sw.js / index.html / manifest 每次都要重新验证，否则 SW 更新检查
        // 会拿到缓存的旧版，前端永远不变（2026-11-12 的 bug）。
        assert_eq!(cache_control("sw.js"), "no-cache");
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(cache_control("manifest.webmanifest"), "no-cache");
        assert_eq!(cache_control("registerSW.js"), "no-cache");
        // Vite 带内容 hash 的资源可长缓存，文件名变了内容就变了。
        assert_eq!(
            cache_control("assets/index-3S2z3f1l.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("icons/icon-192.png"), "public, max-age=3600");
    }
}
