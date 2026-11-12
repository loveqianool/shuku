//! Axum REST API (see docs/API.md). All handlers are thin: heavy work
//! (HTML parse, JS, tantivy, file parse) runs in spawn_blocking upstream.

use crate::chapter_check::ChapterIssue;
use crate::config::Config;
use crate::db::{BookRow, Database};
use crate::parser;
use crate::scheduler::Scheduler;
use crate::search::SearchIndex;
use crate::source::{Engine, SourceDef};
use anyhow::{anyhow, Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub search: Arc<SearchIndex>,
    pub engine: Arc<Engine>,
    pub scheduler: Arc<Scheduler>,
    pub config: Config,
}

pub struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": self.0.to_string() });
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(e)
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(e: tokio::task::JoinError) -> Self {
        ApiError(anyhow!("blocking task failed: {e}"))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError(anyhow!("json: {e}"))
    }
}

pub(crate) type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, Deserialize, Default)]
pub struct PageQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

impl PageQuery {
    fn page(&self) -> i64 {
        // Upper bound so (page - 1) * page_size can never overflow i64
        // (page_size is clamped to 500; 10000 * 500 = 5,000,000).
        self.page.unwrap_or(1).clamp(1, 10000)
    }
    fn page_size(&self) -> i64 {
        self.page_size.unwrap_or(20).clamp(1, 500)
    }
}

