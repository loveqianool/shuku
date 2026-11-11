//! E-book parsing: txt (chardetng + chapter-split regex) and epub (epub-rs).
//! Returns chapter streams; body text is never written to SQLite.

use anyhow::{Context, Result};
use regex::Regex;
use scraper::Html;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

/// Marker line in epub chapter text for an inline image: the rest of the
/// line is the image's path inside the epub (resolved against the chapter
/// file). Chapter text is plain text elsewhere, so a \x01 prefix can never
/// collide with real content. `paragraphs_from_text` turns marker lines
/// into `Paragraph{img}`; `strip_img_markers` removes them before indexing.
pub const IMG_MARKER: &str = "\x01IMG:";

/// Percent-decode `%XX` sequences (UTF-8). Invalid sequences stay as-is.
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a path for use in a URL, keeping `/` and the usual
/// unreserved characters. Used for the images endpoint URL.
pub(crate) fn url_encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for &b in p.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Resolve an `<img src>` against the directory of the xhtml file that
/// contains it, lexically (no filesystem access): `..` pops, a leading
/// `/` is the epub root. Returns the path as stored in the zip.
fn resolve_epub_path(base_dir: &Path, src: &str) -> String {
    let src = percent_decode(src);
    let joined = if let Some(rest) = src.strip_prefix('/') {
        PathBuf::from(rest)
    } else {
        base_dir.join(&src)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::Normal(c) => out.push(c),
        }
    }
    out.to_string_lossy().replace('\\', "/")
}

/// Turn chapter text (plain lines + `\x01IMG:` marker lines) into the
/// paragraphs the reader renders. `img_base` is the images endpoint
/// prefix, e.g. `/api/books/{id}/images/`.
pub fn paragraphs_from_text(text: &str, img_base: &str) -> Vec<crate::source::Paragraph> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            if let Some(p) = l.strip_prefix(IMG_MARKER) {
                crate::source::Paragraph { text: None, img: Some(format!("{}{}", img_base, url_encode_path(p))) }
            } else {
                crate::source::Paragraph { text: Some(l.to_string()), img: None }
            }
        })
        .collect()
}

/// Remove `\x01IMG:` marker lines so they never pollute the search index.
pub fn strip_img_markers(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with(IMG_MARKER))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone)]
pub struct Chapter {
    pub title: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct ParsedBook {
    pub title: String,
    pub author: String,
    pub chapters: Vec<Chapter>,
    /// (bytes, mime) of the epub cover, if the epub declares one.
    pub cover: Option<(Vec<u8>, String)>,
}

/// Covers over this size are skipped (a cover is decoration, not content).
pub const MAX_COVER_BYTES: usize = 5_000_000;

static CHAPTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Matches a chapter heading at the START of a line. Trailing text after
    // the heading marker is allowed (e.g. "第一章 开始"); the title is the
    // whole line when it is short, else just the matched marker.
    Regex::new(
        r"(?m)^\s*(第[一二三四五六七八九十百千零〇两0123456789\s]+[章卷回节集部篇]|第\d+[章卷回节集部篇]|卷[一二三四五六七八九十百千\d\s]+|序[章言]?|楔子|引子|前言|后记|尾声|番外[^\n]{0,40}|Chapter\s+\d+[^\n]{0,40}|CHAPTER\s+\d+[^\n]{0,40})",
    )
    .unwrap()
});

/// Split plain text into chapters. Lines matching the heading regex start a
/// new chapter; leading text before the first heading becomes "前言".
pub fn split_txt_chapters(text: &str) -> Vec<Chapter> {
    let mut chapters: Vec<Chapter> = Vec::new();
    let mut cur_title: Option<String> = None;
    let mut cur_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(m) = CHAPTER_RE.find(line) {
            let heading = m.as_str().trim().to_string();
            // Title: whole line when short, else just the heading marker.
            let title = if line.trim().chars().count() < 60 {
                line.trim().to_string()
            } else {
                heading.clone()
            };
            if heading.len() > 3 && title.chars().count() < 60 {
                if let Some(t) = cur_title.take() {
                    chapters.push(Chapter { title: t, text: cur_lines.join("\n") });
                    cur_lines.clear();
                } else if !cur_lines.is_empty() {
                    chapters.push(Chapter { title: "前言".to_string(), text: cur_lines.join("\n") });
                    cur_lines.clear();
                }
                cur_title = Some(title);
                continue;
            }
        }
        cur_lines.push(line);
    }
    if let Some(t) = cur_title {
        chapters.push(Chapter { title: t, text: cur_lines.join("\n") });
    } else if !cur_lines.is_empty() {
        chapters.push(Chapter { title: "正文".to_string(), text: cur_lines.join("\n") });
    }
    if chapters.is_empty() {
        chapters.push(Chapter { title: "正文".to_string(), text: String::new() });
    }
    chapters
}

