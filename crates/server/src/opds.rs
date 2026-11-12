//! OPDS 1.2 catalog (Atom) so external readers (KOReader, 静读天下,
//! Apple Books, …) can browse and download the library. Read-only.
//!
//! Routes (registered in `api::router`):
//!   /opds/                     navigation: all / by author / recent / search
//!   /opds/books?page=N         acquisition feed, 50 per page, title order
//!   /opds/authors              navigation feed of authors
//!   /opds/authors/{author}     acquisition feed for one author
//!   /opds/recent               acquisition feed, newest first
//!   /opds/search?q=...         acquisition feed, title/author substring
//!   /opds/opensearch.xml       OpenSearch description
//!   /opds/download/{id}        the book file (local file, or a combined
//!                               .txt built from the cached chapter files)

use crate::api::{ApiResult, AppState};
use crate::db::BookRow;
use anyhow::anyhow;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

pub const PAGE_SIZE: i64 = 50;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/opds/", get(root))
        .route("/opds/books", get(books))
        .route("/opds/authors", get(authors))
        .route("/opds/authors/{author}", get(author_books))
        .route("/opds/recent", get(recent))
        .route("/opds/search", get(search))
        .route("/opds/opensearch.xml", get(opensearch))
        .route("/opds/download/{id}", get(download))
}

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn rfc3339(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

fn feed_open(title: &str, self_href: &str, kind: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\" \
         xmlns:opds=\"http://opds-spec.org/2010/catalog\">\n<title>{}</title>\n\
         <id>urn:shuku:opds{}</id>\n<updated>{}</updated>\n\
         <link rel=\"self\" type=\"application/atom+xml;profile=opds-catalog;kind={}\" href=\"{}\"/>\n\
         <link rel=\"start\" type=\"application/atom+xml;profile=opds-catalog;kind=navigation\" href=\"/opds/\"/>\n",
        xml_escape(title),
        xml_escape(self_href),
        rfc3339(chrono::Utc::now().timestamp()),
        kind,
        xml_escape(self_href),
    )
}

fn nav_entry(title: &str, href: &str) -> String {
    format!(
        "<entry><title>{}</title><id>urn:shuku:opds{}</id><updated>{}</updated>\
         <link rel=\"subsection\" type=\"application/atom+xml;profile=opds-catalog;kind=navigation\" href=\"{}\"/>\
         <content type=\"text\">{}</content></entry>\n",
        xml_escape(title),
        xml_escape(href),
        rfc3339(chrono::Utc::now().timestamp()),
        xml_escape(href),
        xml_escape(title),
    )
}

/// Navigation feed: a list of (title, href) subsections.
pub fn navigation_feed(title: &str, self_href: &str, links: &[(String, String)]) -> String {
    let mut s = feed_open(title, self_href, "navigation");
    for (t, h) in links {
        s.push_str(&nav_entry(t, h));
    }
    s.push_str("</feed>\n");
    s
}

fn book_mime(book: &BookRow) -> &'static str {
    match book.path.as_deref().unwrap_or("") {
        p if p.ends_with(".epub") => "application/epub+zip",
        _ => "text/plain; charset=utf-8",
    }
}

fn book_entry(book: &BookRow) -> String {
    let mut s = String::new();
    s.push_str("<entry>\n");
    s.push_str(&format!("<title>{}</title>\n", xml_escape(&book.title)));
    s.push_str(&format!("<id>urn:shuku:book:{}</id>\n", xml_escape(&book.id)));
    if !book.author.is_empty() {
        s.push_str(&format!(
            "<author><name>{}</name></author>\n",
            xml_escape(&book.author)
        ));
    }
    s.push_str(&format!("<updated>{}</updated>\n", rfc3339(book.updated_at)));
    s.push_str(&format!(
        "<link rel=\"http://opds-spec.org/acquisition\" type=\"{}\" href=\"/opds/download/{}\"/>\n",
        book_mime(book),
        xml_escape(&book.id),
    ));
    s.push_str("</entry>\n");
    s
}

