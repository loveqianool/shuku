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
/// Shared with the scanner's book-id function (book ids must stay stable).
pub fn fnv1a_64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn url_hash(url: &str) -> String {
    format!("{:016x}", fnv1a_64(url))
}

/// In-memory cache caps: entries and total body bytes. Without them the
/// map only grew (every cached page body stayed resident for the life of
/// the process). Eviction is oldest-first by last write; expired entries
/// go first.
const MAX_MEM_ENTRIES: usize = 512;
const MAX_MEM_BYTES: usize = 64 * 1024 * 1024;

fn mem_bytes(map: &HashMap<String, (String, Instant, i64)>) -> usize {
    map.values().map(|(b, _, _)| b.len()).sum()
}

/// Drop expired entries, then the oldest entries until both caps hold.
/// Callers must hold the map lock.
fn evict_locked(map: &mut HashMap<String, (String, Instant, i64)>) {
    map.retain(|_, (_, at, ttl)| at.elapsed() < Duration::from_secs(*ttl as u64));
    while map.len() > MAX_MEM_ENTRIES || mem_bytes(map) > MAX_MEM_BYTES {
        // Oldest write wins eviction (insertion order is not tracked, so
        // scan for the smallest Instant).
        let oldest = map
            .iter()
            .min_by_key(|(_, (_, at, _))| *at)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                map.remove(&k);
            }
            None => break,
        }
    }
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
        {
            let mut mem = crate::lock_or_poisoned(&self.mem);
            if let Some((body, at, _)) = mem.get(&key) {
                if at.elapsed() < Duration::from_secs(ttl as u64) {
                    return Some(body.clone());
                }
                // Expired: drop it instead of leaving it resident forever.
                mem.remove(&key);
            }
        }
        let hash = url_hash(&format!("{}:{url}", kind.as_str()));
        let now = chrono::Utc::now().timestamp();
        if let Ok(Some((_u, body))) = self.db.cache_get(&hash, now).await {
            let mut mem = crate::lock_or_poisoned(&self.mem);
            mem.insert(key, (body.clone(), Instant::now(), ttl));
            evict_locked(&mut mem);
            return Some(body);
        }
        None
    }

    pub async fn put(&self, kind: CacheKind, url: &str, body: &str) {
        let key = format!("{}:{}", kind.as_str(), url);
        let ttl = self.ttl(kind);
        {
            let mut mem = crate::lock_or_poisoned(&self.mem);
            mem.insert(key, (body.to_string(), Instant::now(), ttl));
            evict_locked(&mut mem);
        }
        let hash = url_hash(&format!("{}:{url}", kind.as_str()));
        let _ = self.db.cache_put(&hash, url, kind.as_str(), body, ttl).await;
    }

    /// Drop expired in-memory entries (called by the cache worker each
    /// prune round; `put`/`get` already evict on the hot path).
    pub fn prune_expired(&self) {
        let mut mem = crate::lock_or_poisoned(&self.mem);
        mem.retain(|_, (_, at, ttl)| at.elapsed() < Duration::from_secs(*ttl as u64));
    }

    #[cfg(test)]
    pub fn mem_len(&self) -> usize {
        crate::lock_or_poisoned(&self.mem).len()
    }

    #[cfg(test)]
    pub fn mem_contains(&self, kind: CacheKind, url: &str) -> bool {
        crate::lock_or_poisoned(&self.mem).contains_key(&format!("{}:{}", kind.as_str(), url))
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

    #[tokio::test]
    async fn memory_cache_is_bounded_and_prunes_expired() {
        let db = Database::open_in_memory().await.unwrap();
        let cache = Cache::with_ttls(db, 60, 600, 86_400);
        for i in 0..(MAX_MEM_ENTRIES + 10) {
            cache.put(CacheKind::Search, &format!("https://a.example/{i}"), "x").await;
        }
        assert!(cache.mem_len() <= MAX_MEM_ENTRIES, "mem cache must stay capped");
        assert!(!cache.mem_contains(CacheKind::Search, "https://a.example/0"), "oldest entry must be evicted");
        assert!(cache.mem_contains(CacheKind::Search, &format!("https://a.example/{}", MAX_MEM_ENTRIES + 9)));
        // TTL 0: entries are expired on arrival and never stay resident.
        let db2 = Database::open_in_memory().await.unwrap();
        let cache2 = Cache::with_ttls(db2, 0, 600, 86_400);
        cache2.put(CacheKind::Search, "https://a.example/x", "hello").await;
        assert_eq!(cache2.mem_len(), 0);
        // TTL 1: the entry stays until prune_expired runs after expiry.
        let db3 = Database::open_in_memory().await.unwrap();
        let cache3 = Cache::with_ttls(db3, 1, 600, 86_400);
        cache3.put(CacheKind::Search, "https://a.example/y", "hello").await;
        assert_eq!(cache3.mem_len(), 1);
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        cache3.prune_expired();
        assert_eq!(cache3.mem_len(), 0);
    }
}
