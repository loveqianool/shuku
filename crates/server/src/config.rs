use std::path::PathBuf;

/// Read an env var, falling back to `default` when missing, unparseable,
/// or out of range (clamped). Invalid values never fail startup.
/// Numeric env var with both bounds clamped. An unparsable value (including
/// an empty string) falls back to `default`; an out-of-range value is
/// clamped instead of being silently accepted (a `SHUKU_RATE_PER_SEC=1e9`
/// would otherwise hammer sources, `SHUKU_CACHE_MAX_MB=1e9` would OOM).
fn env_num<T>(name: &str, default: T, min: T, max: T) -> T
where
    T: std::str::FromStr + PartialOrd + Copy,
{
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .map(|v| if v < min { min } else if v > max { max } else { v })
        .unwrap_or(default)
}

/// String env var; an empty / whitespace-only value counts as unset
/// (otherwise `SHUKU_HOME=""` would resolve books/data under an empty path).
fn env_str(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|v| {
        let t = v.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    })
}

/// Directory layout (unified under one root):
///
/// ```text
/// $SHUKU_HOME/
///   books/      local library (epub/txt), scanned by the watcher
///   data/
///     shuku.db     SQLite metadata (books/sources/tasks/progress/...)
///     index/        tantivy chapter index
///     chapters/     cached network-book chapter text (<book_id>/<idx>.txt)
///     covers/       epub cover bytes (<book_id>)
/// ```
///
/// `SHUKU_HOME` defaults to `.` (current directory), so a bare launch
/// behaves exactly as before (`./books`, `./data`). `SHUKU_BOOKS_DIR`
/// and `SHUKU_DATA_DIR` still override each directory individually.
/// The scanner's staging dirs under the OS temp dir stay ephemeral.
#[derive(Debug, Clone)]
pub struct Config {
    /// Service root; `books` and `data` live directly under it.
    pub home: PathBuf,
    pub books_dir: PathBuf,
    pub data_dir: PathBuf,
    pub addr: String,
    /// Per-domain token bucket refill (requests/second).
    pub rate_per_sec: f64,
    /// Per-domain token bucket capacity (burst).
    pub rate_burst: f64,
    /// Circuit breaker: failures before the domain cools down.
    pub breaker_failures: u32,
    /// Circuit breaker cooldown in seconds.
    pub breaker_cooldown_secs: u64,
    /// Three-level cache TTLs in seconds (search / toc / content).
    pub cache_ttl_search_secs: i64,
    pub cache_ttl_toc_secs: i64,
    pub cache_ttl_content_secs: i64,
    /// Update-check worker interval in seconds (cached books only).
    pub update_check_interval_secs: u64,
    /// Source health-check worker interval in seconds.
    pub source_health_interval_secs: u64,
    /// source_cache caps: max total body bytes (MiB) and max rows.
    pub cache_max_mb: i64,
    pub cache_max_rows: i64,
    /// Cache prune worker interval in seconds.
    pub cache_prune_interval_secs: u64,
    /// Task history retention: delete done/failed tasks older than this (days).
    pub task_retention_days: i64,
    /// Task history cap: max total rows across scan_tasks + crawl_tasks.
    pub task_max_rows: i64,
    /// Parallel scan width (`SHUKU_SCAN_WORKERS`).
    pub scan_workers: usize,
    /// tantivy writer threads (`SHUKU_INDEX_THREADS`).
    pub index_threads: usize,
    /// tantivy writer heap in MiB (`SHUKU_INDEX_HEAP_MB`).
    pub index_heap_mb: usize,
}

/// Parse + index workers share the cores, so both default to half the
/// CPU count (1..=8): total compute threads stay near core count
/// (measured best on 4 cores: 2 parse + 2 index; 4+4 was slower).
fn half_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .div_ceil(2)
        .clamp(1, 8)
}

pub(crate) fn default_scan_workers() -> usize {
    half_cores()
}

pub(crate) fn default_index_threads() -> usize {
    half_cores()
}

