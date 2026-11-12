//! tantivy full-text index, one document per chapter.
//! Fixed schema: book_id (fast), book_title, author, chapter_title, content
//! (jieba tokenized, snippet highlight). `chapter_idx` is an extra
//! stored/fast u64 field so hits can be located (not part of the fixed spec).

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::{doc, Index, IndexWriter, TantivyDocument, Term};
use tantivy_jieba::JiebaTokenizer;
use tracing::info;

pub const INDEX_VERSION: i64 = 4;

/// tantivy indexing threads: `SHUKU_INDEX_THREADS`, else half the CPU
/// count (1..=8). Writer threads show strongly diminishing returns past 2
/// (probe: 1 thread 5.7s vs 8 threads 4.8s for 159MB), and scan parse
/// workers already occupy cores — total compute threads should stay near
/// core count, so the writer gets half and parsing gets the rest.
fn writer_threads() -> usize {
    std::env::var("SHUKU_INDEX_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(1, 16))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .div_ceil(2)
                .clamp(1, 8)
        })
}

/// tantivy writer heap: `SHUKU_INDEX_HEAP_MB`, default 150MB (was 50MB:
/// too small for big scans, forces constant segment flushes).
fn writer_heap_bytes() -> usize {
    std::env::var("SHUKU_INDEX_HEAP_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|mb| mb.clamp(50, 2048) * 1_000_000)
        .unwrap_or(150_000_000)
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub book_id: String,
    pub book_title: String,
    pub author: String,
    pub chapter_idx: i64,
    pub chapter_title: String,
    pub snippet: String,
    pub score: f32,
    /// Literal occurrences of the query terms in this chapter's content
    /// (0 when the match is only via pinyin/title).
    pub match_count: i64,
}

/// One literal occurrence of the query in a chapter, for the flat
/// in-book match list (the other reader's interaction: every match
/// numbered, tap one to jump to that exact spot).
#[derive(Debug, Clone, Serialize)]
pub struct BookMatch {
    pub chapter_idx: i64,
    pub chapter_title: String,
    /// Char offset of the occurrence in the chapter's content.
    pub offset: i64,
    /// Context window (60 before / 90 after) with the term in `<em>`.
    pub snippet: String,
}

/// HTML-escape one char into `out` (fragments are rendered as HTML).
fn escape_char(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&#39;"),
        _ => out.push(c),
    }
}

/// All literal occurrences of `terms` in `chars`, in document order:
/// (char_pos, term_char_len). `term_chars` must be sorted longest-first
/// so a longer match wins over a shorter one starting at the same place.
/// Shared by content_fragments and the per-match list (list_matches).
fn find_occurrences(chars: &[char], term_chars: &[Vec<char>]) -> Vec<(usize, usize)> {
    let mut occs: Vec<(usize, usize)> = vec![];
    for tc in term_chars {
        let mut i = 0;
        while i + tc.len() <= chars.len() {
            if chars[i..i + tc.len()] == tc[..] {
                occs.push((i, tc.len()));
                // Advance by one, not by the term length, so overlapping
                // occurrences count (e.g. "aa" occurs twice in "aaa").
                // Highlighting still skips past each match (see
                // highlight_window), so rendering stays non-overlapping.
                i += 1;
            } else {
                i += 1;
            }
        }
    }
    occs.sort();
    occs
}

/// One HTML fragment for the char range [s, e) of `chars`, with the
/// terms wrapped in `<em>` (single chars HTML-escaped).
fn highlight_window(chars: &[char], s: usize, e: usize, term_chars: &[Vec<char>]) -> String {
    let mut out = String::new();
    let mut i = s;
    while i < e {
        let mut matched: Option<usize> = None;
        for tc in term_chars {
            if i + tc.len() <= e && chars[i..i + tc.len()] == tc[..] {
                matched = Some(tc.len());
                break;
            }
        }
        if let Some(len) = matched {
            out.push_str("<em>");
            for c in &chars[i..i + len] {
                escape_char(&mut out, *c);
            }
            out.push_str("</em>");
            i += len;
        } else {
            escape_char(&mut out, chars[i]);
            i += 1;
        }
    }
    out
}

/// One snippet window around a single occurrence (60 chars before,
/// 90 after), with the terms wrapped in `<em>`.
fn occurrence_snippet(chars: &[char], pos: usize, len: usize, term_chars: &[Vec<char>]) -> String {
    const BEFORE: usize = 60;
    const AFTER: usize = 90;
    let s = pos.saturating_sub(BEFORE);
    let e = (pos + len + AFTER).min(chars.len());
    highlight_window(chars, s, e, term_chars)
}

/// Build up to `max_fragments` windows around literal occurrences of
/// `terms` in `content` (document order, char-based), each with the
/// terms wrapped in `<em>`. Returns the fragments and the total number
/// of literal occurrences in the whole content.
///
/// A tantivy snippet shows only the single best fragment, but a chapter
/// can mention the term dozens of times, so the result list looked
/// incomplete. When no literal occurrence is found (e.g. pinyin-only
/// matches) this returns empty and the caller keeps the tantivy snippet.
fn content_fragments(content: &str, terms: &[&str], max_fragments: usize) -> (Vec<String>, usize) {
    let chars: Vec<char> = content.chars().collect();
    let mut term_chars: Vec<Vec<char>> = terms
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().collect())
        .collect();
    // Longer terms first, so a longer match wins over a shorter one
    // starting at the same position when highlighting.
    term_chars.sort_by(|a, b| b.len().cmp(&a.len()));
    if term_chars.is_empty() || chars.is_empty() {
        return (vec![], 0);
    }
    let occs = find_occurrences(&chars, &term_chars);
    let match_count = occs.len();
    if occs.is_empty() {
        return (vec![], 0);
    }
    // Merge occurrences into windows: 60 chars before, 90 after.
    const BEFORE: usize = 60;
    const AFTER: usize = 90;
    let mut windows: Vec<(usize, usize)> = vec![];
    for (pos, len) in occs {
        let s = pos.saturating_sub(BEFORE);
        let e = (pos + len + AFTER).min(chars.len());
        if let Some(last) = windows.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        windows.push((s, e));
    }
    windows.truncate(max_fragments);
    let fragments = windows
        .iter()
        .map(|&(s, e)| highlight_window(&chars, s, e, &term_chars))
        .collect();
    (fragments, match_count)
}

