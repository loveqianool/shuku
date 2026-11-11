//! SQLite layer (tokio-rusqlite, single background connection).
//! Stores metadata / tasks / sources / cache / progress only — never body text.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio_rusqlite::Connection;

use crate::chapter_check::ChapterIssue;

pub const MIGRATION_SQL: &str = include_str!("../../migrations/001_init.sql");

/// Full books column list, in row_book order.
const BOOK_COLS: &str = "id,title,author,path,source_url,source,mtime,size,index_version,status,chapter_count,created_at,updated_at,source_id,has_update,last_update_check,cover";

#[derive(Debug, Clone)]
pub struct Database {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub title: String,
    pub author: String,
    pub books: Vec<BookRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadingStats {
    pub total_books: i64,
    pub reading_books: i64,
    pub finished_books: i64,
    pub total_seconds: i64,
    pub active_days: i64,
    /// (month "YYYY-MM", finished count), newest first, up to 12 months.
    pub finished_by_month: Vec<(String, i64)>,
    /// (id, title, author, finished_at unix ts), newest first.
    pub recent_finished: Vec<(String, String, String, i64)>,
    /// (id, title, author, seconds), most time first.
    pub top_books_by_time: Vec<(String, String, String, i64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookRow {
    pub id: String,
    pub title: String,
    pub author: String,
    pub path: Option<String>,
    pub source_url: Option<String>,
    pub source: String,
    pub mtime: i64,
    pub size: i64,
    pub index_version: i64,
    pub status: String,
    pub chapter_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Book-source id (bookSourceUrl) the cached book was crawled from.
    /// Empty for local books. Needed to re-fetch the TOC for update checks.
    #[serde(default)]
    pub source_id: String,
    /// True when an update check found new chapters the user has not seen
    /// yet (cleared when the chapter list is opened).
    #[serde(default)]
    pub has_update: bool,
    /// Unix seconds of the last update check (cached books only).
    #[serde(default)]
    pub last_update_check: i64,
    /// Cover image MIME type ("" = none). Bytes live in data/covers/<id>.
    #[serde(default)]
    pub cover: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterRow {
    pub book_id: String,
    pub idx: i64,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub r#type: String,
    pub status: String,
    pub payload: String,
    pub attempts: i64,
    pub next_retry_at: i64,
    pub error: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Database {
    pub async fn open(path: &Path) -> Result<Self> {
        // Rename 2026-11-11 (legado-server -> shuku): the database file was
        // legado.db. On first open, move it to shuku.db so existing
        // deployments keep their data; a fresh install just creates shuku.db.
        if path.file_name().map(|n| n == "shuku.db").unwrap_or(false) {
            let old = path.with_file_name("legado.db");
            if !path.exists() && old.exists() {
                std::fs::rename(&old, path)?;
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .await
            .with_context(|| format!("open sqlite {}", path.display()))?;
        let db = Self { conn };
        db.migrate().await?;
        Ok(db)
    }

    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().await?;
        let db = Self { conn };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<()> {
        let sql = MIGRATION_SQL.to_string();
        self.call_db(move |c| {
                c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
                c.execute_batch(&sql)?;
                // Reading time accumulated by the reader (seconds per book).
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS reading_seconds (
                        book_id    TEXT PRIMARY KEY REFERENCES books(id) ON DELETE CASCADE,
                        seconds    INTEGER NOT NULL DEFAULT 0,
                        updated_at INTEGER NOT NULL DEFAULT 0
                    )",
                )?;
                // Chapter-issue scan results (missing / scrambled chapters).
                c.execute_batch(
                    "CREATE TABLE IF NOT EXISTS chapter_issues (
                        book_id    TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
                        idx        INTEGER NOT NULL,
                        kind       TEXT NOT NULL,
                        detail     TEXT NOT NULL DEFAULT '',
                        checked_at INTEGER NOT NULL DEFAULT 0,
                        PRIMARY KEY (book_id, idx, kind)
                    )",
                )?;
                c.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_chapter_issues_book ON chapter_issues(book_id)",
                )?;
                // v2 columns (idempotent: only ADD when missing, so old
                // databases pick them up without a version table).
                for (table, col, ddl) in [
                    ("books", "source_id", "TEXT NOT NULL DEFAULT ''"),
                    ("books", "has_update", "INTEGER NOT NULL DEFAULT 0"),
                    ("books", "last_update_check", "INTEGER NOT NULL DEFAULT 0"),
                    ("books", "cover", "TEXT NOT NULL DEFAULT ''"),
                    ("books", "title_pinyin", "TEXT NOT NULL DEFAULT ''"),
                    ("sources", "health", "TEXT NOT NULL DEFAULT 'unknown'"),
                    ("sources", "health_error", "TEXT NOT NULL DEFAULT ''"),
                    ("sources", "subscription_url", "TEXT NOT NULL DEFAULT ''"),
                    ("sources", "subscription_updated_at", "INTEGER NOT NULL DEFAULT 0"),
                ] {
                    let exists: bool = c
                        .query_row(
                            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name='{col}'"),
                            [],
                            |r| r.get::<_, i64>(0),
                        )
                        .map(|n| n > 0)?;
                    if !exists {
                        c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {ddl}"))?;
                        if table == "books" && col == "title_pinyin" {
                            // Backfill once for rows written before the
                            // column existed (later rows get it from upsert).
                            let rows: Vec<(String, String)> = {
                                let mut stmt = c.prepare("SELECT id, title FROM books")?;
                                let rows = stmt
                                    .query_map([], |r| {
                                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                                    })?
                                    .collect::<Result<Vec<_>, _>>()?;
                                rows
                            };
                            for (id, title) in rows {
                                let py = crate::search::pinyin_text(&title);
                                c.execute(
                                    "UPDATE books SET title_pinyin = ?1 WHERE id = ?2",
                                    params![py, id],
                                )?;
                            }
                        }
                    }
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Run a blocking closure on the DB thread, converting tokio-rusqlite
    /// errors to anyhow.
    pub async fn call_db<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.conn.call(f).await.map_err(|e| anyhow!("db: {e}"))
    }

    // ---------- books ----------
    pub async fn get_book(&self, id: &str) -> Result<Option<BookRow>> {
        let id = id.to_string();
        self.call_db(move |c| {
                let row = c
                    .query_row(
                    &format!("SELECT {BOOK_COLS} FROM books WHERE id=?1"),
                        params![id],
                        row_book,
                    )
                    .optional()?;
                Ok(row)
            })
            .await
    }

    #[cfg(test)]
    pub async fn get_book_by_path(&self, path: &str) -> Result<Option<BookRow>> {
        let path = path.to_string();
        self.call_db(move |c| {
                let row = c
                    .query_row(
                    &format!("SELECT {BOOK_COLS} FROM books WHERE path=?1"),
                        params![path],
                        row_book,
                    )
                    .optional()?;
                Ok(row)
            })
            .await
    }

    pub async fn list_books(
        &self,
        q: Option<String>,
        author: Option<String>,
        tag: Option<String>,
        sort: &str,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<BookRow>, i64)> {
        let order = match sort {
            "title" => "title COLLATE NOCASE ASC",
            "author" => "author COLLATE NOCASE ASC",
            _ => "updated_at DESC",
        }
        .to_string();
        let (q, author, tag) = (q.unwrap_or_default(), author.unwrap_or_default(), tag.unwrap_or_default());
        let offset = (page.max(1) - 1) * page_size.max(1);
        let limit = page_size.max(1);
        self.call_db(move |c| {
                // NOTE: title/author filtering here is metadata filtering, NOT
                // full-text search (full-text search is tantivy only).
                let mut where_sql = String::from("1=1");
                let mut args: Vec<String> = vec![];
                if !q.is_empty() {
                    // exact-substring metadata filter on title, plus pinyin:
                    // the title's pinyin (syllables + concatenated + initials
                    // prefixes, see search::pinyin_text) lets "wlcl" match
                    // 武林沉沦. Use `instr` (not LIKE) to stay out of the
                    // LIKE-search ban; `lower()` makes pinyin matching
                    // case-insensitive.
                    where_sql.push_str(" AND (instr(title, ?1) > 0 OR instr(title_pinyin, lower(?1)) > 0)");
                    args.push(q);
                }
                if !author.is_empty() {
                    where_sql.push_str(&format!(" AND instr(author, ?{}) > 0", args.len() + 1));
                    args.push(author);
                }
                let tag_join = if tag.is_empty() {
                    String::new()
                } else {
                    format!(
                        " AND id IN (SELECT book_id FROM tags WHERE tag = '{}')",
                        tag.replace('\'', "''")
                    )
                };
                let total_sql = format!("SELECT COUNT(*) FROM books WHERE {where_sql}{tag_join}");
                let total: i64 = c.query_row(&total_sql, rusqlite::params_from_iter(args.iter()), |r| r.get(0))?;
                let list_sql = format!(
                    "SELECT {BOOK_COLS} FROM books WHERE {where_sql}{tag_join} ORDER BY {order} LIMIT {limit} OFFSET {offset}"
                );
                let mut stmt = c.prepare(&list_sql)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(args.iter()), row_book)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((rows, total))
            })
            .await
    }

    pub async fn upsert_book(&self, b: &BookRow) -> Result<()> {
        let b = b.clone();
        self.call_db(move |c| {
                c.execute(
                    "INSERT INTO books(id,title,author,path,source_url,source,mtime,size,index_version,status,chapter_count,created_at,updated_at,source_id,has_update,last_update_check,cover,title_pinyin) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
                     ON CONFLICT(id) DO UPDATE SET title=excluded.title,author=excluded.author,path=excluded.path,source_url=excluded.source_url,source=excluded.source,mtime=excluded.mtime,size=excluded.size,index_version=excluded.index_version,status=excluded.status,chapter_count=excluded.chapter_count,updated_at=excluded.updated_at,source_id=excluded.source_id,has_update=excluded.has_update,last_update_check=excluded.last_update_check,cover=excluded.cover,title_pinyin=excluded.title_pinyin",
                    params![b.id, b.title, b.author, b.path, b.source_url, b.source, b.mtime, b.size, b.index_version, b.status, b.chapter_count, b.created_at, b.updated_at, b.source_id, b.has_update, b.last_update_check, b.cover, crate::search::pinyin_text(&b.title)],
                )?;
                Ok(())
            })
            .await
    }

    /// Publish a whole scan batch in ONE SQLite transaction: one commit
    /// instead of one per book. With WAL + FULL synchronous mode every
    /// commit is an fsync, and a 200-book scan was spending most of its
    /// wall time on 400 fsyncs (upsert + chapters each), not on parsing
    /// or indexing. `chapters` is (book_id, titles) per book.
    pub async fn upsert_books_batch(
        &self,
        books: Vec<BookRow>,
        chapters: Vec<(String, Vec<String>)>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
            let tx = c.transaction()?;
            for b in &books {
                tx.execute(
                    "INSERT INTO books(id,title,author,path,source_url,source,mtime,size,index_version,status,chapter_count,created_at,updated_at,source_id,has_update,last_update_check,cover,title_pinyin) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
                     ON CONFLICT(id) DO UPDATE SET title=excluded.title,author=excluded.author,path=excluded.path,source_url=excluded.source_url,source=excluded.source,mtime=excluded.mtime,size=excluded.size,index_version=excluded.index_version,status=excluded.status,chapter_count=excluded.chapter_count,updated_at=excluded.updated_at,source_id=excluded.source_id,has_update=excluded.has_update,last_update_check=excluded.last_update_check,cover=excluded.cover,title_pinyin=excluded.title_pinyin",
                    params![b.id, b.title, b.author, b.path, b.source_url, b.source, b.mtime, b.size, b.index_version, b.status, b.chapter_count, b.created_at, b.updated_at, b.source_id, b.has_update, b.last_update_check, b.cover, crate::search::pinyin_text(&b.title)],
                )?;
            }
            for (book_id, titles) in &chapters {
                tx.execute("DELETE FROM chapters WHERE book_id=?1", params![book_id])?;
                for (i, t) in titles.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO chapters(book_id, idx, title, created_at) VALUES(?1,?2,?3,?4)",
                        params![book_id, i as i64, t, now],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn mark_book_status(&self, id: &str, status: &str) -> Result<()> {
        let (id, status) = (id.to_string(), status.to_string());
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute("UPDATE books SET status=?1, updated_at=?2 WHERE id=?3", params![status, now, id])?;
                Ok(())
            })
            .await
    }

    /// Cached books whose update check is due (last check older than
    /// `older_than`, oldest first). Only books that remember their source.
    pub async fn due_update_books(&self, older_than: i64, limit: i64) -> Result<Vec<BookRow>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {BOOK_COLS} FROM books WHERE source='cached' AND source_id<>'' AND source_url IS NOT NULL AND last_update_check<?1 ORDER BY last_update_check ASC LIMIT ?2"
                ))?;
                let rows = stmt
                    .query_map(params![older_than, limit], row_book)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Record that an update check was (or is being) run for this book, so
    /// the periodic worker does not enqueue it again until the next round.
    pub async fn mark_update_checked(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute("UPDATE books SET last_update_check=?1 WHERE id=?2", params![now, id])?;
                Ok(())
            })
            .await
    }

    /// Set/clear the "has unseen new chapters" flag.
    pub async fn set_has_update(&self, id: &str, has: bool) -> Result<()> {
        let id = id.to_string();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute("UPDATE books SET has_update=?1, updated_at=?2 WHERE id=?3", params![has, now, id])?;
                Ok(())
            })
            .await
    }

