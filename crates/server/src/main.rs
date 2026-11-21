mod api;
mod cache;
mod chapter_check;
mod config;
mod db;
mod jsonpath;
mod login_ui;
mod opds;
mod parser;
mod scanner;
mod scheduler;
mod search;
mod semantic;
mod source;
mod static_files;
mod xpath;

use cache::Cache;
use config::Config;
use db::Database;
use scheduler::{run_crawl_worker, run_scan_worker, Scheduler};
use search::SearchIndex;
use semantic::SemanticStore;
use source::Engine;
use std::sync::Arc;
use tracing::info;

/// Lock a Mutex, recovering the inner value when a previous holder
/// panicked: a poisoned lock must not turn one panic into a permanent
/// outage for every later caller (the old `.lock().unwrap()` did).
pub(crate) fn lock_or_poisoned<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Same recovery for RwLock readers / writers.
pub(crate) fn rw_read_or_poisoned<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn rw_write_or_poisoned<T>(m: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("shuku=info,tower_http=info")),
        )
        .init();

    let cfg = Config::from_env();
    std::fs::create_dir_all(&cfg.books_dir)?;
    std::fs::create_dir_all(&cfg.data_dir)?;
    std::fs::create_dir_all(cfg.data_dir.join("chapters"))?;

    let database = Database::open(&cfg.db_path()).await?;
    let search = Arc::new(SearchIndex::open(&cfg.index_path(), cfg.index_threads, cfg.index_heap_mb)?);
    let cache = Cache::with_ttls(
        database.clone(),
        cfg.cache_ttl_search_secs,
        cfg.cache_ttl_toc_secs,
        cfg.cache_ttl_content_secs,
    );
    let engine = Arc::new(Engine::with_limits(
        cache,
        cfg.rate_per_sec,
        cfg.rate_burst,
        cfg.breaker_failures,
        std::time::Duration::from_secs(cfg.breaker_cooldown_secs),
    ));
    let sched = Arc::new(Scheduler::new(
        database.clone(),
        search.clone(),
        engine.clone(),
        cfg.books_dir.clone(),
        cfg.data_dir.clone(),
        cfg.scan_workers,
    ));

    // Background workers: two independent SQLite-driven queues.
    tokio::spawn(run_scan_worker(sched.clone()));
    tokio::spawn(run_crawl_worker(sched.clone()));
    tokio::spawn(crate::scheduler::run_update_worker(sched.clone(), cfg.update_check_interval_secs));
    tokio::spawn(crate::scheduler::run_health_worker(sched.clone(), cfg.source_health_interval_secs));
    tokio::spawn(crate::scheduler::run_cache_worker(
        sched.clone(),
        cfg.cache_prune_interval_secs,
        cfg.cache_max_rows,
        cfg.cache_max_mb * 1_000_000,
        cfg.task_retention_days,
        cfg.task_max_rows,
    ));

    // Filesystem watcher (best-effort): keep the handle alive for the run.
    let _watcher = match scanner::start_watcher(database.clone(), &cfg.books_dir) {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!(target: "scan", "notify watcher failed to start (periodic scan still works): {e}");
            None
        }
    };

    // Kick off an initial incremental scan on boot.
    let _ = database
        .enqueue_task("scan_tasks", "scan", &serde_json::json!({}).to_string())
        .await;

    // Semantic search store (precomputed chunk embeddings, optional):
    // absent or malformed file just disables the feature, never the boot.
    let semantic_path = cfg.data_dir.join("semantic.json");
    let semantic = if semantic_path.exists() {
        match SemanticStore::load(&semantic_path) {
            Ok(s) => {
                info!(
                    "semantic search ready: {} chunks, {} books, dims {}",
                    s.chunk_count(),
                    s.book_count(),
                    s.dims
                );
                Some(s)
            }
            Err(e) => {
                tracing::warn!("semantic.json present but failed to load ({e}); semantic search disabled");
                None
            }
        }
    } else {
        info!("no semantic.json in data dir; semantic search disabled");
        None
    };

    let state = api::AppState {
        db: database,
        search,
        engine,
        scheduler: sched,
        config: cfg.clone(),
        chapter_cache: Arc::new(api::ChapterCache::new(8)),
        semantic,
    };
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind(&cfg.addr).await?;
    info!("shuku listening on {} (books: {}, data: {})", cfg.addr, cfg.books_dir.display(), cfg.data_dir.display());
    axum::serve(listener, app).await?;
    Ok(())
}