/// Decode raw bytes with chardetng auto-detection (UTF-8 / GBK / Big5 ...).
pub fn decode_bytes(bytes: &[u8]) -> String {
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(bytes, true);
    let enc = detector.guess(None, true);
    let (cow, _, _) = enc.decode(bytes);
    cow.into_owned()
}

pub fn parse_txt_bytes(bytes: &[u8], fallback_title: &str) -> ParsedBook {
    let text = decode_bytes(bytes);
    let chapters = split_txt_chapters(&text);
    // Try to guess title/author from the first lines: "书名 作者" patterns are
    // unreliable; keep filename title, empty author.
    ParsedBook { title: fallback_title.to_string(), author: String::new(), chapters, cover: None }
}

pub fn parse_txt_file(path: &Path) -> Result<ParsedBook> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "未知".to_string());
    Ok(parse_txt_bytes(&bytes, &stem))
}

/// Convert HTML to plain text; `<img>` elements become `\x01IMG:{path}`
/// marker lines, with `src` resolved against the chapter file's directory.
fn html_to_text_with_images(html: &str, base_dir: &Path) -> String {
    let frag = Html::parse_fragment(html);
    let mut out = String::new();
    for node in frag.tree.root().descendants() {
        let v = node.value();
        if let Some(text) = v.as_text() {
            out.push_str(text);
            out.push('\n');
        } else if let Some(el) = v.as_element() {
            if el.name() == "img" {
                if let Some(src) = el.attr("src") {
                    let src = src.split(['?', '#']).next().unwrap_or("");
                    if !src.is_empty() && !src.starts_with("data:") {
                        out.push_str(IMG_MARKER);
                        out.push_str(&resolve_epub_path(base_dir, src));
                        out.push('\n');
                    }
                }
            }
        }
    }
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_epub_file(path: &Path) -> Result<ParsedBook> {
    let mut doc = epub::doc::EpubDoc::new(path).with_context(|| format!("open epub {}", path.display()))?;
    let title = doc.get_title().unwrap_or_else(|| {
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "未知".into())
    });
    let author = doc.mdata("creator").map(|m| m.value.clone()).unwrap_or_default();
    let n = doc.get_num_chapters();
    // Prefer NCX toc labels when they line up with spine order.
    let toc_labels: Vec<String> = doc.toc.iter().map(|p| p.label.clone()).collect();
    let mut chapters = Vec::with_capacity(n);
    for i in 0..n {
        if !doc.set_current_chapter(i) {
            continue;
        }
        let raw = doc.get_current_str().map(|(s, _mime)| s).unwrap_or_default();
        let base_dir = doc
            .get_current_path()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default();
        let text = html_to_text_with_images(&raw, &base_dir);
        let label = toc_labels.get(i).cloned().filter(|s| !s.trim().is_empty());
        let title = label.unwrap_or_else(|| format!("第{}章", i + 1));
        chapters.push(Chapter { title, text });
    }
    if chapters.is_empty() {
        anyhow::bail!("epub has no readable spine pages: {}", path.display());
    }
    // EPUB2: <meta name="cover">; EPUB3: manifest properties="cover-image"
    // (both handled by epub-rs). Oversized or non-image covers are skipped.
    let cover = doc
        .get_cover()
        .filter(|(b, m)| b.len() <= MAX_COVER_BYTES && m.starts_with("image/"));
    Ok(ParsedBook { title, author, chapters, cover })
}

pub fn parse_book_file(path: &Path) -> Result<ParsedBook> {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("epub") => parse_epub_file(path),
        Some("txt") => parse_txt_file(path),
        _ => anyhow::bail!("unsupported extension: {}", path.display()),
    }
}

// --- Test helpers: minimal stored-zip epub builder -------------------------

