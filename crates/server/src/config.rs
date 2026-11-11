use std::path::PathBuf;

/// Read an env var, falling back to `default` when missing, unparseable,
/// or out of range (clamped). Invalid values never fail startup.
fn env_f64(name: &str, default: f64, min: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok()).map(|v| v.max(min)).unwrap_or(default)
}

fn env_u64(name: &str, default: u64, min: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()).map(|v| v.max(min)).unwrap_or(default)
}

fn env_i64(name: &str, default: i64, min: i64) -> i64 {
    std::env::var(name).ok().and_then(|v| v.parse::<i64>().ok()).map(|v| v.max(min)).unwrap_or(default)
}

/// Directory layout (unified under one root):
///
/// ```text
/// $LEGADO_HOME/
///   books/      local library (epub/txt), scanned by the watcher
///   data/
///     legado.db     SQLite metadata (books/sources/tasks/progress/...)
///     index/        tantivy chapter index
///     chapters/     cached network-book chapter text (<book_id>/<idx>.txt)
///     covers/       epub cover bytes (<book_id>)
/// ```
///
/// `LEGADO_HOME` defaults to `.` (current directory), so a bare launch
/// behaves exactly as before (`./books`, `./data`). `LEGADO_BOOKS_DIR`
/// and `LEGADO_DATA_DIR` still override each directory individually.
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
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let d = Self::default();
        let home = std::env::var("LEGADO_HOME").map(PathBuf::from).unwrap_or(d.home);
        Self {
            home: home.clone(),
            books_dir: std::env::var("LEGADO_BOOKS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join("books")),
            data_dir: std::env::var("LEGADO_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home.join("data")),
            addr: std::env::var("LEGADO_ADDR").unwrap_or(d.addr),
            rate_per_sec: env_f64("LEGADO_RATE_PER_SEC", d.rate_per_sec, 0.1),
            rate_burst: env_f64("LEGADO_RATE_BURST", d.rate_burst, 1.0),
            breaker_failures: env_u64("LEGADO_BREAKER_FAILURES", d.breaker_failures as u64, 1) as u32,
            breaker_cooldown_secs: env_u64("LEGADO_BREAKER_COOLDOWN_SECS", d.breaker_cooldown_secs, 1),
            cache_ttl_search_secs: env_i64("LEGADO_CACHE_TTL_SEARCH_SECS", d.cache_ttl_search_secs, 0),
            cache_ttl_toc_secs: env_i64("LEGADO_CACHE_TTL_TOC_SECS", d.cache_ttl_toc_secs, 0),
            cache_ttl_content_secs: env_i64("LEGADO_CACHE_TTL_CONTENT_SECS", d.cache_ttl_content_secs, 0),
            update_check_interval_secs: env_u64("LEGADO_UPDATE_CHECK_INTERVAL_SECS", d.update_check_interval_secs, 60),
            source_health_interval_secs: env_u64("LEGADO_SOURCE_HEALTH_INTERVAL_SECS", d.source_health_interval_secs, 3600),
            cache_max_mb: env_i64("LEGADO_CACHE_MAX_MB", d.cache_max_mb, 16),
            cache_max_rows: env_i64("LEGADO_CACHE_MAX_ROWS", d.cache_max_rows, 100),
            cache_prune_interval_secs: env_u64("LEGADO_CACHE_PRUNE_INTERVAL_SECS", d.cache_prune_interval_secs, 3600),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("legado.db")
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
        std::env::set_var("LEGADO_RATE_PER_SEC", "10");
        std::env::set_var("LEGADO_RATE_BURST", "bogus");
        std::env::set_var("LEGADO_BREAKER_FAILURES", "3");
        std::env::set_var("LEGADO_CACHE_TTL_SEARCH_SECS", "5");
        let c = Config::from_env();
        std::env::remove_var("LEGADO_RATE_PER_SEC");
        std::env::remove_var("LEGADO_RATE_BURST");
        std::env::remove_var("LEGADO_BREAKER_FAILURES");
        std::env::remove_var("LEGADO_CACHE_TTL_SEARCH_SECS");
        assert_eq!(c.rate_per_sec, 10.0);
        assert_eq!(c.rate_burst, 5.0, "unparseable value falls back to default");
        assert_eq!(c.breaker_failures, 3);
        assert_eq!(c.cache_ttl_search_secs, 5);
        assert_eq!(c.cache_ttl_toc_secs, 600, "unset vars keep defaults");
    }

    #[test]
    fn home_derivation_and_precedence() {
        // LEGADO_HOME alone derives both dirs under it.
        std::env::set_var("LEGADO_HOME", "/srv/legado");
        let c = Config::from_env();
        assert_eq!(c.home, PathBuf::from("/srv/legado"));
        assert_eq!(c.books_dir, PathBuf::from("/srv/legado/books"));
        assert_eq!(c.data_dir, PathBuf::from("/srv/legado/data"));
        // Per-dir vars win over the home derivation.
        std::env::set_var("LEGADO_BOOKS_DIR", "/mnt/books");
        let c = Config::from_env();
        assert_eq!(c.books_dir, PathBuf::from("/mnt/books"));
        assert_eq!(c.data_dir, PathBuf::from("/srv/legado/data"));
        std::env::remove_var("LEGADO_HOME");
        std::env::remove_var("LEGADO_BOOKS_DIR");
        // No vars at all: defaults, identical to before LEGADO_HOME.
        let c = Config::from_env();
        assert_eq!(c.home, PathBuf::from("."));
        assert_eq!(c.books_dir, PathBuf::from("./books"));
        assert_eq!(c.data_dir, PathBuf::from("./data"));
    }
}