fn page_envelope<T: Serialize>(items: Vec<T>, total: i64, page: i64, page_size: i64) -> Json<Value> {
    Json(json!({ "items": items, "total": total, "page": page, "page_size": page_size }))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/books", get(list_books))
        .route("/api/books/duplicates", get(list_duplicates))
        .route("/api/books/recent", get(list_recent_books))
        .route("/api/books/{id}", get(get_book).delete(delete_book))
        .route("/api/books/{id}/chapters", get(list_chapters))
        .route("/api/books/{id}/chapters/{idx}", get(get_chapter))
        .route("/api/books/{id}/matches", get(list_book_matches))
        .route("/api/books/{id}/images/{*path}", get(get_book_image))
        .route("/api/books/{id}/cover", get(get_book_cover))
        .route("/api/search", get(search))
        .route("/api/suggest", get(suggest))
        .route("/api/sources", get(list_sources).post(create_source))
        .route("/api/sources/import", post(import_sources))
        .route("/api/sources/export", get(export_sources))
        .route("/api/sources/{id}", get(get_source).put(update_source).delete(delete_source))
        .route("/api/sources/{id}/test", post(test_source))
        .route("/api/sources/{id}/health-check", post(trigger_health_check))
        .route("/api/sources/subscribe", post(subscribe_sources))
        .route("/api/picker/fetch", post(picker_fetch))
        .route("/api/books/{id}/progress", get(get_progress).put(put_progress))
        .route("/api/books/{id}/reading-time", post(add_reading_time))
        .route("/api/stats", get(reading_stats))
        .route("/api/books/{id}/bookmarks", get(list_bookmarks).post(add_bookmark))
        .route("/api/books/{id}/bookmarks/{bid}", delete(delete_bookmark))
        .route("/api/books/{id}/tags", get(list_tags).post(add_tag))
        .route("/api/books/{id}/tags/{tag}", delete(delete_tag))
        .route("/api/tags", get(all_tags))
        .route("/api/tasks", get(list_tasks))
        .route("/api/tasks/stats", get(task_stats))
        .route("/api/tasks/scan", post(trigger_scan))
        .route("/api/tasks/chapter-check", post(trigger_chapter_check))
        .route("/api/books/chapter-issues", get(list_books_with_chapter_issues))
        .route("/api/books/{id}/chapter-issues", get(check_book_chapters))
        .route("/api/tasks/crawl", post(trigger_crawl))
        .route("/api/tasks/rebuild-index", post(trigger_rebuild_index))
        .route("/api/tasks/check-updates", post(trigger_check_updates))
        .route("/api/books/{id}/check-update", post(trigger_check_update))
        .route("/api/books/{id}/change-source", post(trigger_change_source))
        .route("/api/tasks/{id}/retry", post(retry_task))
        .route("/api/tasks/{id}/pause", post(pause_task))
        .route("/api/tasks/{id}/resume", post(resume_task))
        .merge(crate::opds::router())
        .fallback(crate::static_files::serve)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        // API responses must not be cached by browsers or the CDN in front
        // of the service (stale JSON / progress / task state is a real bug
        // source). `if_not_present` leaves the static-file handler's own
        // Cache-Control (sw.js no-cache, hashed assets immutable) untouched.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

// ---------------------------------------------------------------------------
// Books
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BookListQuery {
    pub q: Option<String>,
    pub author: Option<String>,
    pub tag: Option<String>,
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

impl BookListQuery {
    fn pq(&self) -> PageQuery {
        PageQuery { page: self.page, page_size: self.page_size }
    }
}

async fn list_books(State(s): State<AppState>, Query(q): Query<BookListQuery>) -> ApiResult<Json<Value>> {
    let (page, page_size) = (q.pq().page(), q.pq().page_size());
    let (items, total) = s
        .db
        .list_books(q.q, q.author, q.tag, q.sort.as_deref().unwrap_or("updated"), page, page_size)
        .await?;
    Ok(page_envelope(items, total, page, page_size))
}

/// 最近阅读：最近打开的 100 本书（按阅读进度的更新时间倒序）。
async fn list_recent_books(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let items = s.db.recent_books(100).await?;
    Ok(Json(json!({ "items": items })))
}

async fn get_book(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let book = s.db.get_book(&id).await?.ok_or_else(|| anyhow!("book not found: {id}"))?;
    let tags = s.db.list_tags(&id).await.unwrap_or_default();
    let progress = s.db.get_progress(&id).await.ok().flatten();
    Ok(Json(json!({
        "id": book.id, "title": book.title, "author": book.author,
        "path": book.path, "source_url": book.source_url, "source": book.source,
        "status": book.status, "chapter_count": book.chapter_count,
        "has_update": book.has_update, "last_update_check": book.last_update_check,
        "cover": book.cover,
        "mtime": book.mtime, "size": book.size, "index_version": book.index_version,
        "created_at": book.created_at, "updated_at": book.updated_at,
        "tags": tags,
        "progress": progress.map(|(c, o)| json!({"chapter_idx": c, "offset": o})),
    })))
}

async fn list_duplicates(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let groups = s.db.duplicate_groups().await?;
    Ok(Json(json!(groups)))
}

async fn delete_book(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let _book = s.db.get_book(&id).await?.ok_or_else(|| anyhow!("book not found: {id}"))?;
    let deleted = s.db.delete_book(&id).await?;
    if deleted {
        // Index + files are best-effort: the DB row is already gone, so
        // a stale index doc or leftover file is harmless and gets
        // rebuilt/cleaned on the next scan. Failures are logged (not
        // swallowed) so a broken index dir or permission problem is visible.
        let id_num: u64 = id.parse().unwrap_or(0);
        let search = s.search.clone();
        // spawn_blocking gives Result<Result<()>, JoinError>: flatten both
        // layers so an index failure is logged, not silently dropped.
        let idx_res = tokio::task::spawn_blocking(move || search.delete_book(id_num))
            .await
            .map_err(|e| anyhow::anyhow!("join: {e}"))
            .and_then(|r| r);
        if let Err(e) = idx_res {
            tracing::warn!(book_id = %id, error = %e, "delete_book: index cleanup failed");
        }
        if let Err(e) = std::fs::remove_dir_all(s.config.data_dir.join("chapters").join(&id)) {
            tracing::warn!(book_id = %id, error = %e, "delete_book: chapter files cleanup failed");
        }
        if let Err(e) = std::fs::remove_file(s.config.data_dir.join("covers").join(&id)) {
            tracing::warn!(book_id = %id, error = %e, "delete_book: cover cleanup failed");
        }
    }
    Ok(Json(json!({ "deleted": deleted })))
}

async fn list_chapters(State(s): State<AppState>, Path(id): Path<String>, Query(q): Query<PageQuery>) -> ApiResult<Json<Value>> {
    let (items, total) = s.db.list_chapters(&id, q.page(), q.page_size()).await?;
    // Opening the chapter list means the user has seen any new chapters.
    if s.db.get_book(&id).await?.map(|b| b.has_update).unwrap_or(false) {
        let _ = s.db.set_has_update(&id, false).await;
    }
    Ok(page_envelope(items, total, q.page(), q.page_size()))
}

async fn get_chapter(State(s): State<AppState>, Path((id, idx)): Path<(String, i64)>) -> ApiResult<Json<Value>> {
    let book: BookRow = s.db.get_book(&id).await?.ok_or_else(|| anyhow!("book not found: {id}"))?;
    let title = s.db.get_chapter_title(&id, idx).await?.unwrap_or_default();
    let img_base = format!("/api/books/{id}/images/");
    // Raw chapter text first; the regex typesetting below is CPU-heavy and
    // must not run on the async runtime thread (it used to run inline here
    // for cached books, blocking the event loop for the whole chapter).
    let (text, reflow): (String, bool) = if book.source == "cached" {
        // Cached network book: per-chapter text file.
        let f = s.config.chapter_file(&id, idx as usize);
        let text = tokio::fs::read_to_string(&f).await.unwrap_or_default();
        // 缓存的网络书是纯文本（可能带硬换行），做断行合并。
        (text, true)
    } else {
        let path = book.path.clone().ok_or_else(|| anyhow!("book has no local path"))?;
        let path = std::path::PathBuf::from(path);
        // epub 行本身就是 HTML block，不做断行合并；txt 才合并硬换行。
        // 注意：要在 move 进 spawn_blocking 之前算好，path 移进去后不能再用。
        let reflow = path.extension().and_then(|e| e.to_str()) != Some("epub");
        let idx_usize = idx as usize;
        let chapters = tokio::task::spawn_blocking(move || parser::parse_book_file(&path)).await??;
        let ch = chapters.chapters.get(idx_usize).ok_or_else(|| anyhow!("chapter {idx} out of range"))?;
        (ch.text.clone(), reflow)
    };
    let paras: Vec<crate::source::Paragraph> =
        tokio::task::spawn_blocking(move || parser::paragraphs_from_text(&text, &img_base, reflow)).await?;
    Ok(Json(json!({
        "book_id": id, "chapter_idx": idx, "title": title, "paragraphs": paras,
    })))
}

/// Serve an image embedded in an epub: `GET /api/books/{id}/images/*path`,
/// where `path` is the image's path inside the epub (as produced by
/// `parser::paragraphs_from_text`). Only epub books, and `..` is rejected.
async fn get_book_image(
    State(s): State<AppState>,
    Path((id, path)): Path<(String, String)>,
) -> ApiResult<Response> {
    let path = parser::percent_decode(&path);
    if path.split('/').any(|seg| seg == ".." || seg == ".") || path.starts_with('/') {
        return Err(ApiError(anyhow!("invalid image path: {path}")));
    }
    let book: BookRow = s.db.get_book(&id).await?.ok_or_else(|| anyhow!("book not found: {id}"))?;
    let epub_path = book.path.clone().ok_or_else(|| anyhow!("book has no local path"))?;
    if !epub_path.to_ascii_lowercase().ends_with(".epub") {
        return Err(ApiError(anyhow!("book is not an epub")));
    }
    let (bytes, mime) = tokio::task::spawn_blocking(move || -> Result<(Vec<u8>, String)> {
        let mut doc = epub::doc::EpubDoc::new(&epub_path)
            .with_context(|| format!("open epub {epub_path}"))?;
        let bytes = doc
            .get_resource_by_path(&path)
            .ok_or_else(|| anyhow!("image not found in epub: {path}"))?;
        let mime = doc.get_resource_mime_by_path(&path).unwrap_or_else(|| "application/octet-stream".to_string());
        Ok((bytes, mime))
    })
    .await??;
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::header::HeaderValue::from_str(&mime).unwrap_or_else(|_| axum::http::header::HeaderValue::from_static("application/octet-stream")),
    );
    Ok(resp)
}

/// Serve a scanned epub's cover: `GET /api/books/{id}/cover`. Bytes live
/// in data/covers/<id>; the row only stores the MIME.
async fn get_book_cover(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let book: BookRow = s.db.get_book(&id).await?.ok_or_else(|| anyhow!("book not found: {id}"))?;
    if book.cover.is_empty() {
        return Err(ApiError(anyhow!("book has no cover")));
    }
    let path = s.config.data_dir.join("covers").join(&id);
    let bytes = tokio::fs::read(&path).await.map_err(|_| ApiError(anyhow!("cover file missing for book {id}")))?;
    let mut resp = Response::new(axum::body::Body::from(bytes));
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::header::HeaderValue::from_str(&book.cover).unwrap_or_else(|_| axum::http::header::HeaderValue::from_static("application/octet-stream")),
    );
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Search (tantivy only — never SQLite LIKE)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub scope: Option<String>,
    /// Restrict results to one book (takes precedence over `scope`).
    pub book_id: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

impl SearchQuery {
    fn pq(&self) -> PageQuery {
        PageQuery { page: self.page, page_size: self.page_size }
    }
}

async fn search(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> ApiResult<Json<Value>> {
    if q.q.trim().is_empty() {
        return Ok(page_envelope(Vec::<Value>::new(), 0, q.pq().page(), q.pq().page_size()));
    }
    let scope = q.scope.as_deref().unwrap_or("all");
    let ids: Option<Vec<u64>> = match &q.book_id {
        Some(bid) => Some(bid.parse::<u64>().map(|n| vec![n]).unwrap_or_default()),
        None => match scope {
            "local" => Some(s.db.book_ids_by_source("local").await?),
            "cached" => Some(s.db.book_ids_by_source("cached").await?),
            _ => None,
        },
    };
    let limit = q.pq().page_size() as usize;
    let offset = ((q.pq().page() - 1) * q.pq().page_size()) as usize;
    let query = q.q.clone();
    let search = s.search.clone();
    let (hits, total) = tokio::task::spawn_blocking(move || search.search(&query, ids.as_deref(), limit, offset)).await??;
    Ok(page_envelope(hits, total as i64, q.pq().page(), q.pq().page_size()))
}

/// Flat in-book match list: every literal occurrence of the query in
/// one book, in document order (chapter, then offset), for the reader's
/// numbered match list (tap one to jump to that exact spot).
async fn list_book_matches(State(s): State<AppState>, Path(id): Path<String>, Query(q): Query<SearchQuery>) -> ApiResult<Json<Value>> {
    let term = q.q.trim().to_string();
    if term.is_empty() {
        return Ok(page_envelope(Vec::<Value>::new(), 0, q.pq().page(), q.pq().page_size()));
    }
    let book_id: u64 = match id.parse() {
        Ok(n) => n,
        Err(_) => return Ok(page_envelope(Vec::<Value>::new(), 0, q.pq().page(), q.pq().page_size())),
    };
    let limit = q.pq().page_size() as usize;
    let offset = ((q.pq().page() - 1) * q.pq().page_size()) as usize;
    let search = s.search.clone();
    let (items, total) = tokio::task::spawn_blocking(move || search.list_matches(book_id, &term, limit, offset)).await??;
    Ok(page_envelope(items, total as i64, q.pq().page(), q.pq().page_size()))
}

/// Search suggestions for the search box dropdown: title/author
/// substring matches first, then top up from the full-text index
/// (which also covers pinyin queries like `xiuxian`).
async fn suggest(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> ApiResult<Json<Value>> {
    let term = q.q.trim().to_string();
    if term.is_empty() {
        return Ok(Json(json!({ "items": [] })));
    }
    let mut items = s.db.suggest_books(&term, 8).await?;
    if items.len() < 8 {
        // Full-text top-up: dedupe by book, keep SQLite order first.
        let query = term.clone();
        let search = s.search.clone();
        if let Ok(Ok((hits, _))) =
            tokio::task::spawn_blocking(move || search.search(&query, None, 20, 0)).await
        {
            for h in hits {
                if items.len() >= 8 {
                    break;
                }
                if items.iter().any(|b: &BookRow| b.id == h.book_id) {
                    continue;
                }
                if let Ok(Some(b)) = s.db.get_book(&h.book_id).await {
                    items.push(b);
                }
            }
        }
    }
    Ok(Json(json!({
        "items": items.iter().map(|b| json!({
            "id": b.id, "title": b.title, "author": b.author,
        })).collect::<Vec<_>>(),
    })))
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

async fn list_sources(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let mut items = s.db.list_sources().await?;
    for item in &mut items {
        if let Some(id) = item.get("bookSourceUrl").and_then(|v| v.as_str()).map(|x| x.to_string()) {
            if let Some((health, error, checked)) = s.db.source_health(&id).await? {
                item["health"] = json!(health);
                item["healthError"] = json!(error);
                item["lastCheckTime"] = json!(checked);
            }
            if let Some((sub_url, _)) = s.db.get_source_subscription(&id).await? {
                if !sub_url.is_empty() {
                    item["subscriptionUrl"] = json!(sub_url);
                }
            }
        }
    }
    let total = items.len();
    Ok(Json(json!({ "items": items, "total": total })))
}

#[derive(Debug, Deserialize)]
pub struct SubscribeReq {
    pub url: String,
}

/// Queue a rule-subscription import/refresh from a JSON URL (array of
/// sources, or a single source object).
async fn subscribe_sources(State(s): State<AppState>, Json(req): Json<SubscribeReq>) -> ApiResult<Json<Value>> {
    let url = req.url.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(anyhow!("subscription url must be http(s)").into());
    }
    let payload = serde_json::json!({ "url": url }).to_string();
    let task_id = s.db.enqueue_task("crawl_tasks", "refresh_subscription", &payload).await?;
    Ok(Json(json!({ "id": task_id, "status": "pending" })))
}

/// Queue a health probe for one source.
async fn trigger_health_check(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    if s.db.get_source_raw(&id).await?.is_none() {
        return Err(anyhow!("source not found: {id}").into());
    }
    let payload = serde_json::json!({ "source_id": id }).to_string();
    let task_id = s.db.enqueue_task("crawl_tasks", "health_check", &payload).await?;
    Ok(Json(json!({ "id": task_id, "status": "pending" })))
}

async fn create_source(State(s): State<AppState>, Json(raw): Json<Value>) -> ApiResult<Json<Value>> {
    crate::source::validate_source(&raw).map_err(|e| anyhow!("invalid source: {e}"))?;
    let id = s.db.upsert_source(&raw).await?;
    Ok(Json(json!({ "id": id })))
}

async fn get_source(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let raw = s.db.get_source_raw(&id).await?.ok_or_else(|| anyhow!("source not found: {id}"))?;
    let mut v = serde_json::from_str::<Value>(&raw)?;
    if let Some((health, error, checked)) = s.db.source_health(&id).await? {
        v["health"] = json!(health);
        v["healthError"] = json!(error);
        v["lastCheckTime"] = json!(checked);
    }
    if let Some((sub_url, _)) = s.db.get_source_subscription(&id).await? {
        if !sub_url.is_empty() {
            v["subscriptionUrl"] = json!(sub_url);
        }
    }
    Ok(Json(v))
}

async fn update_source(State(s): State<AppState>, Path(id): Path<String>, Json(raw): Json<Value>) -> ApiResult<Json<Value>> {
    crate::source::validate_source(&raw).map_err(|e| anyhow!("invalid source: {e}"))?;
    let mut raw = raw;
    // The path id and the body's bookSourceUrl must agree: silently
    // upserting under the body's id would update a *different* source
    // than the URL names (or create a new one under an unrelated id).
    match raw.get("bookSourceUrl").and_then(|v| v.as_str()) {
        Some(u) if !u.is_empty() && u != id => {
            return Err(anyhow!("path id {id} does not match bookSourceUrl {u}").into());
        }
        Some(u) if u.is_empty() => {
            raw["bookSourceUrl"] = json!(id);
        }
        _ => {}
    }
    let saved = s.db.upsert_source(&raw).await?;
    Ok(Json(json!({ "id": saved })))
}

async fn delete_source(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let ok = s.db.delete_source(&id).await?;
    if !ok {
        return Err(anyhow!("source not found: {id}").into());
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub struct ImportReq {
    pub sources: Vec<Value>,
}

async fn import_sources(State(s): State<AppState>, Json(req): Json<ImportReq>) -> ApiResult<Json<Value>> {
    let mut imported = 0;
    let mut errors = vec![];
    for raw in req.sources {
        match crate::source::validate_source(&raw) {
            Ok(()) => match s.db.upsert_source(&raw).await {
                Ok(_) => imported += 1,
                Err(e) => errors.push(e.to_string()),
            },
            Err(e) => errors.push(e.to_string()),
        }
    }
    Ok(Json(json!({ "imported": imported, "errors": errors })))
}

async fn export_sources(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let items = s.db.list_sources().await?;
    Ok(Json(json!(items)))
}

#[derive(Debug, Deserialize)]
pub struct TestReq {
    pub kind: String,
    pub keyword: Option<String>,
    pub book_url: Option<String>,
    pub chapter_url: Option<String>,
    pub force_refresh: Option<bool>,
    /// Search start page (default 1).
    pub page: Option<i64>,
    /// Search pages to fetch, 1..=10 (default 1). Stops early on an empty page.
    pub max_pages: Option<i64>,
}

async fn test_source(State(s): State<AppState>, Path(id): Path<String>, Json(req): Json<TestReq>) -> ApiResult<Json<Value>> {
    let raw = s.db.get_source_raw(&id).await?.ok_or_else(|| anyhow!("source not found: {id}"))?;
    let raw_json: Value = serde_json::from_str(&raw)?;
    let source = SourceDef::from_raw(&raw_json);
    let force = req.force_refresh.unwrap_or(false);
    // High priority: executed directly, never queued behind crawl tasks.
    let res = match req.kind.as_str() {
        "search" => s.engine.run_search_pages(&source, req.keyword.as_deref().unwrap_or(""), req.page.unwrap_or(1), req.max_pages.unwrap_or(1), force).await,
        "explore" => s.engine.run_explore(&source, req.page.unwrap_or(1), force).await,
        "toc" => s.engine.run_toc(&source, req.book_url.as_deref().unwrap_or(""), force).await,
        "content" => s.engine.run_content(&source, req.chapter_url.as_deref().unwrap_or(""), force).await,
        "login" => s.engine.run_login(&source, force).await,
        other => return Err(anyhow!("unknown test kind: {other}").into()),
    };
    Ok(Json(serde_json::to_value(res)?))
}

// ---------------------------------------------------------------------------
// Rule picker: fetch one page raw for the in-app element picker
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PickerFetchReq {
    pub url: String,
    pub keyword: Option<String>,
    pub headers: Option<std::collections::HashMap<String, String>>,
}

async fn picker_fetch(State(s): State<AppState>, Json(req): Json<PickerFetchReq>) -> ApiResult<Json<Value>> {
    let t0 = std::time::Instant::now();
    let (status, body) = s
        .engine
        .fetch_picker(&req.url, req.keyword.as_deref().unwrap_or(""), req.headers.as_ref())
        .await?;
    Ok(Json(json!({ "ok": true, "status": status, "body": body, "elapsed_ms": t0.elapsed().as_millis() })))
}

// ---------------------------------------------------------------------------
// Progress / bookmarks / tags
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ProgressReq {
    pub chapter_idx: i64,
    pub offset: i64,
}

async fn get_progress(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let p = s.db.get_progress(&id).await?;
    Ok(Json(match p {
        Some((c, o)) => json!({ "book_id": id, "chapter_idx": c, "offset": o }),
        None => json!({ "book_id": id, "chapter_idx": 0, "offset": 0 }),
    }))
}

async fn put_progress(State(s): State<AppState>, Path(id): Path<String>, Json(req): Json<ProgressReq>) -> ApiResult<Json<Value>> {
    s.db.put_progress(&id, req.chapter_idx, req.offset).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub struct ReadingTimeReq {
    pub seconds: i64,
}

async fn add_reading_time(State(s): State<AppState>, Path(id): Path<String>, Json(req): Json<ReadingTimeReq>) -> ApiResult<Json<Value>> {
    // Clamp a single report to one hour: a stuck tab must not inflate stats.
    let seconds = req.seconds.clamp(0, 3600);
    s.db.add_reading_seconds(&id, seconds).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn reading_stats(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let stats = s.db.reading_stats().await?;
    Ok(Json(json!(stats)))
}

#[derive(Debug, Deserialize)]
pub struct BookmarkReq {
    pub chapter_idx: i64,
    pub offset: i64,
    pub note: Option<String>,
}

async fn list_bookmarks(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let items = s.db.list_bookmarks(&id).await?;
    Ok(Json(json!({ "items": items, "total": items.len() })))
}

async fn add_bookmark(State(s): State<AppState>, Path(id): Path<String>, Json(req): Json<BookmarkReq>) -> ApiResult<Json<Value>> {
    let bid = s.db.add_bookmark(&id, req.chapter_idx, req.offset, req.note.as_deref().unwrap_or("")).await?;
    Ok(Json(json!({ "id": bid })))
}

async fn delete_bookmark(State(s): State<AppState>, Path((id, bid)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let ok = s.db.delete_bookmark(&id, &bid).await?;
    Ok(Json(json!({ "ok": ok })))
}

#[derive(Debug, Deserialize)]
pub struct TagReq {
    pub tag: String,
}

async fn list_tags(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let items = s.db.list_tags(&id).await?;
    Ok(Json(json!({ "items": items, "total": items.len() })))
}

async fn add_tag(State(s): State<AppState>, Path(id): Path<String>, Json(req): Json<TagReq>) -> ApiResult<Json<Value>> {
    s.db.add_tag(&id, &req.tag).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn delete_tag(State(s): State<AppState>, Path((id, tag)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let ok = s.db.delete_tag(&id, &tag).await?;
    Ok(Json(json!({ "ok": ok })))
}

async fn all_tags(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let items = s.db.all_tags().await?;
    Ok(Json(json!({ "items": items, "total": items.len() })))
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TaskListQuery {
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    pub status: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

impl TaskListQuery {
    fn pq(&self) -> PageQuery {
        PageQuery { page: self.page, page_size: self.page_size }
    }
}

async fn list_tasks(State(s): State<AppState>, Query(q): Query<TaskListQuery>) -> ApiResult<Json<Value>> {
    let (items, total) = match q.r#type.as_deref() {
        Some("scan") => s.db.list_tasks("scan_tasks", q.status.clone(), q.pq().page(), q.pq().page_size()).await?,
        Some("crawl") => s.db.list_tasks("crawl_tasks", q.status.clone(), q.pq().page(), q.pq().page_size()).await?,
        // No type: one UNION ALL query, ordered by created_at and paginated
        // once (db::list_all_tasks). Querying each table with the same page
        // and concatenating returned up to 2*page_size rows on page 1 and
        // each table's own page 2 afterwards.
        _ => s.db.list_all_tasks(q.status.clone(), q.pq().page(), q.pq().page_size()).await?,
    };
    Ok(page_envelope(items, total, q.pq().page(), q.pq().page_size()))
}

#[derive(Debug, Deserialize)]
pub struct ScanReq {
    pub path: Option<String>,
}

async fn trigger_scan(State(s): State<AppState>, body: Option<Json<ScanReq>>) -> ApiResult<Json<Value>> {
    let req = body.map(|Json(r)| r).unwrap_or(ScanReq { path: None });
    // Containment: a scan path must live inside the configured books dir.
    // Without this, POST /api/tasks/scan with an arbitrary path would make
    // the scanner read (and index) any txt/epub file on the host.
    if let Some(p) = &req.path {
        let canon = std::fs::canonicalize(p)
            .map_err(|e| anyhow!("scan path not found or not readable: {p} ({e})"))?;
        let base = std::fs::canonicalize(&s.config.books_dir)
            .map_err(|e| anyhow!("books dir not readable: {} ({e})", s.config.books_dir.display()))?;
        if !canon.starts_with(&base) {
            return Err(anyhow!("scan path must be inside the books dir: {p}").into());
        }
    }
    let payload = serde_json::json!({ "path": req.path }).to_string();
    let id = s.db.enqueue_task("scan_tasks", "scan", &payload).await?;
    Ok(Json(json!({ "id": id, "status": "pending" })))
}

async fn trigger_chapter_check(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let id = s.db.enqueue_task("scan_tasks", "chapter_check", "{}").await?;
    Ok(Json(json!({ "id": id, "status": "pending" })))
}

async fn list_books_with_chapter_issues(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let books = s.db.books_with_chapter_issues().await?;
    Ok(Json(json!({
        "books": books.iter().map(|(id, title, author, n)| json!({
            "id": id, "title": title, "author": author, "issues": n,
        })).collect::<Vec<_>>(),
    })))
}

/// Live per-book check: title-number gaps + empty chapters. The result
/// is stored (so the library-wide list stays current for this book).
async fn check_book_chapters(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let book = s.db.get_book(&id).await?.ok_or_else(|| anyhow!("book not found: {id}"))?;
    let titles = s.db.list_chapter_titles(&id).await?;
    let mut issues = crate::chapter_check::detect_gaps(&titles);
    if book.source == "cached" {
        for (idx, title) in &titles {
            let f = s.config.chapter_file(&id, *idx as usize);
            let empty = match tokio::fs::metadata(&f).await {
                Ok(m) => m.len() == 0,
                Err(_) => true, // missing file = missing chapter
            };
            if empty {
                issues.push(ChapterIssue {
                    idx: *idx,
                    title: title.clone(),
                    kind: "empty".into(),
                    detail: "章节文件为空或缺失（抓取失败）".into(),
                });
            }
        }
    } else if let Some(path) = book.path.clone() {
        // Local book: parse once, flag chapters with no body text.
        let parsed = tokio::task::spawn_blocking(move || {
            crate::parser::parse_book_file(std::path::Path::new(&path))
        })
        .await??;
        for (i, ch) in parsed.chapters.iter().enumerate() {
            if ch.text.trim().is_empty() {
                let title = titles.get(i).map(|(_, t)| t.clone()).unwrap_or_default();
                issues.push(ChapterIssue {
                    idx: i as i64,
                    title,
                    kind: "empty".into(),
                    detail: "本章没有正文".into(),
                });
            }
        }
    }
    issues.sort_by_key(|i| (i.idx, i.kind.clone()));
    s.db.replace_chapter_issues(&id, &issues).await?;
    Ok(Json(json!({ "issues": issues })))
}

#[derive(Debug, Deserialize)]
pub struct CrawlReq {
    pub source_id: String,
    pub book_url: String,
    pub title: Option<String>,
    pub author: Option<String>,
}

async fn trigger_crawl(State(s): State<AppState>, Json(req): Json<CrawlReq>) -> ApiResult<Json<Value>> {
    let payload = serde_json::json!({
        "source_id": req.source_id, "book_url": req.book_url,
        "title": req.title.unwrap_or_default(), "author": req.author.unwrap_or_default(),
    })
    .to_string();
    let id = s.db.enqueue_task("crawl_tasks", "crawl", &payload).await?;
    Ok(Json(json!({ "id": id, "status": "pending" })))
}

#[derive(Debug, Deserialize)]
pub struct RebuildReq {
    pub pause_crawl: Option<bool>,
}

/// Queue update checks for all due cached books right now (same round the
/// periodic worker runs); returns how many were queued.
async fn trigger_check_updates(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let n = crate::scheduler::enqueue_due_updates(&s.scheduler, s.config.update_check_interval_secs as i64, 50).await?;
    Ok(Json(json!({ "queued": n })))
}

/// Queue an update check for one cached book.
async fn trigger_check_update(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let book = s.db.get_book(&id).await?.ok_or_else(|| anyhow!("book not found: {id}"))?;
    if book.source != "cached" {
        return Err(anyhow!("only cached books can be checked for updates").into());
    }
    let payload = serde_json::json!({ "book_id": id }).to_string();
    let task_id = s.db.enqueue_task("crawl_tasks", "check_update", &payload).await?;
    s.db.mark_update_checked(&id).await?;
    Ok(Json(json!({ "id": task_id, "status": "pending" })))
}

#[derive(Debug, Deserialize)]
pub struct ChangeSourceReq {
    pub source_id: String,
    pub book_url: Option<String>,
}

/// Queue a source switch for a cached book. Without book_url the worker
/// searches the new source by title and takes the first hit.
async fn trigger_change_source(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ChangeSourceReq>,
) -> ApiResult<Json<Value>> {
    let book = s.db.get_book(&id).await?.ok_or_else(|| anyhow!("book not found: {id}"))?;
    if book.source != "cached" {
        return Err(anyhow!("only cached books can change source").into());
    }
    if s.db.get_source_raw(&req.source_id).await?.is_none() {
        return Err(anyhow!("source not found: {}", req.source_id).into());
    }
    let payload = serde_json::json!({
        "book_id": id, "source_id": req.source_id, "book_url": req.book_url.unwrap_or_default(),
    })
    .to_string();
    let task_id = s.db.enqueue_task("crawl_tasks", "change_source", &payload).await?;
    Ok(Json(json!({ "id": task_id, "status": "pending" })))
}

async fn trigger_rebuild_index(State(s): State<AppState>, body: Option<Json<RebuildReq>>) -> ApiResult<Json<Value>> {
    let req = body.map(|Json(r)| r).unwrap_or(RebuildReq { pause_crawl: None });
    let pause = req.pause_crawl.unwrap_or(true);
    let payload = serde_json::json!({ "pause_crawl": pause }).to_string();
    let id = s.db.enqueue_task("scan_tasks", "rebuild_index", &payload).await?;
    Ok(Json(json!({ "id": id, "status": "pending", "pause_crawl": pause })))
}

async fn find_task_table(s: &AppState, id: &str) -> ApiResult<Option<&'static str>> {
    for table in ["scan_tasks", "crawl_tasks"] {
        let id_owned = id.to_string();
        let found: bool = s
            .db
            .call_db(move |c| {
                let n: i64 = c.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE id=?1"),
                    rusqlite::params![id_owned],
                    |r| r.get(0),
                )?;
                Ok(n > 0)
            })
            .await?;
        if found {
            return Ok(Some(if table == "scan_tasks" { "scan_tasks" } else { "crawl_tasks" }));
        }
    }
    Ok(None)
}

async fn retry_task(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let table = find_task_table(&s, &id).await?.ok_or_else(|| anyhow!("task not found: {id}"))?;
    s.db.set_task_status(table, &id, "pending").await?;
    Ok(Json(json!({ "ok": true })))
}

async fn pause_task(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let table = find_task_table(&s, &id).await?.ok_or_else(|| anyhow!("task not found: {id}"))?;
    s.db.set_task_status(table, &id, "paused").await?;
    Ok(Json(json!({ "ok": true })))
}

async fn resume_task(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let table = find_task_table(&s, &id).await?.ok_or_else(|| anyhow!("task not found: {id}"))?;
    s.db.set_task_status(table, &id, "pending").await?;
    Ok(Json(json!({ "ok": true })))
}

async fn task_stats(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let scan = s.db.task_counts("scan_tasks").await?;
    let crawl = s.db.task_counts("crawl_tasks").await?;
    let (cache_rows, cache_bytes) = s.db.cache_stats().await?;
    Ok(Json(json!({
        "crawl_paused": s.scheduler.crawl_paused(),
        "scan_tasks": scan,
        "crawl_tasks": crawl,
        "cache": { "rows": cache_rows, "bytes": cache_bytes },
        "breakers": s.engine.breaker.snapshot(),
        "buckets": s.engine.buckets.snapshot(),
    })))
}