    pub async fn all_book_paths(&self) -> Result<Vec<(String, String, i64, i64)>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT id, path, mtime, index_version FROM books WHERE source='local' AND path IS NOT NULL")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// All book ids (as u64) for a source kind, for search scope filtering.
    pub async fn book_ids_by_source(&self, source: &str) -> Result<Vec<u64>> {
        let source = source.to_string();
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT id FROM books WHERE source=?1")?;
                let rows = stmt
                    .query_map(params![source], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows.iter().filter_map(|s| s.parse::<u64>().ok()).collect())
            })
            .await
    }

    /// All cached (network) books, for re-indexing from chapter files.
    pub async fn cached_books(&self) -> Result<Vec<(String, String, String)>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT id, title, author FROM books WHERE source='cached'")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Reset index_version for local books (used by rebuild-index so the
    /// incremental scanner picks them up again).
    pub async fn reset_index_versions(&self) -> Result<()> {
        self.call_db(move |c| {
                c.execute("UPDATE books SET index_version=0 WHERE source='local'", [])?;
                Ok(())
            })
            .await
    }

    // ---------- chapters ----------
    pub async fn replace_chapters(&self, book_id: &str, titles: &[String]) -> Result<()> {
        let book_id = book_id.to_string();
        let titles = titles.to_vec();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                let tx = c.transaction()?;
                tx.execute("DELETE FROM chapters WHERE book_id=?1", params![book_id])?;
                for (i, t) in titles.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO chapters(book_id, idx, title, created_at) VALUES(?1,?2,?3,?4)",
                        params![book_id, i as i64, t, now],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
    }

    pub async fn list_chapters(&self, book_id: &str, page: i64, page_size: i64) -> Result<(Vec<ChapterRow>, i64)> {
        let book_id = book_id.to_string();
        let offset = (page.max(1) - 1) * page_size.max(1);
        let limit = page_size.max(1);
        self.call_db(move |c| {
                let total: i64 = c.query_row("SELECT COUNT(*) FROM chapters WHERE book_id=?1", params![book_id], |r| r.get(0))?;
                let mut stmt = c.prepare("SELECT book_id, idx, title FROM chapters WHERE book_id=?1 ORDER BY idx LIMIT ?2 OFFSET ?3")?;
                let rows = stmt
                    .query_map(params![book_id, limit, offset], |r| {
                        Ok(ChapterRow { book_id: r.get(0)?, idx: r.get(1)?, title: r.get(2)? })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((rows, total))
            })
            .await
    }

    pub async fn get_chapter_title(&self, book_id: &str, idx: i64) -> Result<Option<String>> {
        let book_id = book_id.to_string();
        self.call_db(move |c| {
                let row = c
                    .query_row("SELECT title FROM chapters WHERE book_id=?1 AND idx=?2", params![book_id, idx], |r| r.get::<_, String>(0))
                    .optional()?;
                Ok(row)
            })
            .await
    }

    // ---------- sources ----------
    pub async fn list_sources(&self) -> Result<Vec<serde_json::Value>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT raw_json FROM sources ORDER BY custom_order ASC, name ASC")?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut out = vec![];
                for raw in rows {
                    out.push(serde_json::from_str::<serde_json::Value>(&raw).unwrap_or(serde_json::json!({})));
                }
                Ok(out)
            })
            .await
    }

    pub async fn get_source_raw(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_string();
        self.call_db(move |c| {
                let row = c
                    .query_row("SELECT raw_json FROM sources WHERE id=?1", params![id], |r| r.get::<_, String>(0))
                    .optional()?;
                Ok(row)
            })
            .await
    }

    pub async fn upsert_source(&self, raw: &serde_json::Value) -> Result<String> {
        let raw = raw.clone();
        self.call_db(move |c| {
                let id = raw
                    .get("bookSourceUrl")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let get_s = |k: &str| raw.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let get_i = |k: &str| raw.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
                let get_rule = |k: &str| match raw.get(k) {
                    Some(v) if v.is_string() => v.as_str().unwrap().to_string(),
                    Some(v) => serde_json::to_string(v).unwrap_or_default(),
                    None => String::new(),
                };
                let now = chrono::Utc::now().timestamp();
                c.execute(
                    "INSERT INTO sources(id,name,url,group_name,source_type,search_url,explore_url,book_url_pattern,rule_search,rule_explore,rule_book_info,rule_toc,rule_content,js_lib,enabled,weight,last_check_time,custom_order,raw_json,created_at,updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)
                     ON CONFLICT(id) DO UPDATE SET name=excluded.name,url=excluded.url,group_name=excluded.group_name,source_type=excluded.source_type,search_url=excluded.search_url,explore_url=excluded.explore_url,book_url_pattern=excluded.book_url_pattern,rule_search=excluded.rule_search,rule_explore=excluded.rule_explore,rule_book_info=excluded.rule_book_info,rule_toc=excluded.rule_toc,rule_content=excluded.rule_content,js_lib=excluded.js_lib,enabled=excluded.enabled,weight=excluded.weight,last_check_time=excluded.last_check_time,custom_order=excluded.custom_order,raw_json=excluded.raw_json,updated_at=excluded.updated_at",
                    params![
                        id,
                        get_s("bookSourceName"),
                        get_s("bookSourceUrl"),
                        get_s("bookSourceGroup"),
                        get_i("bookSourceType"),
                        get_s("searchUrl"),
                        get_s("exploreUrl"),
                        get_s("bookUrlPattern"),
                        get_rule("ruleSearch"),
                        get_rule("ruleExplore"),
                        get_rule("ruleBookInfo"),
                        get_rule("ruleToc"),
                        get_rule("ruleContent"),
                        get_s("jsLib"),
                        if raw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) { 1 } else { 0 },
                        get_i("weight"),
                        get_i("lastCheckTime"),
                        get_i("customOrder"),
                        serde_json::to_string(&raw).unwrap_or_default(),
                        now,
                        now,
                    ],
                )?;
                Ok(id)
            })
            .await
    }

    pub async fn delete_source(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.call_db(move |c| {
                let n = c.execute("DELETE FROM sources WHERE id=?1", params![id])?;
                Ok(n > 0)
            })
            .await
    }

    /// (id, source, chapter_count) for every book, id order.
    pub async fn list_all_book_ids(&self) -> Result<Vec<(String, String, i64)>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT id, source, chapter_count FROM books ORDER BY id")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// (idx, title) for one book, in chapter order.
    pub async fn list_chapter_titles(&self, book_id: &str) -> Result<Vec<(i64, String)>> {
        let book_id = book_id.to_string();
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT idx, title FROM chapters WHERE book_id=?1 ORDER BY idx")?;
                let rows = stmt
                    .query_map(params![book_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Replace a book's stored chapter issues with a fresh check result.
    pub async fn replace_chapter_issues(&self, book_id: &str, issues: &[ChapterIssue]) -> Result<()> {
        let book_id = book_id.to_string();
        let issues = issues.to_vec();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute("DELETE FROM chapter_issues WHERE book_id=?1", params![book_id])?;
                for iss in &issues {
                    c.execute(
                        "INSERT INTO chapter_issues(book_id,idx,kind,detail,checked_at) VALUES(?1,?2,?3,?4,?5)",
                        params![book_id, iss.idx, iss.kind, iss.detail, now],
                    )?;
                }
                Ok(())
            })
            .await
    }

    /// A book's stored chapter issues (from the last check).
    /// Test-only read-back for the replace round-trip test; production
    /// lists books via `books_with_chapter_issues` and checks live.
    #[cfg(test)]
    pub async fn list_chapter_issues(&self, book_id: &str) -> Result<Vec<ChapterIssue>> {
        let book_id = book_id.to_string();
        self.call_db(move |c| {
                let mut stmt = c.prepare(
                    "SELECT idx, kind, detail FROM chapter_issues WHERE book_id=?1 ORDER BY idx",
                )?;
                let rows = stmt
                    .query_map(params![book_id], |r| {
                        Ok(ChapterIssue {
                            idx: r.get::<_, i64>(0)?,
                            title: String::new(),
                            kind: r.get::<_, String>(1)?,
                            detail: r.get::<_, String>(2)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Books with stored issues: (id, title, author, issue count).
    pub async fn books_with_chapter_issues(&self) -> Result<Vec<(String, String, String, i64)>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare(
                    "SELECT b.id, b.title, b.author, COUNT(*) FROM chapter_issues i \
                     JOIN books b ON b.id = i.book_id \
                     GROUP BY b.id ORDER BY COUNT(*) DESC, b.title ASC",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Search suggestions: books whose title or author contains `q`
    /// (substring, not full-text). Title matches rank before author
    /// matches. Used by the search box dropdown; the full search is
    /// still tantivy (and covers pinyin).
    pub async fn suggest_books(&self, q: &str, limit: i64) -> Result<Vec<BookRow>> {
        let q = q.to_string();
        self.call_db(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {BOOK_COLS} FROM books \
                     WHERE instr(title, ?1) > 0 OR instr(author, ?1) > 0 \
                     ORDER BY CASE WHEN instr(title, ?1) > 0 THEN 0 ELSE 1 END, title COLLATE NOCASE ASC \
                     LIMIT ?2"
                ))?;
                let rows = stmt
                    .query_map(params![q, limit], row_book)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// (author, book count), most books first. Empty authors are skipped.
    pub async fn list_authors(&self) -> Result<Vec<(String, i64)>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare(
                    "SELECT author, COUNT(*) FROM books WHERE author <> '' GROUP BY author ORDER BY COUNT(*) DESC, author ASC",
                )?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Books by one author (exact match), title order.
    pub async fn books_by_author(&self, author: &str) -> Result<Vec<BookRow>> {
        let author = author.to_string();
        self.call_db(move |c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {BOOK_COLS} FROM books WHERE author=?1 ORDER BY title ASC"
                ))?;
                let rows = stmt
                    .query_map(params![author], row_book)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Delete a book. Chapters/bookmarks/progress/tags go with it via
    /// `ON DELETE CASCADE` (foreign_keys is ON). Files and the search
    /// index are the caller's job.
    pub async fn delete_book(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.call_db(move |c| {
                let n = c.execute("DELETE FROM books WHERE id=?1", params![id])?;
                Ok(n > 0)
            })
            .await
    }

    /// Groups of books sharing the exact same title + author, each with
    /// more than one book. Groups are ordered by title; within a group
    /// the most complete copy (most chapters, then largest size) comes
    /// first, so the frontend can default-keep it.
    pub async fn duplicate_groups(&self) -> Result<Vec<DuplicateGroup>> {
        self.call_db(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {BOOK_COLS} FROM books WHERE (title, author) IN \
                 (SELECT title, author FROM books GROUP BY title, author HAVING COUNT(*) > 1) \
                 ORDER BY title ASC, author ASC, chapter_count DESC, size DESC, id ASC"
            ))?;
            let rows = stmt
                .query_map([], row_book)?
                .collect::<Result<Vec<_>, _>>()?;
            let mut groups: Vec<DuplicateGroup> = Vec::new();
            for b in rows {
                match groups.last_mut() {
                    Some(g) if g.title == b.title && g.author == b.author => g.books.push(b),
                    _ => groups.push(DuplicateGroup { title: b.title.clone(), author: b.author.clone(), books: vec![b] }),
                }
            }
            Ok(groups)
        })
        .await
    }

    /// (id, raw_json, enabled) for every source, for the health worker.
    pub async fn list_sources_full(&self) -> Result<Vec<(String, String, bool)>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT id, raw_json, enabled FROM sources ORDER BY custom_order ASC, name ASC")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? != 0)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Health probe result for one source (health, error, last_check_time).
    pub async fn source_health(&self, id: &str) -> Result<Option<(String, String, i64)>> {
        let id = id.to_string();
        self.call_db(move |c| {
                let row = c
                    .query_row(
                        "SELECT health, health_error, last_check_time FROM sources WHERE id=?1",
                        params![id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)),
                    )
                    .optional()?;
                Ok(row)
            })
            .await
    }

    /// Record a health probe result (also refreshes last_check_time).
    pub async fn set_source_health(&self, id: &str, health: &str, error: &str) -> Result<()> {
        let (id, health, error) = (id.to_string(), health.to_string(), error.to_string());
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute(
                    "UPDATE sources SET health=?1, health_error=?2, last_check_time=?3 WHERE id=?4",
                    params![health, error, now, id],
                )?;
                Ok(())
            })
            .await
    }

    /// Distinct subscription URLs across all sources (for periodic refresh).
    pub async fn list_subscriptions(&self) -> Result<Vec<String>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT DISTINCT subscription_url FROM sources WHERE subscription_url<>''")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    /// Record that a source came from (or was refreshed by) a subscription URL.
    pub async fn set_source_subscription(&self, id: &str, url: &str) -> Result<()> {
        let (id, url) = (id.to_string(), url.to_string());
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute(
                    "UPDATE sources SET subscription_url=?1, subscription_updated_at=?2 WHERE id=?3",
                    params![url, now, id],
                )?;
                Ok(())
            })
            .await
    }

    /// (subscription_url, subscription_updated_at) for one source.
    pub async fn get_source_subscription(&self, id: &str) -> Result<Option<(String, i64)>> {
        let id = id.to_string();
        self.call_db(move |c| {
                let row = c
                    .query_row(
                        "SELECT subscription_url, subscription_updated_at FROM sources WHERE id=?1",
                        params![id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                Ok(row)
            })
            .await
    }

    // ---------- source_cache ----------
    pub async fn cache_get(&self, url_hash: &str, now: i64) -> Result<Option<(String, String)>> {
        let url_hash = url_hash.to_string();
        self.call_db(move |c| {
                let row = c
                    .query_row(
                        "SELECT url, body FROM source_cache WHERE url_hash=?1 AND fetched_at + ttl_seconds > ?2",
                        params![url_hash, now],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?;
                Ok(row)
            })
            .await
    }

    pub async fn cache_put(&self, url_hash: &str, url: &str, kind: &str, body: &str, ttl_seconds: i64) -> Result<()> {
        let (url_hash, url, kind, body) = (url_hash.to_string(), url.to_string(), kind.to_string(), body.to_string());
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute(
                    "INSERT INTO source_cache(url_hash,url,kind,body,fetched_at,ttl_seconds) VALUES(?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(url_hash) DO UPDATE SET url=excluded.url,kind=excluded.kind,body=excluded.body,fetched_at=excluded.fetched_at,ttl_seconds=excluded.ttl_seconds",
                    params![url_hash, url, kind, body, now, ttl_seconds],
                )?;
                Ok(())
            })
            .await
    }

    /// (rows, total body bytes) currently in source_cache.
    pub async fn cache_stats(&self) -> Result<(i64, i64)> {
        self.call_db(move |c| {
                c.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(body AS BLOB))), 0) FROM source_cache",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                )
            })
            .await
    }

    /// Delete expired rows, then enforce the row/byte caps by dropping
    /// the oldest rows first. Returns (deleted_expired, deleted_overflow).
    pub async fn prune_source_cache(&self, max_rows: i64, max_bytes: i64) -> Result<(i64, i64)> {
        self.call_db(move |c| {
                let now = chrono::Utc::now().timestamp();
                let expired = c.execute(
                    "DELETE FROM source_cache WHERE fetched_at + ttl_seconds <= ?1",
                    params![now],
                )? as i64;
                let mut overflow = 0i64;
                loop {
                    let (rows, bytes): (i64, i64) = c.query_row(
                        "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(body AS BLOB))), 0) FROM source_cache",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    if rows <= max_rows && bytes <= max_bytes {
                        break;
                    }
                    // Delete only the excess rows. When only the byte
                    // cap is over, estimate the count from the average
                    // body size (the loop re-checks and corrects).
                    let n = if rows > max_rows {
                        rows - max_rows
                    } else {
                        let avg = (bytes / rows).max(1);
                        ((bytes - max_bytes) / avg).max(1)
                    };
                    let deleted = c.execute(
                        "DELETE FROM source_cache WHERE id IN \
                         (SELECT id FROM source_cache ORDER BY fetched_at ASC, id ASC LIMIT ?1)",
                        params![n],
                    )? as i64;
                    overflow += deleted;
                    if deleted == 0 {
                        break;
                    }
                }
                Ok((expired, overflow))
            })
            .await
    }

    // ---------- tasks ----------
    pub async fn enqueue_task(&self, table: &str, r#type: &str, payload: &str) -> Result<String> {
        let (table, r#type, payload) = (table.to_string(), r#type.to_string(), payload.to_string());
        let id = uuid::Uuid::new_v4().to_string();
        let id2 = id.clone();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                let sql = format!(
                    "INSERT INTO {table}(id,type,status,payload,attempts,next_retry_at,error,created_at,updated_at) VALUES(?1,?2,'pending',?3,0,0,'',?4,?5)"
                );
                c.execute(&sql, params![id2, r#type, payload, now, now])?;
                Ok(())
            })
            .await?;
        Ok(id)
    }

    /// Atomically claim one pending task whose retry time has come.
    pub async fn claim_task(&self, table: &str) -> Result<Option<TaskRow>> {
        let table = table.to_string();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                let row: Option<TaskRow> = c
                    .query_row(
                        &format!("SELECT id,type,status,payload,attempts,next_retry_at,error,created_at,updated_at FROM {table} WHERE status='pending' AND next_retry_at<=?1 ORDER BY created_at ASC LIMIT 1"),
                        params![now],
                        row_task,
                    )
                    .optional()?;
                if let Some(t) = &row {
                    c.execute(
                        &format!("UPDATE {table} SET status='running', updated_at=?1 WHERE id=?2 AND status='pending'"),
                        params![now, t.id],
                    )?;
                }
                Ok(row)
            })
            .await
    }

    pub async fn finish_task(&self, table: &str, id: &str, ok: bool, error: &str) -> Result<()> {
        let (table, id, error) = (table.to_string(), id.to_string(), error.to_string());
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                if ok {
                    c.execute(
                        &format!("UPDATE {table} SET status='done', error='', updated_at=?1 WHERE id=?2"),
                        params![now, id],
                    )?;
                } else {
                    let attempts: i64 = c.query_row(&format!("SELECT attempts FROM {table} WHERE id=?1"), params![id], |r| r.get(0)).unwrap_or(0);
                    let backoff = 60 * 2_i64.pow(attempts.min(6) as u32);
                    c.execute(
                        &format!("UPDATE {table} SET status='failed', attempts=attempts+1, next_retry_at=?1, error=?2, updated_at=?3 WHERE id=?4"),
                        params![now + backoff, error, now, id],
                    )?;
                }
                Ok(())
            })
            .await
    }

    pub async fn set_task_status(&self, table: &str, id: &str, status: &str) -> Result<bool> {
        let (table, id, status) = (table.to_string(), id.to_string(), status.to_string());
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                let n = c.execute(
                    &format!("UPDATE {table} SET status=?1, updated_at=?2 WHERE id=?3"),
                    params![status, now, id],
                )?;
                Ok(n > 0)
            })
            .await
    }

    pub async fn list_tasks(&self, table: &str, status: Option<String>, page: i64, page_size: i64) -> Result<(Vec<TaskRow>, i64)> {
        let table = table.to_string();
        let offset = (page.max(1) - 1) * page_size.max(1);
        let limit = page_size.max(1);
        self.call_db(move |c| {
                let (where_sql, args): (String, Vec<String>) = match &status {
                    Some(s) if !s.is_empty() => ("WHERE status=?1".to_string(), vec![s.clone()]),
                    _ => ("".to_string(), vec![]),
                };
                let total: i64 = c.query_row(
                    &format!("SELECT COUNT(*) FROM {table} {where_sql}"),
                    rusqlite::params_from_iter(args.iter()),
                    |r| r.get(0),
                )?;
                let mut stmt = c.prepare(&format!(
                    "SELECT id,type,status,payload,attempts,next_retry_at,error,created_at,updated_at FROM {table} {where_sql} ORDER BY created_at DESC LIMIT {limit} OFFSET {offset}"
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(args.iter()), row_task)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((rows, total))
            })
            .await
    }

    pub async fn task_counts(&self, table: &str) -> Result<Vec<(String, i64)>> {
        let table = table.to_string();
        self.call_db(move |c| {
                let mut stmt = c.prepare(&format!("SELECT status, COUNT(*) FROM {table} GROUP BY status"))?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    // ---------- progress / bookmarks / tags ----------
    pub async fn get_progress(&self, book_id: &str) -> Result<Option<(i64, i64)>> {
        let book_id = book_id.to_string();
        self.call_db(move |c| {
                let row = c
                    .query_row(
                        "SELECT chapter_idx, offset FROM reading_progress WHERE book_id=?1",
                        params![book_id],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                Ok(row)
            })
            .await
    }

    pub async fn put_progress(&self, book_id: &str, chapter_idx: i64, offset: i64) -> Result<()> {
        let book_id = book_id.to_string();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute(
                    "INSERT INTO reading_progress(book_id,chapter_idx,offset,updated_at) VALUES(?1,?2,?3,?4)
                     ON CONFLICT(book_id) DO UPDATE SET chapter_idx=excluded.chapter_idx,offset=excluded.offset,updated_at=excluded.updated_at",
                    params![book_id, chapter_idx, offset, now],
                )?;
                Ok(())
            })
            .await
    }

    /// Add seconds to a book's accumulated reading time (called by the
    /// reader with small deltas). Non-positive deltas are ignored.
    pub async fn add_reading_seconds(&self, book_id: &str, seconds: i64) -> Result<()> {
        if seconds <= 0 {
            return Ok(());
        }
        let book_id = book_id.to_string();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute(
                    "INSERT INTO reading_seconds(book_id,seconds,updated_at) VALUES(?1,?2,?3)
                     ON CONFLICT(book_id) DO UPDATE SET seconds=reading_seconds.seconds+excluded.seconds,updated_at=excluded.updated_at",
                    params![book_id, seconds, now],
                )?;
                Ok(())
            })
            .await
    }

    /// Aggregate reading stats. "Finished" = progress reached the last
    /// chapter (chapter_idx >= chapter_count - 1); its updated_at is the
    /// finish time. Reading time only exists for sessions the reader
    /// actually reported, so total_seconds is a lower bound.
    pub async fn reading_stats(&self) -> Result<ReadingStats> {
        self.call_db(move |c| {
            let total_books: i64 = c.query_row("SELECT COUNT(*) FROM books", [], |r| r.get(0))?;
            let reading_books: i64 = c.query_row("SELECT COUNT(*) FROM reading_progress", [], |r| r.get(0))?;
            let finished_sql = "FROM reading_progress p JOIN books b ON b.id = p.book_id \
                                WHERE b.chapter_count > 0 AND p.chapter_idx >= b.chapter_count - 1";
            let finished_books: i64 = c.query_row(&format!("SELECT COUNT(*) {finished_sql}"), [], |r| r.get(0))?;
            let total_seconds: i64 = c.query_row("SELECT COALESCE(SUM(seconds), 0) FROM reading_seconds", [], |r| r.get(0))?;
            let active_days: i64 = c.query_row(
                "SELECT COUNT(DISTINCT date(updated_at, 'unixepoch')) FROM reading_progress",
                [], |r| r.get(0),
            )?;
            let mut stmt = c.prepare(&format!(
                "SELECT strftime('%Y-%m', p.updated_at, 'unixepoch'), COUNT(*) {finished_sql} \
                 GROUP BY 1 ORDER BY 1 DESC LIMIT 12"
            ))?;
            let finished_by_month = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut stmt = c.prepare(&format!(
                "SELECT b.id, b.title, b.author, p.updated_at {finished_sql} \
                 ORDER BY p.updated_at DESC LIMIT 10"
            ))?;
            let recent_finished = stmt.query_map([], |r| Ok((
                r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?,
            )))?.collect::<Result<Vec<_>, _>>()?;
            let mut stmt = c.prepare(
                "SELECT b.id, b.title, b.author, s.seconds FROM reading_seconds s \
                 JOIN books b ON b.id = s.book_id ORDER BY s.seconds DESC LIMIT 10",
            )?;
            let top_books_by_time = stmt.query_map([], |r| Ok((
                r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?,
            )))?.collect::<Result<Vec<_>, _>>()?;
            Ok(ReadingStats {
                total_books, reading_books, finished_books, total_seconds, active_days,
                finished_by_month, recent_finished, top_books_by_time,
            })
        })
        .await
    }

    pub async fn list_bookmarks(&self, book_id: &str) -> Result<Vec<serde_json::Value>> {
        let book_id = book_id.to_string();
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT id, chapter_idx, offset, note, created_at FROM bookmarks WHERE book_id=?1 ORDER BY chapter_idx, offset")?;
                let rows = stmt
                    .query_map(params![book_id], |r| {
                        Ok(serde_json::json!({
                            "id": r.get::<_, String>(0)?,
                            "book_id": book_id,
                            "chapter_idx": r.get::<_, i64>(1)?,
                            "offset": r.get::<_, i64>(2)?,
                            "note": r.get::<_, String>(3)?,
                            "created_at": r.get::<_, i64>(4)?,
                        }))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    pub async fn add_bookmark(&self, book_id: &str, chapter_idx: i64, offset: i64, note: &str) -> Result<String> {
        let (book_id, note) = (book_id.to_string(), note.to_string());
        let id = uuid::Uuid::new_v4().to_string();
        let id2 = id.clone();
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute(
                    "INSERT INTO bookmarks(id,book_id,chapter_idx,offset,note,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id2, book_id, chapter_idx, offset, note, now],
                )?;
                Ok(())
            })
            .await?;
        Ok(id)
    }

    pub async fn delete_bookmark(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.call_db(move |c| {
                let n = c.execute("DELETE FROM bookmarks WHERE id=?1", params![id])?;
                Ok(n > 0)
            })
            .await
    }

    pub async fn list_tags(&self, book_id: &str) -> Result<Vec<String>> {
        let book_id = book_id.to_string();
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT tag FROM tags WHERE book_id=?1 ORDER BY tag")?;
                let rows = stmt
                    .query_map(params![book_id], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    pub async fn add_tag(&self, book_id: &str, tag: &str) -> Result<()> {
        let (book_id, tag) = (book_id.to_string(), tag.to_string());
        let now = chrono::Utc::now().timestamp();
        self.call_db(move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO tags(book_id,tag,created_at) VALUES(?1,?2,?3)",
                    params![book_id, tag, now],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn delete_tag(&self, book_id: &str, tag: &str) -> Result<bool> {
        let (book_id, tag) = (book_id.to_string(), tag.to_string());
        self.call_db(move |c| {
                let n = c.execute("DELETE FROM tags WHERE book_id=?1 AND tag=?2", params![book_id, tag])?;
                Ok(n > 0)
            })
            .await
    }

    pub async fn all_tags(&self) -> Result<Vec<String>> {
        self.call_db(move |c| {
                let mut stmt = c.prepare("SELECT DISTINCT tag FROM tags ORDER BY tag")?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }
}

fn row_book(r: &rusqlite::Row) -> rusqlite::Result<BookRow> {
    Ok(BookRow {
        id: r.get(0)?,
        title: r.get(1)?,
        author: r.get(2)?,
        path: r.get(3)?,
        source_url: r.get(4)?,
        source: r.get(5)?,
        mtime: r.get(6)?,
        size: r.get(7)?,
        index_version: r.get(8)?,
        status: r.get(9)?,
        chapter_count: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
        source_id: r.get(13)?,
        has_update: r.get(14)?,
        last_update_check: r.get(15)?,
        cover: r.get(16)?,
    })
}

fn row_task(r: &rusqlite::Row) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: r.get(0)?,
        r#type: r.get(1)?,
        status: r.get(2)?,
        payload: r.get(3)?,
        attempts: r.get(4)?,
        next_retry_at: r.get(5)?,
        error: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_create_nine_tables() {
        let db = Database::open_in_memory().await.unwrap();
        let tables: Vec<String> = db
            .call_db(|c| {
                let mut stmt = c.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap();
        for t in ["books", "chapters", "sources", "source_cache", "scan_tasks", "crawl_tasks", "bookmarks", "reading_progress", "tags"] {
            assert!(tables.iter().any(|x| x == t), "missing table {t}");
        }
    }

    #[tokio::test]
    async fn suggest_books_ranks_title_before_author() {
        let db = Database::open_in_memory().await.unwrap();
        let now = chrono::Utc::now().timestamp();
        let mk = |id: &str, title: &str, author: &str| BookRow {
            id: id.into(), title: title.into(), author: author.into(), path: None,
            source_url: None, source: "local".into(),
            mtime: now, size: 0, index_version: 1, status: "ok".into(), chapter_count: 1,
            created_at: now, updated_at: now, source_id: String::new(),
            has_update: false, last_update_check: 0, cover: String::new(),
        };
        db.upsert_book(&mk("1", "另一本", "修真者")).await.unwrap();
        db.upsert_book(&mk("2", "修仙传", "忘语")).await.unwrap();
        db.upsert_book(&mk("3", "无关", "别人")).await.unwrap();
        let got = db.suggest_books("修", 8).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "2", "title match ranks first");
        assert_eq!(got[1].id, "1", "author match second");
        assert!(db.suggest_books("不存在", 8).await.unwrap().is_empty());
        let got = db.suggest_books("修", 1).await.unwrap();
        assert_eq!(got.len(), 1);
    }

    #[tokio::test]
    async fn list_books_matches_pinyin_initials() {
        let db = Database::open_in_memory().await.unwrap();
        let now = chrono::Utc::now().timestamp();
        let mk = |id: &str, title: &str| BookRow {
            id: id.into(), title: title.into(), author: "".into(), path: None,
            source_url: None, source: "local".into(),
            mtime: now, size: 0, index_version: 1, status: "ok".into(), chapter_count: 1,
            created_at: now, updated_at: now, source_id: String::new(),
            has_update: false, last_update_check: 0, cover: String::new(),
        };
        db.upsert_book(&mk("1", "武林沉沦")).await.unwrap();
        db.upsert_book(&mk("2", "修仙传")).await.unwrap();
        db.upsert_book(&mk("3", "另一本")).await.unwrap();
        // Full pinyin initials: wlcl = 武林沉沦.
        let (items, total) = db.list_books(Some("wlcl".into()), None, None, "title", 1, 50).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].id, "1");
        // Partial initials prefix.
        let (_, total) = db.list_books(Some("wl".into()), None, None, "title", 1, 50).await.unwrap();
        assert_eq!(total, 1);
        // Concatenated pinyin.
        let (_, total) = db.list_books(Some("wulin".into()), None, None, "title", 1, 50).await.unwrap();
        assert_eq!(total, 1);
        // Uppercase initials (case-insensitive pinyin match).
        let (_, total) = db.list_books(Some("WLCL".into()), None, None, "title", 1, 50).await.unwrap();
        assert_eq!(total, 1);
        // Literal title still works.
        let (_, total) = db.list_books(Some("武林".into()), None, None, "title", 1, 50).await.unwrap();
        assert_eq!(total, 1);
        // Unrelated pinyin matches nothing.
        let (_, total) = db.list_books(Some("zz".into()), None, None, "title", 1, 50).await.unwrap();
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn chapter_issues_round_trip() {
        let db = Database::open_in_memory().await.unwrap();
        let now = chrono::Utc::now().timestamp();
        db.upsert_book(&BookRow {
            id: "1".into(), title: "书".into(), author: "".into(), path: None,
            source_url: None, source: "local".into(),
            mtime: now, size: 0, index_version: 1, status: "ok".into(), chapter_count: 5,
            created_at: now, updated_at: now, source_id: String::new(),
            has_update: false, last_update_check: 0, cover: String::new(),
        })
        .await
        .unwrap();
        let issues = vec![ChapterIssue {
            idx: 4,
            title: "第五章".into(),
            kind: "gap".into(),
            detail: "缺第 3–4 章".into(),
        }];
        db.replace_chapter_issues("1", &issues).await.unwrap();
        let got = db.list_chapter_issues("1").await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "gap");
        assert_eq!(got[0].detail, "缺第 3–4 章");
        let books = db.books_with_chapter_issues().await.unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].3, 1);
        db.replace_chapter_issues("1", &[]).await.unwrap();
        assert!(db.books_with_chapter_issues().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prune_source_cache_drops_expired_and_oldest_over_cap() {
        let db = Database::open_in_memory().await.unwrap();
        db.cache_put("h1", "https://a/1", "search", "fresh-body", 3600).await.unwrap();
        db.cache_put("h2", "https://a/2", "search", "old-body", 3600).await.unwrap();
        // Backdate h2 past its TTL.
        db.call_db(|c| {
                c.execute("UPDATE source_cache SET fetched_at = fetched_at - 100000 WHERE url_hash='h2'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let (expired, overflow) = db.prune_source_cache(100, 1_000_000).await.unwrap();
        assert_eq!(expired, 1, "expired row must go");
        assert_eq!(overflow, 0);
        let now = chrono::Utc::now().timestamp();
        assert!(db.cache_get("h1", now).await.unwrap().is_some());
        assert!(db.cache_get("h2", now).await.unwrap().is_none());
        // Row cap: 5 rows, cap 3 -> oldest 2 go.
        for i in 0..4 {
            db.cache_put(&format!("hx{i}"), &format!("https://a/x{i}"), "search", "0123456789", 3600)
                .await
                .unwrap();
        }
        let (rows, _) = db.cache_stats().await.unwrap();
        assert_eq!(rows, 5);
        let (_, overflow) = db.prune_source_cache(3, 1_000_000).await.unwrap();
        assert_eq!(overflow, 2, "oldest rows over the cap must go");
        let (rows, bytes) = db.cache_stats().await.unwrap();
        assert_eq!(rows, 3);
        assert_eq!(bytes, 30, "3 rows x 10 bytes");
        // Byte cap: 30 bytes, cap 25 -> oldest row goes.
        let (_, overflow) = db.prune_source_cache(100, 25).await.unwrap();
        assert_eq!(overflow, 1);
        let (_, bytes) = db.cache_stats().await.unwrap();
        assert!(bytes <= 25, "bytes must be under cap: {bytes}");
    }

    #[tokio::test]
    async fn reading_stats_counts_finished_and_time() {
        let db = Database::open_in_memory().await.unwrap();
        let now = chrono::Utc::now().timestamp();
        let mk = |id: &str, title: &str, chapters: i64| BookRow {
            id: id.into(), title: title.into(), author: "".into(), path: None,
            source_url: None, source: "local".into(),
            mtime: now, size: 0, index_version: 1, status: "ok".into(), chapter_count: chapters,
            created_at: now, updated_at: now, source_id: String::new(),
            has_update: false, last_update_check: 0, cover: String::new(),
        };
        db.upsert_book(&mk("1", "读完的书", 5)).await.unwrap();
        db.upsert_book(&mk("2", "在读的书", 10)).await.unwrap();
        db.upsert_book(&mk("3", "没读的书", 3)).await.unwrap();
        db.put_progress("1", 4, 0).await.unwrap(); // last chapter -> finished
        db.put_progress("2", 2, 0).await.unwrap(); // not finished
        db.add_reading_seconds("1", 3600).await.unwrap();
        db.add_reading_seconds("2", 600).await.unwrap();
        db.add_reading_seconds("1", 60).await.unwrap();
        db.add_reading_seconds("3", -5).await.unwrap(); // ignored
        let st = db.reading_stats().await.unwrap();
        assert_eq!(st.total_books, 3);
        assert_eq!(st.reading_books, 2);
        assert_eq!(st.finished_books, 1);
        assert_eq!(st.total_seconds, 4260);
        assert_eq!(st.active_days, 1);
        assert_eq!(st.recent_finished.len(), 1);
        assert_eq!(st.recent_finished[0].1, "读完的书");
        assert_eq!(st.top_books_by_time[0].1, "读完的书");
        assert_eq!(st.top_books_by_time[0].3, 3660);
        assert!(!st.finished_by_month.is_empty(), "finished month must be listed");
    }

    #[tokio::test]
    async fn duplicate_groups_and_delete_book() {
        let db = Database::open_in_memory().await.unwrap();
        let now = chrono::Utc::now().timestamp();
        let mk = |id: &str, title: &str, author: &str, chapters: i64| BookRow {
            id: id.into(), title: title.into(), author: author.into(), path: None,
            source_url: None, source: "local".into(),
            mtime: now, size: chapters * 100, index_version: 1, status: "ok".into(), chapter_count: chapters,
            created_at: now, updated_at: now, source_id: String::new(),
            has_update: false, last_update_check: 0, cover: String::new(),
        };
        for (id, t, a, n) in [
            ("1", "修仙传", "忘语", 10),
            ("2", "修仙传", "忘语", 4),
            ("3", "修仙传", "别人", 7),
            ("4", "另一本", "忘语", 2),
        ] {
            db.upsert_book(&mk(id, t, a, n)).await.unwrap();
        }
        // Chapters + a bookmark on book 1, to prove the cascade.
        db.replace_chapters("1", &["第一章".to_string()]).await.unwrap();
        db.add_bookmark("1", 0, 0, "").await.unwrap();
        let groups = db.duplicate_groups().await.unwrap();
        assert_eq!(groups.len(), 1, "only 修仙传/忘语 is duplicated");
        assert_eq!(groups[0].title, "修仙传");
        assert_eq!(groups[0].books.len(), 2);
        // Most chapters first, so the frontend can default-keep it.
        assert_eq!(groups[0].books[0].id, "1");
        assert_eq!(groups[0].books[1].id, "2");

        assert!(db.delete_book("2").await.unwrap());
        assert!(db.get_book("2").await.unwrap().is_none());
        assert!(db.duplicate_groups().await.unwrap().is_empty(), "group is gone after deleting the extra copy");
        assert!(!db.delete_book("2").await.unwrap(), "deleting twice returns false");

        assert!(db.delete_book("1").await.unwrap());
        assert!(db.list_chapters("1", 1, 10).await.unwrap().0.is_empty(), "chapters cascade");
        assert!(db.list_bookmarks("1").await.unwrap().is_empty(), "bookmarks cascade");
        assert!(db.get_book("3").await.unwrap().is_some(), "other books untouched");
    }

    #[tokio::test]
    async fn v2_columns_and_update_due() {
        let db = Database::open_in_memory().await.unwrap();
        // v2 columns exist on a fresh migrate.
        let cols: Vec<String> = db
            .call_db(|c| {
                let mut stmt = c.prepare("SELECT name FROM pragma_table_info('books')")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap();
        for col in ["source_id", "has_update", "last_update_check"] {
            assert!(cols.iter().any(|x| x == col), "missing column {col}");
        }
        let scols: Vec<String> = db
            .call_db(|c| {
                let mut stmt = c.prepare("SELECT name FROM pragma_table_info('sources')")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap();
        for col in ["health", "health_error"] {
            assert!(scols.iter().any(|x| x == col), "missing column {col}");
        }
        let now = chrono::Utc::now().timestamp();
        db.upsert_book(&BookRow {
            id: "1".into(), title: "缓存书".into(), author: "".into(), path: None,
            source_url: Some("https://example.com/book/1".into()), source: "cached".into(),
            mtime: now, size: 0, index_version: 1, status: "ok".into(), chapter_count: 3,
            created_at: now, updated_at: now, source_id: "https://example.com".into(),
            has_update: false, last_update_check: 0, cover: String::new(),
        })
        .await
        .unwrap();
        let due = db.due_update_books(now - 3600, 10).await.unwrap();
        assert_eq!(due.len(), 1, "last_update_check=0 must be due");
        assert_eq!(due[0].source_id, "https://example.com");
        db.mark_update_checked("1").await.unwrap();
        assert!(db.due_update_books(now - 3600, 10).await.unwrap().is_empty());
        db.set_has_update("1", true).await.unwrap();
        assert!(db.get_book("1").await.unwrap().unwrap().has_update);
        db.set_has_update("1", false).await.unwrap();
        assert!(!db.get_book("1").await.unwrap().unwrap().has_update);
    }
}