pub struct SearchIndex {
    index: Index,
    writer: Mutex<IndexWriter>,
    f_book_id: Field,
    f_book_title: Field,
    f_author: Field,
    f_chapter_idx: Field,
    f_chapter_title: Field,
    f_content: Field,
    f_pinyin: Field,
    f_source: Field,
}

/// Pinyin of a Chinese string, for pinyin search: space-separated
/// syllables plus the concatenated form, so both `xiu xian` and
/// `xiuxian` match — plus the initials string and all its prefixes,
/// so first-letter search works too (`xx` / `xxc` find 修仙传).
/// Non-Chinese characters contribute nothing (latin titles are already
/// searchable in their own fields).
/// e.g. "修仙传" -> "xiu xian chuan xiuxianchuan x xx xxc".
pub fn pinyin_text(s: &str) -> String {
    use pinyin::ToPinyin;
    let syls: Vec<&str> = s.chars().filter_map(|c| c.to_pinyin().map(|p| p.plain())).collect();
    if syls.is_empty() {
        return String::new();
    }
    let mut out = syls.join(" ");
    if syls.len() > 1 {
        out.push(' ');
        out.push_str(&syls.concat());
    }
    // Initials: first letter of each syllable, plus every prefix, so a
    // partial initials query matches (xx matches xxc).
    let initials: String = syls.iter().filter_map(|w| w.chars().next()).collect();
    for i in 1..=initials.len() {
        out.push(' ');
        out.push_str(&initials[..i]);
    }
    out
}

/// All plain pinyin syllables, collected once from the crate's dictionary
/// (scan the CJK blocks). Used to segment a latin query run into
/// syllables: `xiuxian` -> `xiu xian`.
fn pinyin_syllables() -> &'static std::collections::HashSet<&'static str> {
    use pinyin::ToPinyin;
    static SYLS: std::sync::OnceLock<std::collections::HashSet<&'static str>> = std::sync::OnceLock::new();
    SYLS.get_or_init(|| {
        (0x3400u32..=0x9fff)
            .filter_map(char::from_u32)
            .filter_map(|c| c.to_pinyin().map(|p| p.plain()))
            .collect()
    })
}

/// Greedy longest-match segmentation of a latin run into pinyin
/// syllables. Returns None when the run cannot be fully covered
/// (e.g. `harry` — not pinyin at all), so the caller keeps it as-is.
fn split_pinyin_run(run: &str) -> Option<String> {
    let syls = pinyin_syllables();
    let mut out: Vec<&str> = Vec::new();
    let mut rest = run;
    while !rest.is_empty() {
        let mut matched = None;
        // Longest pinyin syllable is 6 letters (zhuang/chuang/shuang).
        for len in (1..=rest.len().min(6)).rev() {
            let cand = &rest[..len];
            if syls.contains(cand) {
                matched = Some((cand, len));
                break;
            }
        }
        let (syl, len) = matched?;
        out.push(syl);
        rest = &rest[len..];
    }
    Some(out.join(" "))
}

/// Rewrite latin runs in a search query into pinyin syllables, so a
/// typed `xiuxian` matches the indexed `xiu xian`. Runs that are not
/// valid pinyin (english words, codes) are left untouched.
pub fn expand_pinyin_query(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.is_empty() {
            return;
        }
        let lower = run.to_ascii_lowercase();
        match split_pinyin_run(&lower) {
            Some(seg) => out.push_str(&seg),
            None => out.push_str(run),
        }
        run.clear();
    };
    for c in q.chars() {
        if c.is_ascii_alphabetic() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn build_schema() -> Schema {
    let mut sb = Schema::builder();
    sb.add_u64_field("book_id", FAST | INDEXED | STORED);
    sb.add_text_field("book_title", TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("jieba")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    ).set_stored());
    sb.add_text_field("author", TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("jieba")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    ).set_stored());
    sb.add_u64_field("chapter_idx", FAST | STORED);
    sb.add_text_field("chapter_title", TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("jieba")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    ).set_stored());
    sb.add_text_field("content", TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("jieba")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    ).set_stored());
    // Pinyin of book_title + author + chapter_title (indexed + stored for
    // the snippet fallback). Content pinyin is deliberately NOT indexed:
    // it would roughly double the index for a query shape users rarely
    // type — pinyin search is for finding a book / chapter, not for
    // full-text pinyin matching.
    sb.add_text_field("pinyin", TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("jieba")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    ).set_stored());
    // Book source ("local" / "cached"): a keyword field for scope
    // filtering, so search does not have to fetch the book ids from
    // SQLite first. Raw tokenizer = the whole value is one token.
    // Basic (presence only) is enough for a Term filter.
    sb.add_text_field("source", TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_index_option(IndexRecordOption::Basic),
    ).set_stored());
    sb.build()
}