/// Acquisition feed: a page of books, with first/next/last pagination.
pub fn acquisition_feed(
    title: &str,
    self_href: &str,
    books: &[BookRow],
    total: i64,
    page: i64,
    page_size: i64,
) -> String {
    let mut s = feed_open(title, self_href, "acquisition");
    s.push_str(&format!(
        "<opds:totalResults>{}</opds:totalResults>\n<opds:itemsPerPage>{}</opds:itemsPerPage>\n",
        total, page_size
    ));
    let base = self_href.split('?').next().unwrap_or(self_href);
    let last_page = (total.max(0) + page_size - 1) / page_size;
    let link = |rel: &str, p: i64| {
        format!(
            "<link rel=\"{}\" type=\"application/atom+xml;profile=opds-catalog;kind=acquisition\" href=\"{}?page={}\"/>\n",
            rel, base, p
        )
    };
    if page > 1 {
        s.push_str(&link("first", 1));
        s.push_str(&link("previous", page - 1));
    }
    if (page as i64) < last_page {
        s.push_str(&link("next", page + 1));
        s.push_str(&link("last", last_page.max(1)));
    }
    for b in books {
        s.push_str(&book_entry(b));
    }
    s.push_str("</feed>\n");
    s
}

fn atom(content_type: &str, body: String) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type.to_string())], body)
}

const NAV_CT: &str = "application/atom+xml;profile=opds-catalog;kind=navigation";
const ACQ_CT: &str = "application/atom+xml;profile=opds-catalog;kind=acquisition";

async fn root() -> impl IntoResponse {
    atom(
        NAV_CT,
        navigation_feed(
            "书库",
            "/opds/",
            &[
                ("全部图书".into(), "/opds/books".into()),
                ("按作者".into(), "/opds/authors".into()),
                ("最新入库".into(), "/opds/recent".into()),
            ],
        ),
    )
}

#[derive(Debug, Deserialize, Default)]
pub struct PageQuery {
    pub page: Option<i64>,
}

fn page_of(q: &PageQuery) -> i64 {
    q.page.unwrap_or(1).max(1)
}

async fn books(State(s): State<AppState>, Query(q): Query<PageQuery>) -> ApiResult<impl IntoResponse> {
    let page = page_of(&q);
    let (items, total) = s
        .db
        .list_books(None, None, None, "title", page, PAGE_SIZE)
        .await?;
    Ok(atom(
        ACQ_CT,
        acquisition_feed("全部图书", &format!("/opds/books?page={page}"), &items, total, page, PAGE_SIZE),
    ))
}

async fn authors(State(s): State<AppState>) -> ApiResult<impl IntoResponse> {
    let authors = s.db.list_authors().await?;
    let links: Vec<(String, String)> = authors
        .iter()
        .map(|(a, n)| {
            (
                format!("{a} ({n})"),
                format!("/opds/authors/{}", urlencoding(a)),
            )
        })
        .collect();
    Ok(atom(NAV_CT, navigation_feed("按作者", "/opds/authors", &links)))
}

fn urlencoding(s: &str) -> String {
    // Minimal percent-encoding for a path segment (UTF-8 bytes, keep
    // unreserved ASCII). Avoids a new dependency for one call.
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn author_books(
    State(s): State<AppState>,
    Path(author): Path<String>,
) -> ApiResult<impl IntoResponse> {
    let items = s.db.books_by_author(&author).await?;
    let total = items.len() as i64;
    Ok(atom(
        ACQ_CT,
        acquisition_feed(
            &format!("{author} 的书"),
            &format!("/opds/authors/{}", urlencoding(&author)),
            &items,
            total,
            1,
            PAGE_SIZE,
        ),
    ))
}

async fn recent(State(s): State<AppState>, Query(q): Query<PageQuery>) -> ApiResult<impl IntoResponse> {
    let page = page_of(&q);
    let (items, total) = s
        .db
        .list_books(None, None, None, "updated", page, PAGE_SIZE)
        .await?;
    Ok(atom(
        ACQ_CT,
        acquisition_feed("最新入库", &format!("/opds/recent?page={page}"), &items, total, page, PAGE_SIZE),
    ))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
}

async fn search(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> ApiResult<impl IntoResponse> {
    let term = q.q.unwrap_or_default();
    let (items, total) = s
        .db
        .list_books(Some(term.clone()), None, None, "title", 1, PAGE_SIZE)
        .await?;
    Ok(atom(
        ACQ_CT,
        acquisition_feed(
            &format!("搜索：{term}"),
            &format!("/opds/search?q={}", urlencoding(&term)),
            &items,
            total,
            1,
            PAGE_SIZE,
        ),
    ))
}

async fn opensearch() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/opensearchdescription+xml")],
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <OpenSearchDescription xmlns=\"http://a9.com/-/spec/opensearch/1.1/\">\n\
         <ShortName>书库</ShortName><Description>书库搜索</Description>\n\
         <Url type=\"application/atom+xml\" template=\"/opds/search?q={searchTerms}\"/>\n\
         </OpenSearchDescription>\n",
    )
}

