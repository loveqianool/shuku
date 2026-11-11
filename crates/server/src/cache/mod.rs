//! Three-level cache: in-memory (fast) + SQLite `source_cache` (durable).
//! TTLs: search 60s, toc 10min, content 24h.

use crate::db::Database;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Search,
    Toc,
    Content,
}

impl CacheKind {
    /// Documented default TTLs. Only used by the test-only `Cache::new`;
    /// production goes through `Cache::with_ttls`.
    #[cfg(test)]
    pub fn ttl(&self) -> i64 {
        match self {
            CacheKind::Search => 60,
            CacheKind::Toc => 600,
            CacheKind::Content => 86_400,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CacheKind::Search => "search",
            CacheKind::Toc => "toc",
            CacheKind::Content => "content",
        }
    }
}

/// FNV-1a 64-bit hash — stable across restarts (unlike DefaultHasher).
pub fn url_hash(url: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in url.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub struct Cache {
    db: Database,
    mem: Mutex<HashMap<String, (String, Instant, i64)>>,
    /// TTL overrides in seconds, indexed like CacheKind::ttl() defaults
    /// (search, toc, content). `Cache::new` keeps the documented defaults.
    ttls: [i64; 3],
}

impl Cache {
    /// Test-only constructor with the documented default TTLs; production
    /// uses `with_ttls` (env-configured).
    #[cfg(test)]
    pub fn new(db: Database) -> Self {
        Self { db, mem: Mutex::new(HashMap::new()), ttls: [CacheKind::Search.ttl(), CacheKind::Toc.ttl(), CacheKind::Content.ttl()] }
    }

    pub fn with_ttls(db: Database, search_secs: i64, toc_secs: i64, content_secs: i64) -> Self {
        Self { db, mem: Mutex::new(HashMap::new()), ttls: [search_secs, toc_secs, content_secs] }
    }

    pub fn ttl(&self, kind: CacheKind) -> i64 {
        self.ttls[kind as usize]
    }

    pub async fn get(&self, kind: CacheKind, url: &str) -> Option<String> {
        let key = format!("{}:{}", kind.as_str(), url);
        let ttl = self.ttl(kind);
        if let Some((body, at, _)) = self.mem.lock().unwrap().get(&key) {
            if at.elapsed() < Duration::from_secs(ttl as u64) {
                return Some(body.clone());
            }
        }
        let hash = url_hash(&format!("{}:{url}", kind.as_str()));
        let now = chrono::Utc::now().timestamp();
        if let Ok(Some((_u, body))) = self.db.cache_get(&hash, now).await {
            self.mem.lock().unwrap().insert(key, (body.clone(), Instant::now(), ttl));
            return Some(body);
        }
        None
    }

    pub async fn put(&self, kind: CacheKind, url: &str, body: &str) {
        let key = format!("{}:{}", kind.as_str(), url);
        let ttl = self.ttl(kind);
        self.mem.lock().unwrap().insert(key, (body.to_string(), Instant::now(), ttl));
        let hash = url_hash(&format!("{}:{url}", kind.as_str()));
        let _ = self.db.cache_put(&hash, url, kind.as_str(), body, ttl).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_and_db_roundtrip() {
        let db = Database::open_in_memory().await.unwrap();
        let cache = Cache::new(db);
        assert!(cache.get(CacheKind::Search, "https://a.example/x").await.is_none());
        cache.put(CacheKind::Search, "https://a.example/x", "hello").await;
        assert_eq!(cache.get(CacheKind::Search, "https://a.example/x").await.as_deref(), Some("hello"));
        // Same URL, different kind: separate namespace.
        assert!(cache.get(CacheKind::Toc, "https://a.example/x").await.is_none());
    }

    #[tokio::test]
    async fn custom_ttls() {
        let db = Database::open_in_memory().await.unwrap();
        let cache = Cache::with_ttls(db, 5, 600, 86_400);
        assert_eq!(cache.ttl(CacheKind::Search), 5);
        assert_eq!(cache.ttl(CacheKind::Toc), 600);
        assert_eq!(cache.ttl(CacheKind::Content), 86_400);
        let db2 = Database::open_in_memory().await.unwrap();
        let defaults = Cache::new(db2);
        assert_eq!(defaults.ttl(CacheKind::Search), 60);
        assert_eq!(defaults.ttl(CacheKind::Content), 86_400);
    }

    #[test]
    fn url_hash_stable() {
        assert_eq!(url_hash("https://example.com/a"), url_hash("https://example.com/a"));
        assert_ne!(url_hash("https://example.com/a"), url_hash("https://example.com/b"));
    }
}
