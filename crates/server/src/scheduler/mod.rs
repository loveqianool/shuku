//! Scheduler: per-domain token buckets, circuit breakers, and the two
//! SQLite-driven queues (scan_tasks / crawl_tasks). Workers are independent:
//! rebuilding the index pauses the crawl worker via a shared AtomicBool.

use crate::db::Database;
use crate::search::{SearchIndex, INDEX_VERSION};
use crate::source::{Engine, SourceDef};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Token bucket (per domain)
// ---------------------------------------------------------------------------

pub struct TokenBucket {
    rate_per_sec: f64,
    capacity: f64,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    pub fn new(rate_per_sec: f64, capacity: f64) -> Self {
        Self { rate_per_sec, capacity, state: Mutex::new((capacity, Instant::now())) }
    }

    /// Non-blocking attempt; returns true if a token was taken.
    pub fn try_acquire(&self) -> bool {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(s.1).as_secs_f64();
        s.0 = (s.0 + elapsed * self.rate_per_sec).min(self.capacity);
        s.1 = now;
        if s.0 >= 1.0 {
            s.0 -= 1.0;
            true
        } else {
            false
        }
    }

    pub fn available(&self) -> f64 {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(s.1).as_secs_f64();
        s.0 = (s.0 + elapsed * self.rate_per_sec).min(self.capacity);
        s.1 = now;
        s.0
    }

