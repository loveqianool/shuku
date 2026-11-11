-- legado-server schema v1: metadata / tasks / sources / cache / progress only.
-- Chapter TEXT is never stored here (parsed on demand from files, or stored
-- as per-chapter files under data/chapters/ for cached network books).

CREATE TABLE IF NOT EXISTS books (
    id            TEXT PRIMARY KEY,
    title         TEXT NOT NULL,
    author        TEXT NOT NULL DEFAULT '',
    path          TEXT UNIQUE,
    source_url    TEXT,
    source        TEXT NOT NULL DEFAULT 'local',  -- local | cached
    mtime         INTEGER NOT NULL DEFAULT 0,     -- source file mtime (secs)
    size          INTEGER NOT NULL DEFAULT 0,
    index_version INTEGER NOT NULL DEFAULT 0,     -- 0 = not indexed
    status        TEXT NOT NULL DEFAULT 'ok',     -- ok | broken | indexing
    chapter_count INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_books_updated ON books(updated_at);
CREATE INDEX IF NOT EXISTS idx_books_title ON books(title);
CREATE INDEX IF NOT EXISTS idx_books_author ON books(author);

CREATE TABLE IF NOT EXISTS chapters (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id    TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    idx        INTEGER NOT NULL,
    title      TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    UNIQUE(book_id, idx)
);
CREATE INDEX IF NOT EXISTS idx_chapters_book ON chapters(book_id, idx);

CREATE TABLE IF NOT EXISTS sources (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    url              TEXT NOT NULL DEFAULT '',
    group_name       TEXT NOT NULL DEFAULT '',
    source_type      INTEGER NOT NULL DEFAULT 0,
    search_url       TEXT NOT NULL DEFAULT '',
    explore_url      TEXT NOT NULL DEFAULT '',
    book_url_pattern TEXT NOT NULL DEFAULT '',
    rule_search      TEXT NOT NULL DEFAULT '',
    rule_explore     TEXT NOT NULL DEFAULT '',
    rule_book_info   TEXT NOT NULL DEFAULT '',
    rule_toc         TEXT NOT NULL DEFAULT '',
    rule_content     TEXT NOT NULL DEFAULT '',
    js_lib           TEXT NOT NULL DEFAULT '',
    enabled          INTEGER NOT NULL DEFAULT 1,
    weight           INTEGER NOT NULL DEFAULT 0,
    last_check_time  INTEGER NOT NULL DEFAULT 0,
    custom_order     INTEGER NOT NULL DEFAULT 0,
    raw_json         TEXT NOT NULL DEFAULT '{}',
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS source_cache (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    url_hash     TEXT NOT NULL UNIQUE,
    url          TEXT NOT NULL,
    kind         TEXT NOT NULL DEFAULT 'search',  -- search | toc | content
    body         TEXT NOT NULL,
    fetched_at   INTEGER NOT NULL,
    ttl_seconds  INTEGER NOT NULL DEFAULT 60
);
CREATE INDEX IF NOT EXISTS idx_cache_hash ON source_cache(url_hash);

CREATE TABLE IF NOT EXISTS scan_tasks (
    id            TEXT PRIMARY KEY,
    type          TEXT NOT NULL DEFAULT 'scan',   -- scan | rebuild_index
    status        TEXT NOT NULL DEFAULT 'pending',-- pending | running | done | failed | paused
    payload       TEXT NOT NULL DEFAULT '{}',
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER NOT NULL DEFAULT 0,
    error         TEXT NOT NULL DEFAULT '',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS crawl_tasks (
    id            TEXT PRIMARY KEY,
    type          TEXT NOT NULL DEFAULT 'crawl',  -- crawl | toc | content
    status        TEXT NOT NULL DEFAULT 'pending',
    payload       TEXT NOT NULL DEFAULT '{}',
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER NOT NULL DEFAULT 0,
    error         TEXT NOT NULL DEFAULT '',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS bookmarks (
    id          TEXT PRIMARY KEY,
    book_id     TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    chapter_idx INTEGER NOT NULL DEFAULT 0,
    offset      INTEGER NOT NULL DEFAULT 0,
    note        TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_bookmarks_book ON bookmarks(book_id);

CREATE TABLE IF NOT EXISTS reading_progress (
    book_id     TEXT PRIMARY KEY REFERENCES books(id) ON DELETE CASCADE,
    chapter_idx INTEGER NOT NULL DEFAULT 0,
    offset      INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tags (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    book_id    TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    tag        TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(book_id, tag)
);
CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag);
