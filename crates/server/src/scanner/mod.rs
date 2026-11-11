//! Local directory scanning: walkdir full/incremental scan + notify watcher.
//! Incremental rule: mtime + index_version. A broken book is marked
//! `status=broken` and never blocks the rest of the queue.

use crate::db::{BookRow, Database};
use crate::parser;
use crate::search::{SearchIndex, INDEX_VERSION};
use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tracing::{error, info, warn};
use walkdir::WalkDir;

#[derive(Debug, Default)]
pub struct ScanReport {
    pub indexed: usize,
    pub broken: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Parallel width actually used for this scan.
    pub workers: usize,
}

enum ScanOutcome {
    Indexed,
    Broken,
}

enum ScanMsg {
    Parsed(PublishItem),
    Broken,
}

/// Books per tantivy commit in the scan indexer.
const INDEX_BATCH: usize = 64;
/// Bytes of chapter text per batch: flush early so a few huge txt files
/// cannot balloon batch memory (64 books × 10MB would be 640MB).
const INDEX_BATCH_BYTES: usize = 64 * 1024 * 1024;

/// Stable book id: FNV-1a 64 of the canonical path (or URL for cached books).
pub fn book_id_for_path(path: &Path) -> u64 {
    let s = path.to_string_lossy();
    fnv(&s)
}

pub fn book_id_for_url(url: &str) -> String {
    fnv(url).to_string()
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn mtime_secs(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn is_book_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("epub") | Some("txt")
    )
}

/// Parallel scan width: `SHUKU_SCAN_WORKERS`, else half the CPU count
/// (1..=8). Parse workers and tantivy writer threads share the same cores,
/// so both default to half: total compute threads stay near core count
/// (measured best on 4 cores: 2 parse + 2 index; 4+4 was slower).
pub fn scan_workers() -> usize {
    std::env::var("SHUKU_SCAN_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(1, 32))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .div_ceil(2)
                .clamp(1, 8)
        })
}

/// One parsed book, ready to be indexed + published.
struct PublishItem {
    id_num: u64,
    id: String,
    title: String,
    author: String,
    path_str: String,
    mtime: i64,
    size: i64,
    /// Total chapter text bytes (for the batch byte cap).
    bytes: usize,
    titles: Vec<String>,
    chapter_tuples: Vec<(String, String)>,
    /// (bytes, mime) of the epub cover, if the epub declares one.
    cover: Option<(Vec<u8>, String)>,
}