async fn download(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    let book = s
        .db
        .get_book(&id)
        .await?
        .ok_or_else(|| anyhow!("book not found: {id}"))?;
    let (bytes, mime, filename) = if book.source == "cached" {
        // Rebuild a .txt from the per-chapter files.
        let mut text = String::new();
        for idx in 0..book.chapter_count.max(0) {
            let f = s.config.chapter_file(&id, idx as usize);
            let raw = tokio::fs::read_to_string(&f).await.unwrap_or_default();
            let title = s.db.get_chapter_title(&id, idx).await?.unwrap_or_default();
            if !title.is_empty() {
                text.push_str(&title);
                text.push_str("\n\n");
            }
            for line in raw.lines() {
                // epub image markers are not text a reader can use.
                if line.starts_with('\u{1}') {
                    continue;
                }
                text.push_str(line);
                text.push('\n');
            }
            text.push('\n');
        }
        (
            text.into_bytes(),
            "text/plain; charset=utf-8".to_string(),
            format!("{}.txt", sanitize_filename(&book.title)),
        )
    } else {
        let path = book.path.clone().ok_or_else(|| anyhow!("book has no local path"))?;
        let ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("txt");
        let mime = match ext {
            "epub" => "application/epub+zip",
            _ => "text/plain; charset=utf-8",
        };
        let bytes = tokio::fs::read(&path).await.map_err(|e| anyhow!("read {path}: {e}"))?;
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("book.txt");
        // Disk filenames may contain control characters (\n, \r, ...) which
        // would corrupt the Content-Disposition header value (header
        // injection) — sanitize the same way as the cached-book title.
        let filename = sanitize_filename(filename);
        (bytes, mime.to_string(), filename)
    };
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    ))
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        // Control characters (\n, \r, \0, ...) would corrupt the
        // Content-Disposition header value (header injection); strip them
        // the same way as the path-unsafe punctuation.
        .map(|c| if c.is_control() || "/\\?%*:|\"<>".contains(c) { '_' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::BookRow;

    fn row(id: &str, title: &str, author: &str) -> BookRow {
        BookRow {
            id: id.into(),
            title: title.into(),
            author: author.into(),
            path: None,
            source_url: None,
            source: "local".into(),
            mtime: 0,
            size: 0,
            index_version: 1,
            status: "ok".into(),
            chapter_count: 2,
            created_at: 0,
            updated_at: 1_700_000_000,
            source_id: String::new(),
            has_update: false,
            last_update_check: 0,
            cover_mime: String::new(),
        }
    }

    #[test]
    fn xml_escape_escapes_all_five() {
        assert_eq!(xml_escape("a<b>&\"c'd"), "a&lt;b&gt;&amp;&quot;c&apos;d");
    }

    #[test]
    fn acquisition_feed_escapes_and_paginates() {
        let books = vec![row("1", "修仙<传>", "忘语&"), row("2", "另一本", "作者")];
        let feed = acquisition_feed("全部图书", "/opds/books?page=1", &books, 120, 1, 50);
        assert!(feed.contains("修仙&lt;传&gt;"), "title must be escaped: {feed}");
        assert!(feed.contains("忘语&amp;"), "author must be escaped");
        assert!(feed.contains("<opds:totalResults>120</opds:totalResults>"));
        assert!(feed.contains("rel=\"next\""), "page 1 of 3 must link next");
        assert!(!feed.contains("rel=\"previous\""), "page 1 has no previous");
        let last = acquisition_feed("全部图书", "/opds/books?page=3", &books, 120, 3, 50);
        assert!(last.contains("rel=\"previous\""));
        assert!(!last.contains("rel=\"next\""), "last page has no next");
    }

    #[test]
    fn navigation_feed_lists_subsections() {
        let feed = navigation_feed(
            "书库",
            "/opds/",
            &[("全部图书".into(), "/opds/books".into())],
        );
        assert!(feed.contains("kind=navigation"));
        assert!(feed.contains("/opds/books"));
    }
}