pub(crate) fn default_index_heap_mb() -> usize {
    150
}

impl Default for Config {
    fn default() -> Self {
        Self {
            home: PathBuf::from("."),
            books_dir: PathBuf::from("./books"),
            data_dir: PathBuf::from("./data"),
            addr: "0.0.0.0:8080".to_string(),
            rate_per_sec: 2.0,
            rate_burst: 5.0,
            breaker_failures: 5,
            breaker_cooldown_secs: 60,
            cache_ttl_search_secs: 60,
            cache_ttl_toc_secs: 600,
            cache_ttl_content_secs: 86_400,
            update_check_interval_secs: 21_600,
            source_health_interval_secs: 86_400,
            cache_max_mb: 512,
            cache_max_rows: 20_000,
            cache_prune_interval_secs: 86_400,
            task_retention_days: 30,
            task_max_rows: 10_000,
            scan_workers: default_scan_workers(),
            index_threads: default_index_threads(),
            index_heap_mb: default_index_heap_mb(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let d = Self::default();
        let home = env_str("SHUKU_HOME").map(PathBuf::from).unwrap_or(d.home);
        Self {
            home: home.clone(),
            books_dir: env_str("SHUKU_BOOKS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("books")),
            data_dir: env_str("SHUKU_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("data")),
            addr: env_str("SHUKU_ADDR").unwrap_or(d.addr),
            rate_per_sec: env_num("SHUKU_RATE_PER_SEC", d.rate_per_sec, 0.1, 1000.0),
            rate_burst: env_num("SHUKU_RATE_BURST", d.rate_burst, 1.0, 1000.0),
            breaker_failures: env_num("SHUKU_BREAKER_FAILURES", d.breaker_failures as u64, 1, 1000) as u32,
            breaker_cooldown_secs: env_num("SHUKU_BREAKER_COOLDOWN_SECS", d.breaker_cooldown_secs, 1, 86_400),
            cache_ttl_search_secs: env_num("SHUKU_CACHE_TTL_SEARCH_SECS", d.cache_ttl_search_secs, 0, 86_400),
            cache_ttl_toc_secs: env_num("SHUKU_CACHE_TTL_TOC_SECS", d.cache_ttl_toc_secs, 0, 2_592_000),
            cache_ttl_content_secs: env_num("SHUKU_CACHE_TTL_CONTENT_SECS", d.cache_ttl_content_secs, 0, 31_536_000),
            update_check_interval_secs: env_num("SHUKU_UPDATE_CHECK_INTERVAL_SECS", d.update_check_interval_secs, 60, 2_592_000),
            source_health_interval_secs: env_num("SHUKU_SOURCE_HEALTH_INTERVAL_SECS", d.source_health_interval_secs, 3600, 2_592_000),
            cache_max_mb: env_num("SHUKU_CACHE_MAX_MB", d.cache_max_mb, 16, 16_384),
            cache_max_rows: env_num("SHUKU_CACHE_MAX_ROWS", d.cache_max_rows, 100, 1_000_000),
            cache_prune_interval_secs: env_num("SHUKU_CACHE_PRUNE_INTERVAL_SECS", d.cache_prune_interval_secs, 3600, 2_592_000),
            task_retention_days: env_num("SHUKU_TASK_RETENTION_DAYS", d.task_retention_days, 1, 3650),
            task_max_rows: env_num("SHUKU_TASK_MAX_ROWS", d.task_max_rows, 100, 1_000_000),
            scan_workers: env_num("SHUKU_SCAN_WORKERS", d.scan_workers, 1, 32),
            index_threads: env_num("SHUKU_INDEX_THREADS", d.index_threads, 1, 16),
            index_heap_mb: env_num("SHUKU_INDEX_HEAP_MB", d.index_heap_mb, 50, 2048),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("shuku.db")
    }

    pub fn index_path(&self) -> PathBuf {
        self.data_dir.join("index")
    }

    /// Per-chapter text files for cached network books.
    pub fn chapter_file(&self, book_id: &str, idx: usize) -> PathBuf {
        self.data_dir
            .join("chapters")
            .join(book_id)
            .join(format!("{idx}.txt"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_and_invalid_falls_back() {
        // No other test reads these vars, so set/remove here is safe.
        std::env::set_var("SHUKU_RATE_PER_SEC", "10");
        std::env::set_var("SHUKU_RATE_BURST", "bogus");
        std::env::set_var("SHUKU_BREAKER_FAILURES", "3");
        std::env::set_var("SHUKU_CACHE_TTL_SEARCH_SECS", "5");
        let c = Config::from_env();
        std::env::remove_var("SHUKU_RATE_PER_SEC");
        std::env::remove_var("SHUKU_RATE_BURST");
        std::env::remove_var("SHUKU_BREAKER_FAILURES");
        std::env::remove_var("SHUKU_CACHE_TTL_SEARCH_SECS");
        assert_eq!(c.rate_per_sec, 10.0);
        assert_eq!(c.rate_burst, 5.0, "unparseable value falls back to default");
        assert_eq!(c.breaker_failures, 3);
        assert_eq!(c.cache_ttl_search_secs, 5);
        assert_eq!(c.cache_ttl_toc_secs, 600, "unset vars keep defaults");
    }

    #[test]
    fn scan_and_index_vars() {
        std::env::set_var("SHUKU_SCAN_WORKERS", "3");
        std::env::set_var("SHUKU_INDEX_THREADS", "2");
        std::env::set_var("SHUKU_INDEX_HEAP_MB", "200");
        let c = Config::from_env();
        std::env::remove_var("SHUKU_SCAN_WORKERS");
        std::env::remove_var("SHUKU_INDEX_THREADS");
        std::env::remove_var("SHUKU_INDEX_HEAP_MB");
        assert_eq!(c.scan_workers, 3);
        assert_eq!(c.index_threads, 2);
        assert_eq!(c.index_heap_mb, 200);
        // Out-of-range values clamp instead of failing startup.
        std::env::set_var("SHUKU_SCAN_WORKERS", "99");
        std::env::set_var("SHUKU_INDEX_HEAP_MB", "1");
        let c = Config::from_env();
        std::env::remove_var("SHUKU_SCAN_WORKERS");
        std::env::remove_var("SHUKU_INDEX_HEAP_MB");
        assert_eq!(c.scan_workers, 32);
        assert_eq!(c.index_heap_mb, 50);
        // Unset: half the cores (1..=8) and 150 MiB heap.
        let c = Config::from_env();
        assert!((1..=8).contains(&c.scan_workers));
        assert!((1..=8).contains(&c.index_threads));
        assert_eq!(c.index_heap_mb, 150);
    }

    #[test]
    fn home_derivation_and_precedence() {
        // SHUKU_HOME alone derives both dirs under it.
        std::env::set_var("SHUKU_HOME", "/srv/shuku");
        let c = Config::from_env();
        assert_eq!(c.home, PathBuf::from("/srv/shuku"));
        assert_eq!(c.books_dir, PathBuf::from("/srv/shuku/books"));
        assert_eq!(c.data_dir, PathBuf::from("/srv/shuku/data"));
        // Per-dir vars win over the home derivation.
        std::env::set_var("SHUKU_BOOKS_DIR", "/mnt/books");
        let c = Config::from_env();
        assert_eq!(c.books_dir, PathBuf::from("/mnt/books"));
        assert_eq!(c.data_dir, PathBuf::from("/srv/shuku/data"));
        std::env::remove_var("SHUKU_HOME");
        std::env::remove_var("SHUKU_BOOKS_DIR");
        // No vars at all: defaults, identical to before SHUKU_HOME.
        let c = Config::from_env();
        assert_eq!(c.home, PathBuf::from("."));
        assert_eq!(c.books_dir, PathBuf::from("./books"));
        assert_eq!(c.data_dir, PathBuf::from("./data"));
    }
}