/// Index a batch with ONE commit, then publish each book's DB row.
/// "Index first, then DB row" holds per book; a batch index failure marks
/// every book in the batch broken (the next scan re-picks them up).
async fn publish_batch(
    db: &Database,
    search: &Arc<SearchIndex>,
    batch: Vec<PublishItem>,
    data_dir: &Path,
) -> Vec<ScanOutcome> {
    let tuples: Vec<(u64, String, String, Vec<(String, String)>)> = batch
        .iter()
        .map(|b| (b.id_num, b.title.clone(), b.author.clone(), b.chapter_tuples.clone()))
        .collect();
    let search2 = search.clone();
    let indexed = tokio::task::spawn_blocking(move || search2.index_books(&tuples)).await;
    let counts = match indexed {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            error!(target: "scan", books = batch.len(), "batch index: {e}");
            for item in &batch {
                let _ = db.mark_book_status(&item.id, "broken").await;
            }
            return batch.into_iter().map(|_| ScanOutcome::Broken).collect();
        }
        Err(e) => {
            error!(target: "scan", books = batch.len(), "batch index task failed: {e}");
            for item in &batch {
                let _ = db.mark_book_status(&item.id, "broken").await;
            }
            return batch.into_iter().map(|_| ScanOutcome::Broken).collect();
        }
    };
    let now = chrono::Utc::now().timestamp();
    // Covers first: bytes go to data/covers/<id>, the row only stores the
    // MIME. A failed write clears the mime (the book still indexes); a
    // book whose cover was removed gets its stale file deleted.
    let cover_dir = data_dir.join("covers");
    let mut cover_mimes: Vec<String> = Vec::with_capacity(batch.len());
    for item in &batch {
        match &item.cover {
            Some((bytes, mime)) => {
                let f = cover_dir.join(&item.id);
                if std::fs::create_dir_all(&cover_dir).and_then(|_| std::fs::write(&f, bytes)).is_ok() {
                    cover_mimes.push(mime.clone());
                } else {
                    warn!(target: "scan", path = %item.path_str, "cover write failed");
                    cover_mimes.push(String::new());
                }
            }
            None => {
                let _ = std::fs::remove_file(cover_dir.join(&item.id));
                cover_mimes.push(String::new());
            }
        }
    }
    // One SQLite transaction for the whole batch (see upsert_books_batch:
    // per-book commits were the hidden bottleneck — 400 fsyncs per scan).
    let rows: Vec<BookRow> = batch
        .iter()
        .zip(counts.iter())
        .zip(cover_mimes.iter())
        .map(|((item, n), cover)| BookRow {
            id: item.id.clone(),
            title: item.title.clone(),
            author: item.author.clone(),
            path: Some(item.path_str.clone()),
            source_url: None,
            source: "local".into(),
            mtime: item.mtime,
            size: item.size,
            index_version: INDEX_VERSION,
            status: "ok".into(),
            chapter_count: *n as i64,
            created_at: now,
            updated_at: now,
            source_id: String::new(),
            has_update: false,
            last_update_check: 0,
            cover: cover.clone(),
        })
        .collect();
    let chaps: Vec<(String, Vec<String>)> =
        batch.iter().map(|item| (item.id.clone(), item.titles.clone())).collect();
    if let Err(e) = db.upsert_books_batch(rows, chaps).await {
        error!(target: "scan", books = batch.len(), "batch upsert: {e}");
        for item in &batch {
            let _ = db.mark_book_status(&item.id, "broken").await;
        }
        return batch.into_iter().map(|_| ScanOutcome::Broken).collect();
    }
    for (item, n) in batch.iter().zip(counts.iter()) {
        info!(target: "scan", path = %item.path_str, chapters = n, "indexed");
    }
    batch.into_iter().map(|_| ScanOutcome::Indexed).collect()
}

/// Parse ONE file (blocking CPU work). On parse failure the broken row is
/// written here; the caller only counts the outcome.
async fn parse_one_file(
    db: &Database,
    path: &Path,
    mtime: i64,
    size: i64,
    path_str: &str,
) -> Option<PublishItem> {
    let id_num = book_id_for_path(path);
    let id = id_num.to_string();
    let path_clone = path.to_path_buf();
    let parsed = match tokio::task::spawn_blocking(move || parser::parse_book_file(&path_clone)).await {
        Ok(p) => p,
        Err(e) => {
            error!(target: "scan", path = %path_str, "parse task failed: {e}");
            return None;
        }
    };
    match parsed {
        Ok(parsed) => {
            let titles: Vec<String> = parsed.chapters.iter().map(|c| c.title.clone()).collect();
            let chapter_tuples: Vec<(String, String)> = parsed
                .chapters
                .iter()
                .map(|c| (c.title.clone(), parser::strip_img_markers(&c.text)))
                .collect();
            let bytes = chapter_tuples.iter().map(|(_, t)| t.len()).sum();
            Some(PublishItem {
                id_num,
                id,
                title: parsed.title,
                author: parsed.author,
                path_str: path_str.to_string(),
                mtime,
                size,
                bytes,
                titles,
                chapter_tuples,
                cover: parsed.cover,
            })
        }
        Err(e) => {
            warn!(target: "scan", path = %path_str, "broken book, skipping: {e}");
            let now = chrono::Utc::now().timestamp();
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "未知".into());
            let _ = db
                .upsert_book(&BookRow {
                    id: id.clone(),
                    title: stem,
                    author: String::new(),
                    path: Some(path_str.to_string()),
                    source_url: None,
                    source: "local".into(),
                    mtime,
                    size,
                    index_version: 0,
                    status: "broken".into(),
                    chapter_count: 0,
                    created_at: now,
                    updated_at: now,
                    source_id: String::new(),
                    has_update: false,
                    last_update_check: 0,
                    cover: String::new(),
                })
                .await;
            None
        }
    }
}