    pub async fn acquire(&self) {
        loop {
            if self.try_acquire() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Arc<TokenBucket>>>,
    rate: f64,
    capacity: f64,
}

impl RateLimiter {
    pub fn new(rate: f64, capacity: f64) -> Self {
        Self { buckets: Mutex::new(HashMap::new()), rate, capacity }
    }

    pub fn bucket_for(&self, domain: &str) -> Arc<TokenBucket> {
        let mut map = self.buckets.lock().unwrap();
        map.entry(domain.to_string())
            .or_insert_with(|| Arc::new(TokenBucket::new(self.rate, self.capacity)))
            .clone()
    }

    pub async fn acquire(&self, domain: &str) {
        let b = self.bucket_for(domain);
        b.acquire().await;
    }

    pub fn snapshot(&self) -> Vec<(String, f64)> {
        self.buckets
            .lock()
            .unwrap()
            .iter()
            .map(|(d, b)| (d.clone(), b.available()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Circuit breaker (per domain): N consecutive failures -> cooldown
// ---------------------------------------------------------------------------

pub struct CircuitBreaker {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    threshold: u32,
    cooldown: Duration,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self { failures: Mutex::new(HashMap::new()), threshold, cooldown }
    }

    pub fn check(&self, domain: &str) -> Result<()> {
        let map = self.failures.lock().unwrap();
        if let Some((n, since)) = map.get(domain) {
            if *n >= self.threshold && since.elapsed() < self.cooldown {
                return Err(anyhow!(
                    "circuit open for {domain}: {} failures, cooling down for {}s",
                    n,
                    self.cooldown.as_secs() - since.elapsed().as_secs()
                ));
            }
        }
        Ok(())
    }

    pub fn record_success(&self, domain: &str) {
        self.failures.lock().unwrap().remove(domain);
    }

    pub fn record_failure(&self, domain: &str) {
        let mut map = self.failures.lock().unwrap();
        let e = map.entry(domain.to_string()).or_insert((0, Instant::now()));
        e.0 += 1;
        if e.0 >= self.threshold {
            e.1 = Instant::now();
            warn!(target: "crawl", domain, failures = e.0, "circuit opened");
        }
    }

    #[cfg(test)]
    pub fn is_open(&self, domain: &str) -> bool {
        self.check(domain).is_err()
    }

    pub fn snapshot(&self) -> Vec<(String, u32, bool)> {
        self.failures
            .lock()
            .unwrap()
            .iter()
            .map(|(d, (n, since))| (d.clone(), *n, *n >= self.threshold && since.elapsed() < self.cooldown))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Scheduler state + workers
// ---------------------------------------------------------------------------

pub struct Scheduler {
    pub db: Database,
    pub search: Arc<SearchIndex>,
    pub engine: Arc<Engine>,
    pub crawl_paused: AtomicBool,
    pub books_dir: std::path::PathBuf,
    pub data_dir: std::path::PathBuf,
}

impl Scheduler {
    pub fn new(db: Database, search: Arc<SearchIndex>, engine: Arc<Engine>, books_dir: std::path::PathBuf, data_dir: std::path::PathBuf) -> Self {
        Self { db, search, engine, crawl_paused: AtomicBool::new(false), books_dir, data_dir }
    }

    pub fn pause_crawl(&self, paused: bool) {
        self.crawl_paused.store(paused, Ordering::SeqCst);
        info!(target: "crawl", paused, "crawl worker pause flag set");
    }

    pub fn crawl_paused(&self) -> bool {
        self.crawl_paused.load(Ordering::SeqCst)
    }
}

pub async fn run_scan_worker(sched: Arc<Scheduler>) {
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let task = match sched.db.claim_task("scan_tasks").await {
            Ok(t) => t,
            Err(e) => {
                error!(target: "scan", "claim scan task: {e}");
                continue;
            }
        };
        let Some(task) = task else { continue };
        info!(target: "scan", task_id = %task.id, r#type = %task.r#type, "processing scan task");
        let res = process_scan_task(&sched, &task).await;
        match res {
            Ok(msg) => {
                info!(target: "scan", task_id = %task.id, "{msg}");
                let _ = sched.db.finish_task("scan_tasks", &task.id, true, "").await;
            }
            Err(e) => {
                error!(target: "scan", task_id = %task.id, "scan task failed: {e}");
                let _ = sched.db.finish_task("scan_tasks", &task.id, false, &e.to_string()).await;
            }
        }
    }
}

/// Full-library chapter check (scan task `chapter_check`): title-number
/// gaps for every book, plus empty chapter files for cached books.
/// Local books' content is NOT parsed here (too slow for a sweep);
/// empty local chapters are only caught by the per-book live check.
async fn check_all_chapters(sched: &Arc<Scheduler>) -> Result<String> {
    let books = sched.db.list_all_book_ids().await?;
    let mut checked = 0i64;
    let mut with_issues = 0i64;
    let mut total_issues = 0i64;
    for (id, source, _) in books {
        let titles = sched.db.list_chapter_titles(&id).await?;
        let mut issues = crate::chapter_check::detect_gaps(&titles);
        if source == "cached" {
            for (idx, title) in &titles {
                let f = sched
                    .data_dir
                    .join("chapters")
                    .join(&id)
                    .join(format!("{idx}.txt"));
                let empty = match std::fs::metadata(&f) {
                    Ok(m) => m.len() == 0,
                    Err(_) => true, // missing file = missing chapter
                };
                if empty {
                    issues.push(crate::chapter_check::ChapterIssue {
                        idx: *idx,
                        title: title.clone(),
                        kind: "empty".into(),
                        detail: "章节文件为空或缺失（抓取失败）".into(),
                    });
                }
            }
        }
        sched.db.replace_chapter_issues(&id, &issues).await?;
        checked += 1;
        if !issues.is_empty() {
            with_issues += 1;
            total_issues += issues.len() as i64;
        }
    }
    Ok(format!(
        "chapter check done: {checked} books, {with_issues} with issues, {total_issues} issues"
    ))
}

async fn process_scan_task(sched: &Arc<Scheduler>, task: &crate::db::TaskRow) -> Result<String> {
    if task.r#type == "chapter_check" {
        return check_all_chapters(sched).await;
    }
    if task.r#type == "rebuild_index" {
        let payload: serde_json::Value = serde_json::from_str(&task.payload).unwrap_or(serde_json::json!({}));
        let pause = payload.get("pause_crawl").and_then(|v| v.as_bool()).unwrap_or(true);
        return rebuild_index(sched, pause).await;
    }
    let payload: serde_json::Value = serde_json::from_str(&task.payload).unwrap_or(serde_json::json!({}));
    let path = payload.get("path").and_then(|v| v.as_str()).map(|s| s.to_string());
    let dir = match &path {
        Some(p) => std::path::PathBuf::from(p),
        None => sched.books_dir.clone(),
    };
    // Only a scan of the whole books dir may mark missing books broken.
    // A single file (watcher) or subdirectory scan must not: it only walks
    // part of the library, so every other book would look "vanished".
    let full_scan = path.is_none() || dir == sched.books_dir;
    let report = crate::scanner::scan_once(&sched.db, &sched.search, &dir, &sched.data_dir, full_scan).await?;
    Ok(format!("scan done: {} new/updated, {} broken, {} unchanged", report.indexed, report.broken, report.unchanged))
}

pub async fn run_crawl_worker(sched: Arc<Scheduler>) {
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if sched.crawl_paused() {
            continue;
        }
        let task = match sched.db.claim_task("crawl_tasks").await {
            Ok(t) => t,
            Err(e) => {
                error!(target: "crawl", "claim crawl task: {e}");
                continue;
            }
        };
        let Some(task) = task else { continue };
        info!(target: "crawl", task_id = %task.id, r#type = %task.r#type, "processing crawl task");
        let res = process_crawl_task(&sched, &task).await;
        match res {
            Ok(msg) => {
                info!(target: "crawl", task_id = %task.id, "{msg}");
                let _ = sched.db.finish_task("crawl_tasks", &task.id, true, "").await;
            }
            Err(e) => {
                error!(target: "crawl", task_id = %task.id, "crawl task failed: {e}");
                let _ = sched.db.finish_task("crawl_tasks", &task.id, false, &e.to_string()).await;
            }
        }
    }
}

async fn process_crawl_task(sched: &Arc<Scheduler>, task: &crate::db::TaskRow) -> Result<String> {
    let payload: serde_json::Value = serde_json::from_str(&task.payload)?;
    if task.r#type == "check_update" {
        let book_id = payload.get("book_id").and_then(|v| v.as_str()).unwrap_or("");
        if book_id.is_empty() {
            return Err(anyhow!("check_update task needs book_id"));
        }
        return process_check_update(sched, book_id).await;
    }
    if task.r#type == "change_source" {
        let book_id = payload.get("book_id").and_then(|v| v.as_str()).unwrap_or("");
        let source_id = payload.get("source_id").and_then(|v| v.as_str()).unwrap_or("");
        let book_url = payload.get("book_url").and_then(|v| v.as_str()).map(|s| s.to_string());
        if book_id.is_empty() || source_id.is_empty() {
            return Err(anyhow!("change_source task needs book_id + source_id"));
        }
        return process_change_source(sched, book_id, source_id, book_url).await;
    }
    if task.r#type == "health_check" {
        let source_id = payload.get("source_id").and_then(|v| v.as_str()).unwrap_or("");
        if source_id.is_empty() {
            return Err(anyhow!("health_check task needs source_id"));
        }
        return process_health_check(sched, source_id).await;
    }
    if task.r#type == "refresh_subscription" {
        let url = payload.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() {
            return Err(anyhow!("refresh_subscription task needs url"));
        }
        return process_refresh_subscription(sched, url).await;
    }
    let source_id = payload.get("source_id").and_then(|v| v.as_str()).unwrap_or("");
    let book_url = payload.get("book_url").and_then(|v| v.as_str()).unwrap_or("");
    if source_id.is_empty() || book_url.is_empty() {
        return Err(anyhow!("crawl task needs source_id + book_url"));
    }
    let raw = sched
        .db
        .get_source_raw(source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found: {source_id}"))?;
    let raw_json: serde_json::Value = serde_json::from_str(&raw)?;
    let source = SourceDef::from_raw(&raw_json);
    let toc = sched.engine.run_toc(&source, book_url, false).await;
    if !toc.ok {
        return Err(anyhow!("toc fetch failed: {}", toc.error.unwrap_or_default()));
    }
    let chapters: Vec<crate::source::ChapterHit> = serde_json::from_value(toc.extracted)?;
    if chapters.is_empty() {
        return Err(anyhow!("toc extracted 0 chapters"));
    }
    // Fetch each chapter (rate-limited + breaker via engine.fetch) and store
    // text as per-chapter files under data/chapters/<book_id>/.
    let book_id = crate::scanner::book_id_for_url(book_url);
    let book_title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let author = payload.get("author").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let indexed = fetch_chapter_texts(sched, &source, &book_id, &chapters).await?;
    let titles: Vec<String> = chapters.iter().map(|c| c.name.clone()).collect();
    let now = chrono::Utc::now().timestamp();
    // Index first, then publish the DB row (same reason as the scanner).
    let book_id_num: u64 = book_id.parse().unwrap_or(0);
    let search = sched.search.clone();
    let idx_title = if book_title.is_empty() { book_url.to_string() } else { book_title.clone() };
    let author_for_index = author.clone();
    let n = tokio::task::spawn_blocking(move || search.index_book(book_id_num, &idx_title, &author_for_index, &indexed)).await??;
    sched
        .db
        .upsert_book(&crate::db::BookRow {
            id: book_id.clone(),
            title: if book_title.is_empty() { book_url.to_string() } else { book_title },
            author,
            path: None,
            source_url: Some(book_url.to_string()),
            source: "cached".into(),
            mtime: now,
            size: 0,
            index_version: INDEX_VERSION,
            status: "ok".into(),
            chapter_count: chapters.len() as i64,
            created_at: now,
            updated_at: now,
            source_id: source_id.to_string(),
            has_update: false,
            // Freshly crawled: the first periodic check is due after a full
            // interval, not on the worker's first round.
            last_update_check: now,
            cover: String::new(),
        })
        .await?;
    sched.db.replace_chapters(&book_id, &titles).await?;
    Ok(format!("crawled {} chapters ({} indexed)", chapters.len(), n))
}

/// Enqueue one `check_update` crawl task per due cached book (at most
/// `limit`), marking each checked so the next round does not re-enqueue it
/// until a full interval has passed. Returns the number enqueued.
pub async fn enqueue_due_updates(sched: &Arc<Scheduler>, interval_secs: i64, limit: i64) -> Result<usize> {
    let now = chrono::Utc::now().timestamp();
    let due = sched.db.due_update_books(now - interval_secs, limit).await?;
    let mut n = 0;
    for book in due {
        let payload = serde_json::json!({"book_id": book.id}).to_string();
        sched.db.enqueue_task("crawl_tasks", "check_update", &payload).await?;
        sched.db.mark_update_checked(&book.id).await?;
        n += 1;
    }
    if n > 0 {
        info!(target: "crawl", enqueued = n, "update checks queued");
    }
    Ok(n)
}

/// Periodic worker: every `interval_secs`, queue update checks for cached
/// books whose last check is older than the interval. The first round runs
/// 60s after boot so the boot scan can settle.
pub async fn run_update_worker(sched: Arc<Scheduler>, interval_secs: u64) {
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        match enqueue_due_updates(&sched, interval_secs as i64, 50).await {
            Ok(n) => {
                if n > 0 {
                    info!(target: "crawl", "update check round queued {n} books");
                }
            }
            Err(e) => error!(target: "crawl", "update check round failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

/// Load a source definition by id. A source deleted after a book was
/// cached can never be used again; callers that hit this return Ok(skip)
/// instead of failing the task forever.
async fn source_by_id(sched: &Arc<Scheduler>, source_id: &str) -> Result<Option<SourceDef>> {
    let Some(raw) = sched.db.get_source_raw(source_id).await? else {
        return Ok(None);
    };
    let raw_json: serde_json::Value = serde_json::from_str(&raw)?;
    Ok(Some(SourceDef::from_raw(&raw_json)))
}

/// Fetch every chapter's content (rate-limited + breaker via the engine),
/// writing each to data/chapters/<book_id>/<idx>.txt. The returned list is
/// positional: one (title, text) pair per chapter, in TOC order. A chapter
/// whose fetch fails gets an empty file and a warning instead of shifting
/// the indices of the chapters after it (the old crawl skipped failures,
/// which silently mis-indexed every later chapter).
async fn fetch_chapter_texts(
    sched: &Arc<Scheduler>,
    source: &SourceDef,
    book_id: &str,
    chapters: &[crate::source::ChapterHit],
) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(chapters.len());
    for (i, ch) in chapters.iter().enumerate() {
        if sched.crawl_paused() {
            return Err(anyhow!("crawl paused mid-task after {i} chapters"));
        }
        let res = sched.engine.run_content(source, &ch.url, false).await;
        let text = match res.ok {
            true => {
                let paras: Vec<crate::source::Paragraph> = serde_json::from_value(res.extracted).unwrap_or_default();
                paras.iter().filter_map(|p| p.text.clone()).collect::<Vec<_>>().join("\n")
            }
            false => {
                warn!(target: "crawl", chapter = i, "chapter fetch failed: {}", res.error.unwrap_or_default());
                String::new()
            }
        };
        let dir = sched.data_dir.join("chapters").join(book_id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{i}.txt")), &text)?;
        out.push((ch.name.clone(), text));
    }
    Ok(out)
}

/// Switch a cached book to another source: find the same book there (by
/// explicit book_url, or by searching the book title and taking the first
/// hit), re-fetch the whole TOC + content, and replace the stored chapters.
/// The book keeps its id (and therefore its progress / bookmarks / tags).
/// Stale chapter files beyond the new TOC length are removed so a direct
/// chapter fetch cannot serve text from the old source.
pub(crate) async fn process_change_source(
    sched: &Arc<Scheduler>,
    book_id: &str,
    source_id: &str,
    book_url: Option<String>,
) -> Result<String> {
    let book = sched.db.get_book(book_id).await?.ok_or_else(|| anyhow!("book not found: {book_id}"))?;
    if book.source != "cached" {
        return Ok("skip: only cached books can change source".into());
    }
    let Some(source) = source_by_id(sched, source_id).await? else {
        return Err(anyhow!("source not found: {source_id}"));
    };
    let book_url = match book_url.filter(|u| !u.is_empty()) {
        Some(u) => u,
        None => {
            let res = sched.engine.run_search_pages(&source, &book.title, 1, 2, true).await;
            if !res.ok {
                return Err(anyhow!("search on new source failed: {}", res.error.unwrap_or_default()));
            }
            let hits: Vec<crate::source::BookHit> = serde_json::from_value(res.extracted)?;
            let Some(hit) = hits.into_iter().find(|h| !h.book_url.is_empty()) else {
                return Err(anyhow!("new source has no book matching {:?}", book.title));
            };
            hit.book_url
        }
    };
    let toc = sched.engine.run_toc(&source, &book_url, false).await;
    if !toc.ok {
        return Err(anyhow!("toc fetch failed: {}", toc.error.unwrap_or_default()));
    }
    let chapters: Vec<crate::source::ChapterHit> = serde_json::from_value(toc.extracted)?;
    if chapters.is_empty() {
        return Err(anyhow!("toc extracted 0 chapters"));
    }
    let indexed = fetch_chapter_texts(sched, &source, book_id, &chapters).await?;
    // Drop stale files from a longer old TOC.
    let dir = sched.data_dir.join("chapters").join(book_id);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(n) = name.strip_suffix(".txt").and_then(|s| s.parse::<usize>().ok()) {
                if n >= chapters.len() {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    let titles: Vec<String> = chapters.iter().map(|c| c.name.clone()).collect();
    let id_num: u64 = book_id.parse().unwrap_or(0);
    let search = sched.search.clone();
    let (t, a) = (book.title.clone(), book.author.clone());
    tokio::task::spawn_blocking(move || search.index_book(id_num, &t, &a, &indexed))
        .await
        .map_err(|e| anyhow!("spawn_blocking: {e}"))??;
    sched.db.replace_chapters(book_id, &titles).await?;
    let mut row = book.clone();
    row.source_id = source_id.to_string();
    row.source_url = Some(book_url.clone());
    row.chapter_count = titles.len() as i64;
    row.has_update = false;
    row.last_update_check = chrono::Utc::now().timestamp();
    row.updated_at = row.last_update_check;
    sched.db.upsert_book(&row).await?;
    Ok(format!("source changed: {} chapters from {source_id}", titles.len()))
}

/// Enqueue one `health_check` task per enabled source. The crawl worker
/// runs them one at a time (rate-limited per domain by the engine).
pub async fn enqueue_health_checks(sched: &Arc<Scheduler>) -> Result<usize> {
    let sources = sched.db.list_sources_full().await?;
    let mut n = 0;
    for (id, _, enabled) in sources {
        if !enabled {
            continue;
        }
        let payload = serde_json::json!({"source_id": id}).to_string();
        sched.db.enqueue_task("crawl_tasks", "health_check", &payload).await?;
        n += 1;
    }
    if n > 0 {
        info!(target: "crawl", enqueued = n, "health checks queued");
    }
    Ok(n)
}

/// Periodic worker: every `interval_secs`, queue a health probe for every
/// enabled source and a refresh for every subscription URL. The first round
/// runs 5 minutes after boot.
pub async fn run_health_worker(sched: Arc<Scheduler>, interval_secs: u64) {
    tokio::time::sleep(Duration::from_secs(300)).await;
    loop {
        match enqueue_health_checks(&sched).await {
            Ok(n) => {
                if n > 0 {
                    info!(target: "crawl", "health check round queued {n} sources");
                }
            }
            Err(e) => error!(target: "crawl", "health check round failed: {e}"),
        }
        match enqueue_subscription_refreshes(&sched).await {
            Ok(n) => {
                if n > 0 {
                    info!(target: "crawl", "subscription refresh round queued {n} urls");
                }
            }
            Err(e) => error!(target: "crawl", "subscription refresh round failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

/// Cache prune worker: drop expired source_cache rows and enforce the
/// row/byte caps (oldest first). First run after 60s, then every interval.
pub async fn run_cache_worker(sched: Arc<Scheduler>, interval_secs: u64, max_rows: i64, max_bytes: i64) {
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        match sched.db.prune_source_cache(max_rows, max_bytes).await {
            Ok((expired, overflow)) => {
                if expired > 0 || overflow > 0 {
                    info!(target: "cache", "pruned source_cache: {expired} expired, {overflow} over cap");
                }
            }
            Err(e) => error!(target: "cache", "cache prune failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

/// Enqueue one `refresh_subscription` task per distinct subscription URL.
pub async fn enqueue_subscription_refreshes(sched: &Arc<Scheduler>) -> Result<usize> {
    let urls = sched.db.list_subscriptions().await?;
    let mut n = 0;
    for url in urls {
        let payload = serde_json::json!({"url": url}).to_string();
        sched.db.enqueue_task("crawl_tasks", "refresh_subscription", &payload).await?;
        n += 1;
    }
    if n > 0 {
        info!(target: "crawl", enqueued = n, "subscription refreshes queued");
    }
    Ok(n)
}

/// Fetch a rule subscription URL (JSON array of sources, or a single source
/// object) through the engine's pool and upsert every valid entry, tagging
/// each with the subscription URL so later rounds refresh it. One invalid
/// entry is skipped with a warning instead of failing the whole feed.
pub(crate) async fn process_refresh_subscription(sched: &Arc<Scheduler>, url: &str) -> Result<String> {
    let (status, body) = sched.engine.fetch_picker(url, "", None).await?;
    if status != 200 {
        return Err(anyhow!("subscription fetch failed: HTTP {status} for {url}"));
    }
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| anyhow!("subscription is not JSON: {e}"))?;
    let entries: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(a) => a,
        v if v.is_object() => vec![v],
        _ => return Err(anyhow!("subscription JSON must be an object or an array of sources")),
    };
    let mut imported = 0usize;
    let mut skipped = 0usize;
    for raw in entries {
        match crate::source::validate_source(&raw) {
            Ok(()) => {
                let id = sched.db.upsert_source(&raw).await?;
                sched.db.set_source_subscription(&id, url).await?;
                imported += 1;
            }
            Err(e) => {
                skipped += 1;
                warn!(target: "crawl", url, "skipping invalid subscription entry: {e}");
            }
        }
    }
    Ok(format!("subscription {url}: {imported} imported, {skipped} skipped"))
}

/// Probe one source: search its checkKeyWord (one page), falling back to
/// the explore page when the source has no keyword. A probe that returns
/// results is healthy; anything else records the failure. A source with
/// neither a keyword nor an explore URL cannot be probed and is left
/// 'unknown'.
pub(crate) async fn process_health_check(sched: &Arc<Scheduler>, source_id: &str) -> Result<String> {
    let Some(source) = source_by_id(sched, source_id).await? else {
        return Ok("skip: source deleted".into());
    };
    let keyword = source
        .rule_search
        .get("checkKeyWord")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (ok, detail): (bool, String) = if !keyword.is_empty() {
        let res = sched.engine.run_search_pages(&source, &keyword, 1, 1, true).await;
        if !res.ok {
            (false, res.error.unwrap_or_default())
        } else {
            let hits: Vec<crate::source::BookHit> = serde_json::from_value(res.extracted).unwrap_or_default();
            if hits.is_empty() {
                (false, format!("search for {keyword:?} returned no results"))
            } else {
                (true, format!("search for {keyword:?} returned {} books", hits.len()))
            }
        }
    } else if !source.explore_url.is_empty() {
        let res = sched.engine.run_explore(&source, 1, true).await;
        if !res.ok {
            (false, res.error.unwrap_or_default())
        } else {
            let hits: Vec<crate::source::BookHit> = serde_json::from_value(res.extracted).unwrap_or_default();
            if hits.is_empty() {
                (false, "explore returned no results".into())
            } else {
                (true, format!("explore returned {} books", hits.len()))
            }
        }
    } else {
        return Ok("skip: no checkKeyWord or exploreUrl to probe".into());
    };
    sched
        .db
        .set_source_health(source_id, if ok { "ok" } else { "fail" }, if ok { "" } else { &detail })
        .await?;
    if ok {
        Ok(format!("healthy: {detail}"))
    } else {
        Ok(format!("unhealthy: {detail}"))
    }
}

/// Re-fetch a cached book's TOC. New chapters (appended after the stored
/// count — serial novels append) are fetched, written, indexed, and the
/// book is flagged `has_update` so the shelf can badge it. A chapter whose
/// content fetch fails gets an empty file and a warning (like the initial
/// crawl, one bad chapter does not block the rest). A TOC that shrank or
/// is empty is a source hiccup, not an update: skip quietly.
pub(crate) async fn process_check_update(sched: &Arc<Scheduler>, book_id: &str) -> Result<String> {
    let book = sched.db.get_book(book_id).await?.ok_or_else(|| anyhow!("book not found: {book_id}"))?;
    let Some(book_url) = book.source_url.clone() else {
        return Ok("skip: book has no source_url".into());
    };
    if book.source != "cached" || book.source_id.is_empty() {
        return Ok("skip: not a cached book with a source".into());
    }
    let raw = sched
        .db
        .get_source_raw(&book.source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found: {}", book.source_id))?;
    // Source deleted after the book was cached: the check can never
    // succeed; complete the task instead of retrying forever.
    let raw_json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok("skip: source raw_json is corrupt".into()),
    };
    let source = SourceDef::from_raw(&raw_json);
    let toc = sched.engine.run_toc(&source, &book_url, false).await;
    if !toc.ok {
        return Err(anyhow!("toc fetch failed: {}", toc.error.unwrap_or_default()));
    }
    let chapters: Vec<crate::source::ChapterHit> = serde_json::from_value(toc.extracted)?;
    let existing = book.chapter_count as usize;
    if chapters.len() <= existing {
        return Ok(format!("no update (toc {} <= stored {existing})", chapters.len()));
    }
    let mut fetched = 0usize;
    for (i, ch) in chapters.iter().enumerate().skip(existing) {
        if sched.crawl_paused() {
            return Err(anyhow!("crawl paused mid-check after {} new chapters", i - existing));
        }
        let dir = sched.data_dir.join("chapters").join(&book.id);
        std::fs::create_dir_all(&dir)?;
        let res = sched.engine.run_content(&source, &ch.url, false).await;
        let text = match res.ok {
            true => {
                let paras: Vec<crate::source::Paragraph> = serde_json::from_value(res.extracted).unwrap_or_default();
                fetched += 1;
                paras.iter().filter_map(|p| p.text.clone()).collect::<Vec<_>>().join("\n")
            }
            false => {
                warn!(target: "crawl", book_url, chapter = i, "new chapter fetch failed: {}", res.error.unwrap_or_default());
                String::new()
            }
        };
        std::fs::write(dir.join(format!("{i}.txt")), &text)?;
    }
    // Re-index the whole book from its chapter files (index_book deletes
    // the book's docs first, so this is append-safe).
    let (old_titles, _) = sched.db.list_chapters(&book.id, 1, 100_000).await?;
    let mut titles: Vec<String> = old_titles.into_iter().map(|c| c.title).collect();
    titles.extend(chapters.iter().skip(existing).map(|c| c.name.clone()));
    let mut tuples: Vec<(String, String)> = Vec::with_capacity(titles.len());
    for (i, t) in titles.iter().enumerate() {
        let f = sched.data_dir.join("chapters").join(&book.id).join(format!("{i}.txt"));
        let text = tokio::fs::read_to_string(&f).await.unwrap_or_default();
        tuples.push((t.clone(), text));
    }
    let id_num: u64 = book.id.parse().unwrap_or(0);
    let search = sched.search.clone();
    let (t, a) = (book.title.clone(), book.author.clone());
    tokio::task::spawn_blocking(move || search.index_book(id_num, &t, &a, &tuples))
        .await
        .map_err(|e| anyhow!("spawn_blocking: {e}"))??;
    sched.db.replace_chapters(&book.id, &titles).await?;
    let mut row = book.clone();
    row.chapter_count = titles.len() as i64;
    row.has_update = fetched > 0;
    row.updated_at = chrono::Utc::now().timestamp();
    sched.db.upsert_book(&row).await?;
    Ok(format!("update: +{} chapters ({} fetched)", titles.len() - existing, fetched))
}

/// Rebuild the whole tantivy index from local books. Pauses the crawl
/// worker while running when `pause_crawl` is set.
pub async fn rebuild_index(sched: &Arc<Scheduler>, pause_crawl: bool) -> Result<String> {
    if pause_crawl {
        sched.pause_crawl(true);
    }
    // Run the fallible part in a helper so the crawl worker is resumed on
    // EVERY exit path: previously an early `?` (delete_all / scan_once /
    // reindex_cached_books) left `pause_crawl` set forever and the crawl
    // queue stalled while the index was already empty.
    let result = rebuild_index_inner(sched).await;
    if pause_crawl {
        sched.pause_crawl(false);
    }
    let (report, cached) = result?;
    Ok(format!("rebuild done: {} local indexed, {} cached re-indexed, {} broken", report.indexed, cached, report.broken))
}

async fn rebuild_index_inner(sched: &Arc<Scheduler>) -> Result<(crate::scanner::ScanReport, usize)> {
    let search = sched.search.clone();
    tokio::task::spawn_blocking(move || search.delete_all()).await??;
    info!(target: "search", "index cleared for rebuild");
    // Force the incremental scanner to pick local books up again.
    sched.db.reset_index_versions().await?;
    let report = crate::scanner::scan_once(&sched.db, &sched.search, &sched.books_dir, &sched.data_dir, true).await?;
    let cached = crate::scanner::reindex_cached_books(&sched.db, &sched.search, &sched.data_dir).await?;
    Ok((report, cached))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_bucket_limits_and_refills() {
        let b = TokenBucket::new(10.0, 2.0);
        assert!(b.try_acquire());
        assert!(b.try_acquire());
        assert!(!b.try_acquire(), "burst capacity is 2");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(b.try_acquire(), "should refill ~1.5 tokens in 150ms at 10/s");
    }

    #[test]
    fn breaker_opens_after_threshold_and_cools_down() {
        let br = CircuitBreaker::new(3, Duration::from_millis(200));
        assert!(br.check("example.com").is_ok());
        br.record_failure("example.com");
        br.record_failure("example.com");
        assert!(br.check("example.com").is_ok());
        br.record_failure("example.com");
        assert!(br.is_open("example.com"), "3 failures must open the circuit");
        std::thread::sleep(Duration::from_millis(250));
        assert!(br.check("example.com").is_ok(), "circuit must close after cooldown");
        br.record_success("example.com");
        assert!(br.check("example.com").is_ok());
    }

    #[test]
    fn limiter_is_per_domain() {
        let lim = RateLimiter::new(1.0, 1.0);
        let a = lim.bucket_for("a.example");
        let b = lim.bucket_for("b.example");
        assert!(a.try_acquire());
        assert!(!a.try_acquire());
        assert!(b.try_acquire(), "different domain must have its own bucket");
    }
}