/// Remove every entry inside `dir`, keeping the directory itself.
fn clear_dir(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

impl SearchIndex {
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let schema = build_schema();
        let index = match Index::open_or_create(
            tantivy::directory::MmapDirectory::open(path)?,
            schema.clone(),
        ) {
            Ok(index) => index,
            // The on-disk index was built with an older schema (INDEX_VERSION
            // bump, e.g. a new field): tantivy refuses to open it, so the
            // process would die here before the boot scan could rebuild it
            // (ff2aa11 added the `source` field and bricked startup on the
            // deployed box). The index is fully derived data — books and
            // chapters live in SQLite + files — so drop it and start empty;
            // the boot scan re-indexes every book whose recorded version
            // is stale.
            Err(tantivy::TantivyError::SchemaError(msg)) => {
                tracing::warn!("tantivy index schema mismatch ({msg}); rebuilding index from scratch");
                clear_dir(path)?;
                Index::open_or_create(
                    tantivy::directory::MmapDirectory::open(path)?,
                    schema.clone(),
                )
                .context("open tantivy index after rebuild")?
            }
            Err(e) => return Err(e).context("open tantivy index"),
        };
        index.tokenizers().register("jieba", JiebaTokenizer::new());
        index.tokenizers().register("raw", tantivy::tokenizer::RawTokenizer::default());
        let writer = index.writer_with_num_threads(writer_threads(), writer_heap_bytes())?;
        Ok(Self {
            f_book_id: schema.get_field("book_id")?,
            f_book_title: schema.get_field("book_title")?,
            f_author: schema.get_field("author")?,
            f_chapter_idx: schema.get_field("chapter_idx")?,
            f_chapter_title: schema.get_field("chapter_title")?,
            f_content: schema.get_field("content")?,
            f_pinyin: schema.get_field("pinyin")?,
            f_source: schema.get_field("source")?,
            index,
            writer: Mutex::new(writer),
        })
    }

    #[cfg(test)]
    pub fn open_in_ram() -> Result<Self> {
        let schema = build_schema();
        let index = Index::create_in_ram(schema.clone());
        index.tokenizers().register("jieba", JiebaTokenizer::new());
        index.tokenizers().register("raw", tantivy::tokenizer::RawTokenizer::default());
        let writer = index.writer_with_num_threads(writer_threads(), writer_heap_bytes())?;
        Ok(Self {
            f_book_id: schema.get_field("book_id")?,
            f_book_title: schema.get_field("book_title")?,
            f_author: schema.get_field("author")?,
            f_chapter_idx: schema.get_field("chapter_idx")?,
            f_chapter_title: schema.get_field("chapter_title")?,
            f_content: schema.get_field("content")?,
            f_pinyin: schema.get_field("pinyin")?,
            f_source: schema.get_field("source")?,
            index,
            writer: Mutex::new(writer),
        })
    }

    /// Delete all chapters of a book, then add the new ones. Blocking call —
    /// run inside spawn_blocking.
    pub fn index_book(
        &self,
        book_id_num: u64,
        book_title: &str,
        author: &str,
        source: &str,
        chapters: &[(String, String)],
    ) -> Result<usize> {
        let mut w = self.writer.lock().map_err(|e| anyhow::anyhow!("index writer lock poisoned: {e}"))?;
        w.delete_term(Term::from_field_u64(self.f_book_id, book_id_num));
        let book_pinyin = format!("{} {}", pinyin_text(book_title), pinyin_text(author));
        let mut n = 0;
        for (i, (title, content)) in chapters.iter().enumerate() {
            let pinyin = format!("{book_pinyin} {}", pinyin_text(title));
            w.add_document(doc!(
                self.f_book_id => book_id_num,
                self.f_book_title => book_title,
                self.f_author => author,
                self.f_chapter_idx => i as u64,
                self.f_chapter_title => title.as_str(),
                self.f_content => content.as_str(),
                self.f_pinyin => pinyin.as_str(),
                self.f_source => source,
            ))?;
            n += 1;
        }
        w.commit()?;
        info!(target: "search", book_id = book_id_num, chapters = n, "indexed book");
        Ok(n)
    }

    /// Drop the whole index (rebuild path). Blocking call.
    pub fn delete_all(&self) -> Result<()> {
        let mut w = self.writer.lock().map_err(|e| anyhow::anyhow!("index writer lock poisoned: {e}"))?;
        w.delete_all_documents()?;
        w.commit()?;
        Ok(())
    }

    /// Delete all documents of one book (delete-book path). Blocking call.
    pub fn delete_book(&self, book_id_num: u64) -> Result<()> {
        let mut w = self.writer.lock().map_err(|e| anyhow::anyhow!("index writer lock poisoned: {e}"))?;
        w.delete_term(Term::from_field_u64(self.f_book_id, book_id_num));
        w.commit()?;
        Ok(())
    }

    /// Index several books with ONE writer lock + ONE commit. Blocking call —
    /// run inside spawn_blocking. This is what makes parallel scan fast:
    /// per-book commit() dominates when a scan indexes hundreds of books
    /// (each commit writes segment meta); batching amortizes it. Returns the
    /// per-book chapter counts, in input order. Crash safety is unchanged:
    /// a book is only published to SQLite after its batch commits, so an
    /// interrupted scan simply re-picks the book up next time (mtime /
    /// index_version still stale).
    pub fn index_books(
        &self,
        books: &[(u64, String, String, String, Vec<(String, String)>)],
    ) -> Result<Vec<usize>> {
        self.index_books_pinyin(books, true)
    }

    /// Same as `index_books` but with the pinyin field left empty when
    /// `include_pinyin` is false, so probes can measure what pinyin search
    /// actually costs (index size, index time). Production always passes
    /// true; the flag exists only for the probe.
    pub fn index_books_pinyin(
        &self,
        books: &[(u64, String, String, String, Vec<(String, String)>)],
        include_pinyin: bool,
    ) -> Result<Vec<usize>> {
        let mut w = self.writer.lock().map_err(|e| anyhow::anyhow!("index writer lock poisoned: {e}"))?;
        for (id, _, _, _, _) in books {
            w.delete_term(Term::from_field_u64(self.f_book_id, *id));
        }
        let mut counts = Vec::with_capacity(books.len());
        for (id, title, author, source, chapters) in books {
            let book_pinyin = if include_pinyin {
                format!("{} {}", pinyin_text(title), pinyin_text(author))
            } else {
                String::new()
            };
            let mut n = 0;
            for (i, (ctitle, content)) in chapters.iter().enumerate() {
                let pinyin = if include_pinyin {
                    format!("{book_pinyin} {}", pinyin_text(ctitle))
                } else {
                    String::new()
                };
                w.add_document(doc!(
                    self.f_book_id => *id,
                    self.f_book_title => title.as_str(),
                    self.f_author => author.as_str(),
                    self.f_chapter_idx => i as u64,
                    self.f_chapter_title => ctitle.as_str(),
                    self.f_content => content.as_str(),
                    self.f_pinyin => pinyin.as_str(),
                    self.f_source => source.as_str(),
                ))?;
                n += 1;
            }
            counts.push(n);
        }
        w.commit()?;
        info!(target: "search", books = books.len(), "indexed batch");
        Ok(counts)
    }

    /// Full-text search. `book_ids` restricts scope (None = all books);
    /// `source` restricts to one book source ("local" / "cached", None =
    /// all) via the indexed `source` field, so callers no longer have to
    /// fetch the book ids from SQLite first. Blocking call — run inside
    /// spawn_blocking.
    pub fn search(
        &self,
        query_str: &str,
        book_ids: Option<&[u64]>,
        source: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<SearchHit>, usize)> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let schema = self.index.schema();
        let mut parser = QueryParser::new(
            schema,
            vec![self.f_book_title, self.f_author, self.f_chapter_title, self.f_content, self.f_pinyin],
            self.index.tokenizers().clone(),
        );
        parser.set_conjunction_by_default();
        // Segment typed pinyin into syllables before parsing, so
        // `xiuxian` becomes `xiu xian` and matches the indexed field.
        let expanded = expand_pinyin_query(query_str);
        let base_query = parser.parse_query(&expanded)?;
        // Scope filters inside tantivy (BooleanQuery) so totals stay correct.
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![(Occur::Must, base_query)];
        if let Some(src) = source {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.f_source, src),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        let query: Box<dyn tantivy::query::Query> = match book_ids {
            Some(ids) if !ids.is_empty() => {
                let scope_clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = ids
                    .iter()
                    .map(|id| {
                        (
                            Occur::Should,
                            Box::new(TermQuery::new(
                                Term::from_field_u64(self.f_book_id, *id),
                                IndexRecordOption::Basic,
                            )) as Box<dyn tantivy::query::Query>,
                        )
                    })
                    .collect();
                clauses.push((Occur::Must, Box::new(BooleanQuery::new(scope_clauses))));
                Box::new(BooleanQuery::new(clauses))
            }
            Some(_) => return Ok((vec![], 0)),
            None => {
                if clauses.len() == 1 {
                    clauses.pop().unwrap().1
                } else {
                    Box::new(BooleanQuery::new(clauses))
                }
            }
        };
        let top_docs = searcher.search(
            &*query,
            &TopDocs::with_limit(limit + offset).and_offset(offset).order_by_score(),
        )?;
        let total = searcher.search(&*query, &tantivy::collector::Count)?;
        let mut snippet_gen = SnippetGenerator::create(&searcher, &*query, self.f_content)?;
        snippet_gen.set_max_num_chars(400);
        // Fallback generators (title / book / pinyin) are created lazily:
        // most hits match in the content, so building all four up front
        // wastes a tokenizer+highlighter setup on every search.
        let mut title_snippet_gen: Option<SnippetGenerator> = None;
        let mut book_snippet_gen: Option<SnippetGenerator> = None;
        let mut pinyin_snippet_gen: Option<SnippetGenerator> = None;
        let mut hits = Vec::new();
        for (score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let bid = doc.get_first(self.f_book_id).and_then(|v| v.as_u64()).unwrap_or(0);
            let mut snippet = snippet_gen
                .snippet_from_doc(&doc)
                .to_html()
                .replace("<b>", "<em>")
                .replace("</b>", "</em>");
            if snippet.is_empty() {
                // Match was in the title/author, not the body: highlight there.
                if title_snippet_gen.is_none() {
                    title_snippet_gen = SnippetGenerator::create(&searcher, &*query, self.f_chapter_title).ok().map(|mut g| { g.set_max_num_chars(120); g });
                }
                if let Some(g) = title_snippet_gen.as_mut() {
                    snippet = g.snippet_from_doc(&doc).to_html().replace("<b>", "<em>").replace("</b>", "</em>");
                }
            }
            if snippet.is_empty() {
                if book_snippet_gen.is_none() {
                    book_snippet_gen = SnippetGenerator::create(&searcher, &*query, self.f_book_title).ok().map(|mut g| { g.set_max_num_chars(120); g });
                }
                if let Some(g) = book_snippet_gen.as_mut() {
                    snippet = g.snippet_from_doc(&doc).to_html().replace("<b>", "<em>").replace("</b>", "</em>");
                }
            }
            if snippet.is_empty() {
                // Match was only via pinyin: show the matched pinyin.
                if pinyin_snippet_gen.is_none() {
                    pinyin_snippet_gen = SnippetGenerator::create(&searcher, &*query, self.f_pinyin).ok().map(|mut g| { g.set_max_num_chars(120); g });
                }
                if let Some(g) = pinyin_snippet_gen.as_mut() {
                    snippet = g.snippet_from_doc(&doc).to_html().replace("<b>", "<em>").replace("</b>", "</em>");
                }
            }
            // One tantivy fragment per chapter made results look
            // incomplete; show up to 5 windows around literal
            // occurrences instead, plus the total occurrence count.
            let content = doc
                .get_first(self.f_content)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let terms: Vec<&str> = query_str.split_whitespace().collect();
            let (fragments, match_count) = content_fragments(content, &terms, 5);
            if !fragments.is_empty() {
                snippet = fragments.join(" … ");
            }
            hits.push(SearchHit {
                book_id: bid.to_string(),
                book_title: doc.get_first(self.f_book_title).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                author: doc.get_first(self.f_author).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                chapter_idx: doc.get_first(self.f_chapter_idx).and_then(|v| v.as_u64()).unwrap_or(0) as i64,
                chapter_title: doc.get_first(self.f_chapter_title).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                snippet,
                score,
                match_count: match_count as i64,
            });
        }
        Ok((hits, total))
    }

    /// Book ids whose title / author / pinyin match `query_str`, in
    /// relevance order, deduplicated. Used by the library list filter and
    /// by search suggestions, so they match on jieba tokens and pinyin
    /// instead of the SQLite `instr` substring match. Searches
    /// title/author/pinyin only — not chapter content (a library filter
    /// is about the book, not a chapter). Blocking call — run inside
    /// spawn_blocking.
    pub fn search_book_ids(&self, query_str: &str, limit: usize) -> Result<Vec<u64>> {
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let schema = self.index.schema();
        let mut parser = QueryParser::new(
            schema,
            vec![self.f_book_title, self.f_author, self.f_pinyin],
            self.index.tokenizers().clone(),
        );
        parser.set_conjunction_by_default();
        let expanded = expand_pinyin_query(query_str);
        let query = parser.parse_query(&expanded)?;
        // Ask for more docs than the limit: several chapters of one book
        // can occupy the top slots, and we dedupe by book below.
        let top_docs = searcher.search(
            &*query,
            &TopDocs::with_limit(limit.saturating_mul(4).max(limit)).order_by_score(),
        )?;
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut out: Vec<u64> = Vec::new();
        for (_score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let bid = doc.get_first(self.f_book_id).and_then(|v| v.as_u64()).unwrap_or(0);
            if bid != 0 && seen.insert(bid) {
                out.push(bid);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Every literal occurrence of `query_str` in one book, in document
    /// order (chapter_idx, then offset). Returns the page and the total.
    /// Pinyin-only matches have no literal occurrence, so they contribute
    /// nothing here (the chapter-grouped search still covers them, and
    /// the reader falls back to it when this list is empty).
    pub fn list_matches(
        &self,
        book_id: u64,
        query_str: &str,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<BookMatch>, usize)> {
        let terms: Vec<&str> = query_str.split_whitespace().collect();
        let mut term_chars: Vec<Vec<char>> = terms
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| t.chars().collect())
            .collect();
        term_chars.sort_by(|a, b| b.len().cmp(&a.len()));
        if term_chars.is_empty() {
            return Ok((vec![], 0));
        }
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let schema = self.index.schema();
        let mut parser = QueryParser::new(
            schema,
            vec![self.f_book_title, self.f_author, self.f_chapter_title, self.f_content, self.f_pinyin],
            self.index.tokenizers().clone(),
        );
        parser.set_conjunction_by_default();
        let expanded = expand_pinyin_query(query_str);
        let base_query = parser.parse_query(&expanded)?;
        let query = BooleanQuery::new(vec![
            (Occur::Must, base_query),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.f_book_id, book_id),
                    IndexRecordOption::Basic,
                )) as Box<dyn tantivy::query::Query>,
            ),
        ]);
        // All matching chapters (a book has at most a few thousand);
        // document order comes from sorting by chapter_idx below.
        let n_chapters = searcher.search(&query, &Count)?;
        if n_chapters == 0 {
            return Ok((vec![], 0));
        }
        let top_docs = searcher.search(&query, &TopDocs::with_limit(n_chapters).order_by_score())?;
        let mut all: Vec<BookMatch> = vec![];
        for (_score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let content = doc
                .get_first(self.f_content)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let chars: Vec<char> = content.chars().collect();
            let occs = find_occurrences(&chars, &term_chars);
            if occs.is_empty() {
                continue;
            }
            let chapter_idx = doc.get_first(self.f_chapter_idx).and_then(|v| v.as_u64()).unwrap_or(0) as i64;
            let chapter_title = doc
                .get_first(self.f_chapter_title)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            for (pos, len) in occs {
                all.push(BookMatch {
                    chapter_idx,
                    chapter_title: chapter_title.clone(),
                    offset: pos as i64,
                    snippet: occurrence_snippet(&chars, pos, len, &term_chars),
                });
            }
        }
        all.sort_by_key(|m| (m.chapter_idx, m.offset));
        let total = all.len();
        let page = all.into_iter().skip(offset).take(limit).collect();
        Ok((page, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_pinyin_query_segments_latin_runs() {
        assert_eq!(expand_pinyin_query("xiuxian"), "xiu xian");
        assert_eq!(expand_pinyin_query("xiuxianchuan"), "xiu xian chuan");
        assert_eq!(expand_pinyin_query("wangyu"), "wang yu");
        assert_eq!(expand_pinyin_query("修仙 xiuxian"), "修仙 xiu xian");
        // Not pinyin: left untouched, so english/code search is unaffected.
        assert_eq!(expand_pinyin_query("harry"), "harry");
        assert_eq!(expand_pinyin_query("hello world"), "hello world");
    }

    #[test]
    fn content_fragments_multiple_windows() {
        let filler = "甲".repeat(200);
        let content = format!("李茉进了屋。{filler}李茉又出来了。{filler}李茉回来了。");
        let (frags, count) = content_fragments(&content, &["李茉"], 5);
        assert_eq!(count, 3);
        assert_eq!(frags.len(), 3);
        assert!(frags.iter().all(|f| f.contains("<em>李茉</em>")));
        // No literal occurrence (pinyin): empty fragments, count 0.
        let (frags, count) = content_fragments(&content, &["limo"], 5);
        assert_eq!(count, 0);
        assert!(frags.is_empty());
    }

    #[test]
    fn pinyin_text_joins_and_concatenates() {
        assert_eq!(pinyin_text("修仙传"), "xiu xian chuan xiuxianchuan x xx xxc");
        assert_eq!(pinyin_text("仙"), "xian x");
        assert_eq!(pinyin_text("第1章"), "di zhang dizhang d dz");
        assert_eq!(pinyin_text("第1章 第0本第1回"), "di zhang di ben di hui dizhangdibendihui d dz dzd dzdb dzdbd dzdbdh");
        assert_eq!(pinyin_text("Harry Potter"), "");
        assert_eq!(pinyin_text(""), "");
    }

    #[test]
    fn open_rebuilds_index_with_stale_schema() {
        // Regression: ff2aa11 added the `source` field (INDEX_VERSION 3→4)
        // and the deployed binary died at startup — open_or_create refuses
        // an on-disk index whose schema differs, before the boot scan
        // could rebuild it. open() must drop the stale index and start
        // empty instead of failing.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("probe-stale-schema-{}", uuid::Uuid::new_v4()));
        // Old schema: same shape minus the `source` field.
        let mut sb = tantivy::schema::SchemaBuilder::default();
        sb.add_text_field("book_id", tantivy::schema::TextOptions::default().set_stored());
        let old_schema = sb.build();
        {
            std::fs::create_dir_all(&dir).unwrap();
            let old = Index::create_in_dir(&dir, old_schema).unwrap();
            let mut w = old.writer(50_000_000).unwrap();
            w.add_document(doc!(old.schema().get_field("book_id").unwrap() => "b1"))
                .unwrap();
            w.commit().unwrap();
        }
        let idx = SearchIndex::open(&dir).unwrap();
        assert!(idx.index.schema().get_field("source").is_ok());
        assert_eq!(idx.index.reader().unwrap().searcher().num_docs(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_by_initials() {
        let idx = SearchIndex::open_in_ram().unwrap();
        idx.index_book(
            1,
            "修仙传",
            "忘语",
            "local",
            &[("第一章 开始".into(), "山里的少年什么也不知道。".into())],
        )
        .unwrap();
        idx.index_book(
            2,
            "测试小说",
            "作者甲",
            "local",
            &[("序".into(), "另一本完全无关的书。".into())],
        )
        .unwrap();
        // Full initials, partial initials, author initials.
        for q in ["xxc", "xx", "wy", "x"] {
            let (hits, _) = idx.search(q, None, None, 10, 0).unwrap();
            assert!(!hits.is_empty(), "initials query {q:?} must hit 修仙传");
            assert!(
                hits.iter().any(|h| h.book_id == "1"),
                "initials query {q:?} must include 修仙传: {hits:?}"
            );
        }
        // "xx" must not hit the other book (its initials are cxs / zzj / x).
        let (hits, _) = idx.search("xx", None, None, 10, 0).unwrap();
        assert!(hits.iter().all(|h| h.book_id == "1"), "xx must not hit 测试小说: {hits:?}");
        // Full pinyin still works after the initials change.
        let (hits, _) = idx.search("xiuxian", None, None, 10, 0).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].book_title, "修仙传");
    }

    #[test]
    fn search_by_pinyin() {
        let idx = SearchIndex::open_in_ram().unwrap();
        idx.index_book(
            1,
            "修仙传",
            "忘语",
            "local",
            &[("第一章 开始".into(), "山里的少年什么也不知道。".into())],
        )
        .unwrap();
        idx.index_book(
            2,
            "测试小说",
            "作者甲",
            "local",
            &[("序".into(), "另一本完全无关的书。".into())],
        )
        .unwrap();
        for q in ["xiuxian", "xiu xian", "wangyu", "xiuxianchuan", "xiu xian chuan"] {
            let (hits, _) = idx.search(q, None, None, 10, 0).unwrap();
            assert!(!hits.is_empty(), "pinyin query {q:?} must hit 修仙传");
            assert!(hits.iter().all(|h| h.book_id == "1"), "pinyin query {q:?} must not hit the other book: {hits:?}");
            assert_eq!(hits[0].book_title, "修仙传");
        }
        // Chinese query still works, and a pinyin-only match shows a snippet.
        let (hits, _) = idx.search("修仙", None, None, 10, 0).unwrap();
        assert!(!hits.is_empty());
        let (hits, _) = idx.search("xiuxian", None, None, 10, 0).unwrap();
        assert!(!hits[0].snippet.is_empty(), "pinyin match should still show a snippet");
    }

    #[test]
    fn delete_book_removes_docs() {
        let idx = SearchIndex::open_in_ram().unwrap();
        idx.index_book(1, "测试小说", "作者甲", "local", &[("第一章".into(), "修仙内容".into())]).unwrap();
        idx.index_book(2, "另一本", "作者乙", "local", &[("序".into(), "修仙世界".into())]).unwrap();
        idx.delete_book(1).unwrap();
        let (hits, _) = idx.search("修仙", None, None, 10, 0).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].book_id, "2");
    }

    #[test]
    fn search_and_highlight() {
        let idx = SearchIndex::open_in_ram().unwrap();
        idx.index_book(
            1,
            "测试小说",
            "作者甲",
            "local",
            &[
                ("第一章 开始".into(), "少年走在山路上，心中想着修仙的事。".into()),
                ("第二章 练功".into(), "他在山洞里打坐修炼，灵气汇聚。".into()),
            ],
        )
        .unwrap();
        let (hits, _total) = idx.search("修仙", None, None, 10, 0).unwrap();
        assert!(!hits.is_empty(), "expected a hit for 修仙");
        assert!(hits[0].snippet.contains("<em>"), "snippet should highlight: {}", hits[0].snippet);
        assert_eq!(hits[0].chapter_idx, 0);
        // incremental re-index replaces old docs
        idx.index_book(1, "测试小说", "作者甲", "local", &[("第一章 开始".into(), "全新的内容没有任何关键词。".into())])
            .unwrap();
        let (hits, _) = idx.search("修仙", None, None, 10, 0).unwrap();
        assert!(hits.is_empty(), "old docs must be gone after re-index");
        // scope filter
        idx.index_book(2, "另一本", "作者乙", "local", &[("序".into(), "修仙世界广阔无边。".into())])
            .unwrap();
        let (hits, _) = idx.search("修仙", Some(&[2]), None, 10, 0).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].book_id, "2");
    }

    #[test]
    fn search_scope_by_source_field() {
        // scope=local / scope=cached filters by the indexed `source`
        // field, without fetching book ids from SQLite first.
        let idx = SearchIndex::open_in_ram().unwrap();
        idx.index_book(1, "本地书", "作者甲", "local", &[("第一章".into(), "修仙内容".into())]).unwrap();
        idx.index_book(2, "缓存书", "作者乙", "cached", &[("序".into(), "修仙世界".into())]).unwrap();
        let (hits, _) = idx.search("修仙", None, Some("local"), 10, 0).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].book_id, "1");
        let (hits, _) = idx.search("修仙", None, Some("cached"), 10, 0).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].book_id, "2");
        let (hits, total) = idx.search("修仙", None, None, 10, 0).unwrap();
        assert_eq!(total, 2);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_book_ids_matches_title_and_pinyin_initials() {
        // Library filter / suggest via tantivy: title substring, author,
        // and pinyin initials (wlcl -> 武林沉沦) all match; chapter
        // content alone does not (a library filter is about the book).
        let idx = SearchIndex::open_in_ram().unwrap();
        idx.index_book(1, "武林沉沦", "佚名", "local", &[("第一章".into(), "江湖内容".into())]).unwrap();
        idx.index_book(2, "修仙传", "忘语", "local", &[("第一章".into(), "武林高手出现".into())]).unwrap();
        assert_eq!(idx.search_book_ids("武林", 10).unwrap(), vec![1]);
        assert_eq!(idx.search_book_ids("wlcl", 10).unwrap(), vec![1]);
        assert_eq!(idx.search_book_ids("忘语", 10).unwrap(), vec![2]);
        // "武林高手" only occurs in book 2's chapter content, not in a
        // title/author, so the book filter must not match it.
        assert!(idx.search_book_ids("武林高手", 10).unwrap().is_empty());
    }

    #[test]
    fn list_matches_returns_every_occurrence() {
        let idx = SearchIndex::open_in_ram().unwrap();
        idx.index_book(
            1,
            "武林沉沦番外",
            "佚名",
            "local",
            &[
                ("番外（1）".into(), "李茉进了屋，李茉又出来了。".into()),
                ("第一章".into(), "他想起李茉，想起李茉，想起李茉。".into()),
            ],
        )
        .unwrap();
        idx.index_book(2, "另一本", "作者乙", "local", &[("序".into(), "李茉也在别处。".into())])
            .unwrap();
        // 2 + 3 = 5 occurrences, document order (chapter, then offset).
        let (items, total) = idx.list_matches(1, "李茉", 10, 0).unwrap();
        assert_eq!(total, 5);
        assert_eq!(items.len(), 5);
        assert!(items.iter().all(|m| m.snippet.contains("<em>李茉</em>")), "snippets must highlight");
        assert_eq!(items[0].chapter_idx, 0);
        assert_eq!(items[0].chapter_title, "番外（1）");
        assert_eq!(items[2].chapter_idx, 1);
        assert_eq!(items[2].chapter_title, "第一章");
        assert!(items.windows(2).all(|w| (w[0].chapter_idx, w[0].offset) < (w[1].chapter_idx, w[1].offset)), "must be in document order");
        // Pagination.
        let (page, total) = idx.list_matches(1, "李茉", 2, 0).unwrap();
        assert_eq!(total, 5);
        assert_eq!(page.len(), 2);
        let (page, _) = idx.list_matches(1, "李茉", 2, 4).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].chapter_idx, 1);
        // Scoped to one book: book 2's single occurrence is not included.
        let (_, total) = idx.list_matches(2, "李茉", 10, 0).unwrap();
        assert_eq!(total, 1);
        // Pinyin has no literal occurrence: empty (the chapter-grouped
        // search still covers it, and the reader falls back to it).
        let (_, total) = idx.list_matches(1, "limo", 10, 0).unwrap();
        assert_eq!(total, 0);
    }

    /// Perf probe: what pinyin + first-letter search actually costs, on the
    /// 200-book perf corpus (159MB, ~10k chapters). Builds the same index
    /// twice — with and without the pinyin field — and compares size,
    /// index time, and query latency. Skips when perf-books/ is absent.
    #[test]
    fn probe_pinyin_cost() {
        use std::time::Instant;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("perf-books");
        if !dir.exists() {
            return;
        }
        let mut files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .collect();
        files.sort();
        let t0 = Instant::now();
        let mut books: Vec<(u64, String, String, String, Vec<(String, String)>)> = Vec::new();
        for (i, f) in files.iter().enumerate() {
            let parsed = crate::parser::parse_book_file(f).unwrap();
            let chs: Vec<(String, String)> =
                parsed.chapters.iter().map(|c| (c.title.clone(), c.text.clone())).collect();
            books.push((i as u64 + 1, parsed.title, parsed.author, "local".to_string(), chs));
        }
        let parse_ms = t0.elapsed().as_millis();
        let total_ch: usize = books.iter().map(|b| b.3.len()).sum();
        let total_bytes: usize = books.iter().flat_map(|b| b.4.iter().map(|c| c.1.len())).sum();

        // 1. Raw cost of computing pinyin_text for every title/author.
        let t0 = Instant::now();
        let mut pinyin_bytes = 0usize;
        for (_, title, author, _source, chs) in &books {
            pinyin_bytes += pinyin_text(title).len() + pinyin_text(author).len();
            for (ct, _) in chs {
                pinyin_bytes += pinyin_text(ct).len();
            }
        }
        println!(
            "pinyin probe: books={} chapters={total_ch} content_bytes={total_bytes} parse_ms={parse_ms} pinyin_text_ms={} pinyin_bytes={pinyin_bytes}",
            books.len(),
            t0.elapsed().as_millis()
        );

        // 2. Two disk indexes, same docs, pinyin on vs off. Wait 15s after
        // each build so background segment merges settle — measuring right
        // after the last commit counts unmerged segments (each carrying its
        // own copy of the content term dictionary) and overstates the delta.
        let dir_size = |d: &std::path::Path| -> u64 {
            walkdir::WalkDir::new(d)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        };
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let mut sizes = Vec::new();
        let mut times = Vec::new();
        for (name, with_pinyin) in [("with", true), ("without", false)] {
            let idx_dir = base.join(format!("probe-pinyin-{name}-{}", uuid::Uuid::new_v4()));
            let (size, index_ms) = {
                let idx = SearchIndex::open(&idx_dir).unwrap();
                let t0 = Instant::now();
                for chunk in books.chunks(16) {
                    idx.index_books_pinyin(chunk, with_pinyin).unwrap();
                }
                let index_ms = t0.elapsed().as_millis();
                // Let the merge threads catch up before measuring.
                std::thread::sleep(std::time::Duration::from_secs(15));
                (dir_size(&idx_dir), index_ms)
            }; // index dropped before the dir is removed
            let segments: usize = std::fs::read_dir(&idx_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".store"))
                        .count()
                })
                .unwrap_or(0);
            println!("pinyin probe: index {name}: ms={index_ms} bytes={size} segments={segments}");
            sizes.push(size);
            times.push(index_ms);
            let _ = std::fs::remove_dir_all(&idx_dir);
        }
        let (with_size, with_ms) = (sizes[0], times[0]);
        let (without_size, without_ms) = (sizes[1], times[1]);
        println!(
            "pinyin probe: DELTA size_bytes={} ({:.1}% of index) index_ms={} vs {without_ms}",
            with_size as i64 - without_size as i64,
            100.0 * (with_size as i64 - without_size as i64) as f64 / without_size as f64,
            with_ms
        );

        // 3. Query latency on the pinyin index (fresh build, median of 21).
        let idx_dir = base.join(format!("probe-pinyin-q-{}", uuid::Uuid::new_v4()));
        let idx = SearchIndex::open(&idx_dir).unwrap();
        for chunk in books.chunks(16) {
            idx.index_books_pinyin(chunk, true).unwrap();
        }
        // First call builds the syllable set once; time it separately.
        let t0 = Instant::now();
        let _ = expand_pinyin_query("xiuxian");
        println!("pinyin probe: first expand_pinyin_query (syllable-set build) ms={}", t0.elapsed().as_millis());
        // Queries that actually match this synthetic corpus (titles are
        // 第N本 / 第N章, content is repeated 江湖 sentences):
        //   江湖  = chinese content match (many docs)
        //   di    = pinyin of 第, matches every chapter title
        //   dyz   = initials prefix of 第N章 (di yi zhang), matches every chapter
        //   jianghu = pinyin of content words — NOT indexed by design, 0 hits
        //   xx    = initials matching nothing here, 0 hits
        for q in ["江湖", "di", "dyz", "jianghu", "xx", "不存在的词"] {
            let mut times: Vec<u128> = Vec::new();
            for _ in 0..21 {
                let t0 = Instant::now();
                let (hits, total) = idx.search(q, None, None, 10, 0).unwrap();
                times.push(t0.elapsed().as_micros());
                let _ = (hits, total);
            }
            times.sort_unstable();
            let (hits, total) = idx.search(q, None, None, 10, 0).unwrap();
            println!("pinyin probe: query {q:?} median_us={} hits={} total={total}", times[10], hits.len());
        }
        let _ = std::fs::remove_dir_all(&idx_dir);
    }
}