/// Scan `dir` (recursive). Parses + indexes only files whose mtime changed
/// or that were never indexed.
pub async fn scan_once(db: &Database, search: &Arc<SearchIndex>, dir: &Path, data_dir: &Path) -> Result<ScanReport> {
    let mut report = ScanReport::default();
    if !dir.exists() {
        info!(target: "scan", dir = %dir.display(), "books dir does not exist, skipping");
        return Ok(report);
    }
    let known = db.all_book_paths().await?;
    let mut known_map: HashMap<String, (String, i64, i64)> = HashMap::new();
    for (id, path, mtime, iv) in known {
        known_map.insert(path, (id, mtime, iv));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(dir).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path().to_path_buf();
        if entry.file_type().is_file() && is_book_file(&p) {
            found.push(p);
        }
    }
    let found_set: std::collections::HashSet<String> = found.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    for (path, (id, _mtime, _iv)) in &known_map {
        if !found_set.contains(path) {
            // File vanished: mark broken (do not delete metadata silently).
            let _ = db.mark_book_status(id, "broken").await;
            report.removed += 1;
        }
    }
    // Cheap stat pass first (sequential): only changed / new files need work.
    let mut todo: Vec<(PathBuf, i64, i64, String)> = Vec::new();
    for path in found {
        let mtime = mtime_secs(&path);
        let size = std::fs::metadata(&path).map(|m| m.len() as i64).unwrap_or(0);
        let path_str = path.to_string_lossy().into_owned();
        let needs_index = match known_map.get(&path_str) {
            Some((_, old_mtime, iv)) => *old_mtime != mtime || *iv < INDEX_VERSION,
            None => true,
        };
        if !needs_index {
            report.unchanged += 1;
        } else {
            todo.push((path, mtime, size, path_str));
        }
    }
    // Parallel pipeline: each file is parsed (spawn_blocking — txt decode +
    // chapter split is CPU-bound) then indexed + published by one task.
    // The semaphore bounds in-flight files; the tantivy writer mutex inside
    // index_book serializes commits, and DB writes go through
    // tokio-rusqlite's single background connection.
    let workers = scan_workers();
    report.workers = workers;
    info!(target: "scan", total = todo.len(), workers, "parallel scan starting");
    // Producer/consumer: parse tasks run concurrently (bounded by the
    // semaphore); a single indexer task batches parsed books into one
    // tantivy commit per INDEX_BATCH books, so commit overhead does not
    // dominate a big scan. DB rows are still published only after the
    // batch containing them commits.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ScanMsg>(workers * 2);
    let db_idx = db.clone();
    let search_idx = search.clone();
    let data_dir_idx = data_dir.to_path_buf();
    let indexer = tokio::spawn(async move {
        let mut rep = ScanReport::default();
        rep.workers = workers;
        let mut batch: Vec<PublishItem> = Vec::new();
        let mut batch_bytes = 0usize;
        while let Some(msg) = rx.recv().await {
            match msg {
                ScanMsg::Parsed(item) => {
                    batch_bytes += item.bytes;
                    batch.push(item);
                    if batch.len() >= INDEX_BATCH || batch_bytes >= INDEX_BATCH_BYTES {
                        batch_bytes = 0;
                        for o in publish_batch(&db_idx, &search_idx, std::mem::take(&mut batch), &data_dir_idx).await {
                            match o {
                                ScanOutcome::Indexed => rep.indexed += 1,
                                ScanOutcome::Broken => rep.broken += 1,
                            }
                        }
                    }
                }
                ScanMsg::Broken => rep.broken += 1,
            }
        }
        if !batch.is_empty() {
            for o in publish_batch(&db_idx, &search_idx, batch, &data_dir_idx).await {
                match o {
                    ScanOutcome::Indexed => rep.indexed += 1,
                    ScanOutcome::Broken => rep.broken += 1,
                }
            }
        }
        rep
    });
    let sem = Arc::new(tokio::sync::Semaphore::new(workers));
    let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    for (path, mtime, size, path_str) in todo {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| anyhow::anyhow!("scan semaphore closed: {e}"))?;
        let db = db.clone();
        let tx = tx.clone();
        set.spawn(async move {
            let _permit = permit; // held until this file is parsed
            match parse_one_file(&db, &path, mtime, size, &path_str).await {
                Some(item) => {
                    let _ = tx.send(ScanMsg::Parsed(item)).await;
                }
                None => {
                    let _ = tx.send(ScanMsg::Broken).await;
                }
            }
        });
    }
    drop(tx);
    let mut join_errors = 0usize;
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            error!(target: "scan", "parse task failed: {e}");
            join_errors += 1;
        }
    }
    let mut rep = indexer.await.map_err(|e| anyhow::anyhow!("indexer task failed: {e}"))?;
    rep.broken += join_errors;
    rep.unchanged = report.unchanged;
    rep.removed = report.removed;
    Ok(rep)
}