#[cfg(test)]
pub fn test_crc32(data: &[u8]) -> u32 {
    static TABLE: LazyLock<[u32; 256]> = LazyLock::new(|| {
        let mut t = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            t[i as usize] = c;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
pub fn test_stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = test_crc32(data);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    let cd_size = central.len() as u32;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// Minimal EPUB 2.0 with one chapter (containing an inline image) and a
/// declared cover (`<meta name="cover">` + cover.jpg). Shared by the
/// parser, scanner and (via a copy) integration tests.
#[cfg(test)]
pub fn test_epub_with_cover() -> Vec<u8> {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="id" version="2.0">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>图册</dc:title><dc:creator>作者</dc:creator><dc:identifier id="id">urn:uuid:test</dc:identifier><meta name="cover" content="cover-img"/></metadata>
<manifest><item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/><item id="pic" href="images/pic.jpg" media-type="image/jpeg"/><item id="cover-img" href="images/cover.jpg" media-type="image/jpeg"/></manifest>
<spine><itemref idref="ch1"/></spine></package>"#;
    let container = r#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
    let xhtml = r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>第一章</title></head><body><p>第一段文字</p><p><img src="images/pic.jpg" alt="插图"/></p><p>第二段文字</p></body></html>"#;
    test_stored_zip(&[
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", container.as_bytes()),
        ("OEBPS/content.opf", opf.as_bytes()),
        ("OEBPS/chapter1.xhtml", xhtml.as_bytes()),
        ("OEBPS/images/pic.jpg", b"\xff\xd8\xff\xe0fake-jpeg"),
        ("OEBPS/images/cover.jpg", b"\xff\xd8\xff\xe0fake-cover"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_chapter_split() {
        let text = "第一章 开始\n少年走在路上。\n\n第二章 练功\n他在山洞打坐。\n第3章 下山\n回家了。\n";
        let chs = split_txt_chapters(text);
        assert_eq!(chs.len(), 3);
        assert_eq!(chs[0].title, "第一章 开始");
        assert!(chs[0].text.contains("少年走在路上"));
        assert_eq!(chs[1].title, "第二章 练功");
        assert_eq!(chs[2].title, "第3章 下山");
    }

    #[test]
    fn txt_no_headings_is_single_chapter() {
        let chs = split_txt_chapters("没有章节标题的纯文本\n第二行");
        assert_eq!(chs.len(), 1);
        assert_eq!(chs[0].title, "正文");
    }

    #[test]
    fn txt_preface_before_first_heading() {
        let chs = split_txt_chapters("作者的话\n写在前面。\n第一章 开始\n正文。");
        assert_eq!(chs[0].title, "前言");
        assert_eq!(chs[1].title, "第一章 开始");
    }

    #[test]
    fn gbk_bytes_decode() {
        // "第一章 开始" in GBK
        let bytes = vec![0xB5, 0xDA, 0xD2, 0xBB, 0xD5, 0xC2, 0x20, 0xBF, 0xAA, 0xCA, 0xBC];
        let s = decode_bytes(&bytes);
        assert!(s.contains("第一章"), "decoded: {s}");
        let chs = split_txt_chapters(&s);
        assert_eq!(chs.len(), 1);
    }

    #[test]
    fn broken_epub_errors() {
        let dir = std::env::temp_dir().join("shuku-test-broken.epub");
        std::fs::write(&dir, b"not a zip at all").unwrap();
        assert!(parse_epub_file(&dir).is_err());
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn epub_image_paragraphs() {
        let zip = test_epub_with_cover();
        let path = std::env::temp_dir().join("shuku-test-img.epub");
        std::fs::write(&path, &zip).unwrap();
        let book = parse_epub_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(book.title, "图册");
        assert_eq!(book.chapters.len(), 1);
        // Cover: declared via <meta name="cover">, jpeg bytes.
        let (cover_bytes, cover_mime) = book.cover.as_ref().expect("epub must expose its cover");
        assert_eq!(cover_mime, "image/jpeg");
        assert_eq!(cover_bytes.as_slice(), b"\xff\xd8\xff\xe0fake-cover");
        let text = &book.chapters[0].text;
        assert!(text.contains(&format!("{IMG_MARKER}OEBPS/images/pic.jpg")), "chapter text: {text:?}");
        assert!(text.contains("第一段文字"));
        // Paragraphs: head <title> text (pre-existing html_to_text
        // behaviour), text, image, text — the marker never leaks into text.
        let paras = paragraphs_from_text(text, "/api/books/abc/images/");
        assert_eq!(paras.len(), 4);
        assert_eq!(paras[0].text.as_deref(), Some("第一章"));
        assert_eq!(paras[1].text.as_deref(), Some("第一段文字"));
        assert_eq!(paras[2].img.as_deref(), Some("/api/books/abc/images/OEBPS/images/pic.jpg"));
        assert_eq!(paras[3].text.as_deref(), Some("第二段文字"));
        // Indexing strips the marker line entirely.
        let clean = strip_img_markers(text);
        assert!(!clean.contains(IMG_MARKER));
        assert!(clean.contains("第一段文字") && clean.contains("第二段文字"));
        // Relative resolution: ../ and leading /.
        assert_eq!(resolve_epub_path(Path::new("OEBPS/text"), "../images/a.jpg"), "OEBPS/images/a.jpg");
        assert_eq!(resolve_epub_path(Path::new("OEBPS/text"), "/images/b.jpg"), "images/b.jpg");
        assert_eq!(resolve_epub_path(Path::new("OEBPS/text"), "images/my%20pic.jpg"), "OEBPS/text/images/my pic.jpg");
    }
}