/// Re-index cached network books from their per-chapter text files
/// (used by rebuild-index; the directory scanner only covers local books).
pub async fn reindex_cached_books(db: &Database, search: &Arc<SearchIndex>, data_dir: &Path) -> Result<usize> {
    let books = db.cached_books().await?;
    let mut n = 0;
    for (id, title, author) in books {
        let (chapters, _total) = db.list_chapters(&id, 1, 100_000).await?;
        let mut tuples: Vec<(String, String)> = Vec::new();
        for ch in chapters {
            let f = data_dir.join("chapters").join(&id).join(format!("{}.txt", ch.idx));
            let text = tokio::fs::read_to_string(&f).await.unwrap_or_default();
            tuples.push((ch.title, text));
        }
        let id_num: u64 = id.parse().unwrap_or(0);
        let search2 = search.clone();
        let (t, a) = (title, author);
        tokio::task::spawn_blocking(move || search2.index_book(id_num, &t, &a, &tuples))
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??;
        n += 1;
    }
    Ok(n)
}

/// Start a notify watcher: file create/modify under the books dir enqueues
/// an incremental scan task (best-effort; the periodic scan is the fallback).
pub fn start_watcher(db: Database, dir: &Path) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        let Ok(event) = res else { return };
        use notify::EventKind;
        let relevant = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_));
        if !relevant {
            return;
        }
        for p in &event.paths {
            if is_book_file(p) {
                let db2 = db.clone();
                let path = p.to_string_lossy().into_owned();
                tokio::spawn(async move {
                    let payload = serde_json::json!({"path": path}).to_string();
                    let _ = db2.enqueue_task("scan_tasks", "scan", &payload).await;
                });
                break;
            }
        }
    })?;
    watcher.watch(dir, RecursiveMode::Recursive)?;
    info!(target: "scan", dir = %dir.display(), "notify watcher started");
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchIndex;

    fn write_txt(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[tokio::test]
    async fn incremental_scan_by_mtime() {
        let tmp = std::env::temp_dir().join(format!("shuku-scan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Database::open_in_memory().await.unwrap();
        let search = Arc::new(SearchIndex::open_in_ram().unwrap());
        let book = write_txt(&tmp, "test.txt", "第一章 开始\n少年修仙。\n第二章 练功\n打坐修炼。\n");
        let r1 = scan_once(&db, &search, &tmp, &tmp.join("data")).await.unwrap();
        assert_eq!(r1.indexed, 1);
        assert_eq!(r1.broken, 0);
        let r2 = scan_once(&db, &search, &tmp, &tmp.join("data")).await.unwrap();
        assert_eq!(r2.indexed, 0, "unchanged book must not be re-indexed");
        assert_eq!(r2.unchanged, 1);
        // Touch the file: new mtime forces re-index.
        let later = SystemTime::now() + std::time::Duration::from_secs(2);
        filetime_set(&book, later);
        let r3 = scan_once(&db, &search, &tmp, &tmp.join("data")).await.unwrap();
        assert_eq!(r3.indexed, 1, "mtime change must trigger re-index");
        // Search must see the content.
        let (hits, _) = search.search("修仙", None, 10, 0).unwrap();
        assert!(!hits.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn scan_extracts_epub_cover() {
        let tmp = std::env::temp_dir().join(format!("shuku-scan-cover-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Database::open_in_memory().await.unwrap();
        let search = Arc::new(SearchIndex::open_in_ram().unwrap());
        std::fs::write(tmp.join("图册.epub"), crate::parser::test_epub_with_cover()).unwrap();
        let data = tmp.join("data");
        let r = scan_once(&db, &search, &tmp, &data).await.unwrap();
        assert_eq!(r.indexed, 1);
        let (books, _) = db.list_books(None, None, None, "updated", 1, 10).await.unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].cover, "image/jpeg");
        let cover_file = data.join("covers").join(&books[0].id);
        assert!(cover_file.exists(), "cover bytes must be stored under data/covers");
        assert_eq!(std::fs::read(&cover_file).unwrap().as_slice(), b"\xff\xd8\xff\xe0fake-cover");
        // A txt book has no cover.
        write_txt(&tmp, "plain.txt", "第一章 开始\n内容。\n");
        let r2 = scan_once(&db, &search, &tmp, &data).await.unwrap();
        assert_eq!(r2.indexed, 1);
        let (books2, _) = db.list_books(None, None, None, "updated", 1, 10).await.unwrap();
        let plain = books2.iter().find(|b| b.title == "plain").unwrap();
        assert!(plain.cover.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn broken_book_does_not_block_queue() {
        let tmp = std::env::temp_dir().join(format!("shuku-scan-broken-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Database::open_in_memory().await.unwrap();
        let search = Arc::new(SearchIndex::open_in_ram().unwrap());
        write_txt(&tmp, "good.txt", "第一章 好书\n内容正常。\n");
        let bad = tmp.join("bad.epub");
        std::fs::write(&bad, b"not a zip").unwrap();
        let r = scan_once(&db, &search, &tmp, &tmp.join("data")).await.unwrap();
        assert_eq!(r.indexed, 1);
        assert_eq!(r.broken, 1);
        let bad_row = db.get_book_by_path(&bad.to_string_lossy()).await.unwrap().unwrap();
        assert_eq!(bad_row.status, "broken");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn parallel_scan_many_txt_books() {
        let tmp = std::env::temp_dir().join(format!("shuku-scan-par-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = Database::open_in_memory().await.unwrap();
        let search = Arc::new(SearchIndex::open_in_ram().unwrap());
        // 20 txt books, each with a unique searchable phrase, plus 1 broken epub.
        for i in 0..20 {
            let content = format!("第一章 开篇\n这是第{i}本小说的独特内容词{i}。\n第二章 继续\n故事继续发展词{i}。\n");
            write_txt(&tmp, &format!("book{i:02}.txt"), &content);
        }
        std::fs::write(tmp.join("bad.epub"), b"not a zip").unwrap();
        let r1 = scan_once(&db, &search, &tmp, &tmp.join("data")).await.unwrap();
        assert!(r1.workers >= 1, "workers must be reported");
        assert_eq!(r1.indexed, 20, "all 20 txt books must be indexed, got {r1:?}");
        assert_eq!(r1.broken, 1, "broken epub must be counted, not block the queue");
        // Every book must be searchable (index-first invariant holds in parallel).
        for i in [0, 7, 19] {
            let (hits, _) = search.search(&format!("独特内容词{i}"), None, 10, 0).unwrap();
            assert!(!hits.is_empty(), "book {i} must be searchable");
        }
        let (hits, total) = search.search("故事继续发展", None, 100, 0).unwrap();
        assert_eq!(total, 20, "all 20 books share the phrase, hits: {}", hits.len());
        // Second scan: txt books unchanged, broken epub retried (index_version 0).
        let r2 = scan_once(&db, &search, &tmp, &tmp.join("data")).await.unwrap();
        assert_eq!(r2.indexed, 0);
        assert_eq!(r2.unchanged, 20);
        assert_eq!(r2.broken, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Perf probe: time jieba tokenization modes over the corpus, to see
    /// whether search-mode (cut_for_search + HMM) is what caps scan speed.
    #[test]
    fn probe_jieba_modes() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("perf-books");
        if !dir.exists() {
            return;
        }
        let mut texts: Vec<String> = Vec::new();
        for e in walkdir::WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            if e.file_type().is_file() {
                let parsed = crate::parser::parse_book_file(e.path()).unwrap();
                for c in parsed.chapters {
                    texts.push(c.text);
                }
            }
        }
        let total_bytes: usize = texts.iter().map(|t| t.len()).sum();
        let jieba = tantivy_jieba::jieba_rs::Jieba::new();
        for (name, f) in [
            ("cut", 0u8),
            ("cut_for_search", 1u8),
            ("cut_no_hmm", 2u8),
        ] {
            let t0 = std::time::Instant::now();
            let mut tokens = 0usize;
            for t in &texts {
                let n = match f {
                    0 => jieba.cut(t, true).len(),
                    1 => jieba.cut_for_search(t, true).len(),
                    _ => jieba.cut(t, false).len(),
                };
                tokens += n;
            }
            println!(
                "jieba probe: mode={name} ms={} tokens={tokens} bytes={total_bytes}",
                t0.elapsed().as_millis()
            );
        }
    }

    /// Perf probe (not a correctness test): splits a big scan into parse
    /// time vs index time vs DB time so a bottleneck can be located instead
    /// of guessed. Reads /tmp/shuku-perf/books when present, otherwise skips.
    #[tokio::test]
    async fn probe_scan_split() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("perf-books");
        if !dir.exists() {
            return;
        }
        let t0 = std::time::Instant::now();
        let mut books: Vec<(u64, String, String, Vec<(String, String)>)> = Vec::new();
        let mut files: Vec<PathBuf> = Vec::new();
        for e in walkdir::WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            if e.file_type().is_file() {
                files.push(e.path().to_path_buf());
            }
        }
        files.sort();
        for f in &files {
            let parsed = crate::parser::parse_book_file(f).unwrap();
            let tuples: Vec<(String, String)> =
                parsed.chapters.iter().map(|c| (c.title.clone(), c.text.clone())).collect();
            books.push((book_id_for_path(f), parsed.title, parsed.author, tuples));
        }
        let parse_ms = t0.elapsed().as_millis();
        // Disk index, 16-book batches, like the real scanner.
        let idx_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("probe-idx-{}", uuid::Uuid::new_v4()));
        let search = Arc::new(SearchIndex::open(&idx_dir).unwrap());
        let t1 = std::time::Instant::now();
        let mut total_ch = 0usize;
        for chunk in books.chunks(16) {
            let search2 = search.clone();
            let chunk = chunk.to_vec();
            let counts = tokio::task::spawn_blocking(move || search2.index_books(&chunk))
                .await
                .unwrap()
                .unwrap();
            total_ch += counts.iter().sum::<usize>();
        }
        let index_ms = t1.elapsed().as_millis();
        // DB publish: upsert + chapter titles, like publish_batch.
        let db = Database::open_in_memory().await.unwrap();
        let t2 = std::time::Instant::now();
        let now = chrono::Utc::now().timestamp();
        for (id_num, title, author, chapters) in &books {
            let id = id_num.to_string();
            let titles: Vec<String> = chapters.iter().map(|c| c.0.clone()).collect();
            db.upsert_book(&BookRow {
                id: id.clone(),
                title: title.clone(),
                author: author.clone(),
                path: Some(format!("perf-books/{id}.txt")),
                source_url: None,
                source: "local".into(),
                mtime: 0,
                size: 0,
                index_version: INDEX_VERSION,
                status: "ok".into(),
                chapter_count: chapters.len() as i64,
                created_at: now,
                updated_at: now,
                source_id: String::new(),
                has_update: false,
                last_update_check: 0,
                cover: String::new(),
            })
            .await
            .unwrap();
            db.replace_chapters(&id, &titles).await.unwrap();
        }
        let db_ms = t2.elapsed().as_millis();
        println!(
            "probe: files={} parse_ms={parse_ms} index_disk_batched_ms={index_ms} db_ms={db_ms} chapters={total_ch}",
            files.len()
        );
        let _ = std::fs::remove_dir_all(&idx_dir);
    }

    fn filetime_set(p: &Path, t: SystemTime) {
        // Portable mtime touch via std only: rewrite the file with same bytes.
        let _ = t;
        let bytes = std::fs::read(p).unwrap();
        std::fs::write(p, bytes).unwrap();
        // Ensure the mtime actually advanced (fs granularity can be 1s on some FS).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let bytes = std::fs::read(p).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
}
