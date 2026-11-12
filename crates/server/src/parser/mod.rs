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

// ---- vvsolo 通用规则移植（展示层，仅 paragraphs_from_text 用） ----
// 来源：typeset-web/packages/core/src/transforms/{punctuation,space,number,letter}.ts
// 与 pipelines.ts cleanup。只用于展示分段，不改存档正文与搜索索引。
// 不搬的：Quote（跨调用 stateful masking，搬到无状态 Rust 不稳阵）、
// Variant（改中文字形，有搜索风险）、Unicode 转义（txt 里罕见）、
// Chapter/Separator/SerialNumber/English/End（结构类，与段落展示无关）。

static PUNCT_MAP: &[(char, char)] = &[('-', '—'), ('︐', '，'), ('︑', '、'), ('︒', '。'), ('︓', '：'), ('︔', '；'), ('︕', '！'), ('︖', '？'), ('‛', '`'), ('｀', '`'), ('‐', '—'), ('━', '—'), ('―', '—'), ('─', '—'), ('ー', '—'), ('ˉ', '—'), ('﹣', '－'), ('~', '～'), ('〜', '～'), ('∽', '～'), ('∼', '～'), ('⁓', '～'), ('﹗', '！'), ('!', '！'), ('﹫', '@'), ('＠', '@'), ('﹟', '#'), ('＃', '#'), ('﹩', '$'), ('＄', '$'), ('﹪', '%'), ('％', '%'), ('﹠', '&'), ('＆', '&'), ('﹡', '＊'), ('ꁘ', '＊'), ('(', '（'), ('﹙', '（'), ('﹚', '）'), (')', '）'), ('﹐', '，'), (',', '，'), ('｡', '。'), ('.', '。'), ('．', '。'), ('∶', '：'), ('﹕', '：'), ('︰', '：'), (':', '：'), ('﹔', '；'), (';', '；'), ('､', '、'), ('丶', '、'), ('﹑', '、'), ('﹖', '？'), ('?', '？'), ('⋯', '…'), ('┅', '…'), ('¨', '…'), ('▪', '·'), ('•', '·'), ('‧', '·'), ('・', '·'), ('﹒', '·'), ('ꔷ', '·'), ('︳', '〉'), ('﹛', '｛'), ('﹜', '｝'), ('{', '｛'), ('}', '｝'), ('〝', '“'), ('〞', '”'), ('｢', '“'), ('″', '”'), ('〃', '”'), ('｣', '”'), ('‴', '”'), ('⌈', '「'), ('⌋', '」'), ('﹤', '＜'), ('﹥', '＞'), ('︿', '∧'), ('﹀', '∨'), ('＾', '^'), ('﹦', '='), ('＝', '='), ('﹢', '+'), ('＋', '+'), ('／', '/'), ('﹨', '\\'), ('＼', '\\'), ('｜', '|'), ('︱', '|'), ('¦', '|'), ('‖', '|'), ('＂', '"'), ('‟', '"'), ('′', '\''), ('＇', '\''), ];

/// 全角数字字母 → 半角（vvsolo NumberLetter；全角区与半角区固定差 0xFEE0）。
fn fullwidth_alnum_to_half(c: char) -> char {
    let u = c as u32;
    if (0xFF10..=0xFF19).contains(&u) || (0xFF21..=0xFF3A).contains(&u) || (0xFF41..=0xFF5A).contains(&u) {
        char::from_u32(u - 0xFEE0).unwrap_or(c)
    } else {
        c
    }
}

fn normalize_number_letter(text: &str) -> String {
    text.chars().map(fullwidth_alnum_to_half).collect()
}

static PUNCT_MAP_LOOKUP: LazyLock<std::collections::HashMap<char, char>> =
    LazyLock::new(|| PUNCT_MAP.iter().copied().collect());

static PUNCT_R2A: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"…[，；：、。·`～—？！]+").unwrap()
});
static PUNCT_R2B: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[，；。·`～—？！]+…").unwrap()
});
static PUNCT_R2C: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"。。+|，，+").unwrap()
});
static PUNCT_R3: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"…+").unwrap());
static PUNCT_R4: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"……(等[，。！？、])").unwrap());
static PUNCT_R6: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"“：|：“”").unwrap());
static PUNCT_R7: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"、”“").unwrap());
static PUNCT_R8: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"。）。").unwrap());
static PUNCT_R9: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([…。，！？])”[，；：、。·`～—？！]").unwrap()
});
static PUNCT_R10: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([，；：、。·`])[，；：、。·`～—？！]+").unwrap()
});
static PUNCT_R11: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([？！])[，；：、。·`～—]+").unwrap()
});
static PUNCT_R12: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"～[，；：、。·`—？！]+").unwrap()
});
static PUNCT_R13: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[？！]{2,}").unwrap());

/// vvsolo Punctuation R1：连续相同的标点（·、`°—－～：；（）［］｛｝%∧∨〈〉《》）
/// 合并———－～ 成对化，·、`° → ……，其余留单个。
fn merge_repeated_punct(text: &str) -> String {
    let set = "·、`°—－～：；（）［］｛｝%∧∨〈〉《》";
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if set.contains(c) {
            let mut j = i + 1;
            while j < chars.len() && chars[j] == c {
                j += 1;
            }
            if j - i > 1 {
                if "—－～".contains(c) {
                    out.push(c);
                    out.push(c);
                } else if "·、`°".contains(c) {
                    out.push_str("……");
                } else {
                    out.push(c);
                }
            } else {
                out.push(c);
            }
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// vvsolo Punctuation：先按 PUNCT_MAP 把半角/异体标点逐字映射为全角，
/// 再跑 13 组 rps（合并重复标点、省略号/破折号后缀清理、?! 连用规范化）。
/// 其中第 5 组（跨行的省略号行）只在整章文本层有意义，段落展示层跳过。
fn normalize_punctuation(text: &str) -> String {
    let mut s: String = text
        .chars()
        .map(|c| PUNCT_MAP_LOOKUP.get(&c).copied().unwrap_or(c))
        .collect();
    // R1：重复标点合并（—－～ 成对化，·、`° → ……，其余留单个）。
    // 不用正则 backreference（Rust regex 不支援 \1），手写扫描连续相同标点。
    s = merge_repeated_punct(&s);
    s = PUNCT_R2A.replace_all(&s, "…").into_owned();
    s = PUNCT_R2B.replace_all(&s, "…").into_owned();
    s = PUNCT_R2C.replace_all(&s, "…").into_owned();
    s = PUNCT_R3.replace_all(&s, "……").into_owned();
    s = PUNCT_R4.replace_all(&s, "$1").into_owned();
    s = PUNCT_R6.replace_all(&s, "：“").into_owned();
    s = PUNCT_R7.replace_all(&s, "”、“").into_owned();
    s = PUNCT_R8.replace_all(&s, "）。").into_owned();
    s = PUNCT_R9.replace_all(&s, "$1”").into_owned();
    s = PUNCT_R10.replace_all(&s, "$1").into_owned();
    s = PUNCT_R11.replace_all(&s, "$1").into_owned();
    s = PUNCT_R12.replace_all(&s, "～").into_owned();
    // R13：?! 连用规范化（？ 开头 → ？！/？？？；！ 开头 → ！？/！！！）。
    s = PUNCT_R13
        .replace_all(&s, |caps: &regex::Captures| {
            let m = caps.get(0).unwrap().as_str();
            let mut it = m.chars();
            let first = it.next().unwrap_or('？');
            let rest: String = it.collect();
            if first == '？' {
                if rest.contains('！') { "？！".to_string() } else { "？？？".to_string() }
            } else if rest.contains('？') {
                "！？".to_string()
            } else {
                "！！！".to_string()
            }
        })
        .into_owned();
    s
}

/// 是否中文或全角符号（vvsolo Space 字符组的近似：汉字+假名+注音+全角区）。
fn is_cjk_or_fw(c: char) -> bool {
    matches!(c,
        '\u{2E80}'..='\u{2EFF}'
        | '\u{2F00}'..='\u{2FDF}'
        | '\u{3000}'..='\u{303F}'
        | '\u{3040}'..='\u{309F}'
        | '\u{30A0}'..='\u{30FF}'
        | '\u{3100}'..='\u{312F}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}'
        | '\u{20000}'..='\u{2A6DF}'
        | '·' | '–' | '—' | '―' | '…' | '※' | '・' | '°' | '％' | '‰' | '‱' | '℃' | '℉' | '%' | '&' | '/' | '\\'
    )
}

/// vvsolo Space：删掉汉字/全角标点及 &、/、\\ 左右两侧的多余半角空格。
/// 整段连续空格视作一段，若其任一侧贴着中文/全角符号则整段删掉。
fn normalize_space(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' {
            let start = i;
            while i < chars.len() && chars[i] == ' ' {
                i += 1;
            }
            let prev = if start > 0 { Some(chars[start - 1]) } else { None };
            let next = chars.get(i).copied();
            let adj = prev.map(is_cjk_or_fw).unwrap_or(false)
                || next.map(is_cjk_or_fw).unwrap_or(false);
            if !adj {
                for _ in start..i {
                    out.push(' ');
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// 展示层通用排版：全角数字字母转半角 → 标点修正 → 空格修正。
/// 顺序重要：标点要在断行修正之前跑（半角 . 先变全角 。，断行规则才认得出句末）。
fn apply_typeset(text: &str) -> String {
    let s = normalize_number_letter(text);
    let s = normalize_punctuation(&s);
    normalize_space(&s)
}

/// vvsolo「修正错误换行」breakPairs 的移植
/// （typeset-web/packages/core/src/pipelines.ts，cleanup pipeline）。
/// 网上下载的小说常带源文件定宽硬换行：行尾不是句末标点（。！？…”：；〗】）
/// 的换行视为硬换行，删掉换行把它并回上一行；再加几组引号/省略号周围的修正。
/// 只用于展示分段，不改存档正文与搜索索引。
static WRONG_BREAK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([^。！？…”：；〗】])\n([^\n])").unwrap()
});

fn fix_wrong_line_breaks(block: &str) -> String {
    // 主规则跑两遍（vvsolo 也是两遍；单遍 replace_all 会漏掉相邻的连续断行，
    // 因为一次匹配消耗了行尾字符，下一处断行就对不上）。
    let mut s = block.to_string();
    for _ in 0..2 {
        s = WRONG_BREAK_RE.replace_all(&s, "$1$2").into_owned();
    }
    // 引号/省略号周围的断行修正（vvsolo breakPairs 其余几组）。
    s = s.replace("，”\n", "，”").replace("\n”", "”").replace("…\n…", "……");
    s
}

/// Turn chapter text (plain lines + `\x01IMG:` marker lines) into the
/// paragraphs the reader renders. `img_base` is the images endpoint
/// prefix, e.g. `/api/books/{id}/images/`.
///
/// Reflow: lines are grouped into blocks by blank lines; inside a block the
/// vvsolo wrong-break rule above joins hard-wrapped lines into one flowing
/// paragraph, and only the line breaks it keeps (after sentence-ending
/// punctuation) stay as paragraph boundaries. IMG marker lines are always
/// their own paragraph and never join.
pub fn paragraphs_from_text(text: &str, img_base: &str, reflow: bool) -> Vec<crate::source::Paragraph> {
    let mut paras: Vec<crate::source::Paragraph> = Vec::new();
    let mut block: Vec<String> = Vec::new();
    let mut flush = |block: &mut Vec<String>, paras: &mut Vec<crate::source::Paragraph>| {
        if block.is_empty() {
            return;
        }
        // reflow=true（txt/缓存纯文本）才做断行合并；epub 行本身就是 HTML block，不合并。
        let joined = if reflow {
            fix_wrong_line_breaks(&block.join("\n"))
        } else {
            block.join("\n")
        };
        for line in joined.lines() {
            let t = line.trim();
            if !t.is_empty() {
                paras.push(crate::source::Paragraph { text: Some(t.to_string()), img: None });
            }
        }
        block.clear();
    };
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            flush(&mut block, &mut paras);
            continue;
        }
        if let Some(p) = t.strip_prefix(IMG_MARKER) {
            flush(&mut block, &mut paras);
            paras.push(crate::source::Paragraph { text: None, img: Some(format!("{}{}", img_base, url_encode_path(p))) });
            continue;
        }
        // 每行先过展示层通用排版（全角转半角→标点→空格），再进块做断行修正。
        block.push(apply_typeset(t));
    }
    flush(&mut block, &mut paras);
    paras
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
    fn paragraphs_join_hard_wrapped_lines() {
        // vvsolo「修正错误换行」：源文件定宽硬换行（行尾不是句末标点）要并回一段。
        // 取自线上实测截图的段落：三行硬换行应并成一段，不再出现「城」「省」单字孤行。
        let text = "受害的苦主们也很开心，他们的女儿\n终得复仇，黄泉之下也能安息，整个开封城\n的人都开心，尤其是那些事不关己的平民百姓，平白无故地白吃三天的豪宴，省\n下了一大笔钱。\n\n下了一大笔钱。\n";
        let paras = paragraphs_from_text(text, "/api/books/abc/images/", true);
        assert_eq!(paras.len(), 2, "paras: {paras:?}");
        assert_eq!(
            paras[0].text.as_deref(),
            Some("受害的苦主们也很开心，他们的女儿终得复仇，黄泉之下也能安息，整个开封城的人都开心，尤其是那些事不关己的平民百姓，平白无故地白吃三天的豪宴，省下了一大笔钱。")
        );
        assert_eq!(paras[1].text.as_deref(), Some("下了一大笔钱。"));
        // 行尾是句末标点 。 的换行是真的分段，保留。
        let text2 = "第一句话。\n第二句话。\n";
        let paras2 = paragraphs_from_text(text2, "/api/books/abc/images/", true);
        assert_eq!(paras2.len(), 2);
        assert_eq!(paras2[0].text.as_deref(), Some("第一句话。"));
        assert_eq!(paras2[1].text.as_deref(), Some("第二句话。"));
    }

    #[test]
    fn typeset_general_rules() {
        // vvsolo 通用规则（展示层）：半角标点转全角、全角数字字母转半角、汉字间空格删除。
        assert_eq!(apply_typeset("你好,世界!这是test。"), "你好，世界！这是test。");
        assert_eq!(apply_typeset("第１２３章 ＡＢＣ"), "第123章ABC");
        assert_eq!(apply_typeset("中 文 标 点"), "中文标点");
        // 重复标点合并：。。。→……；？！连用规范化。
        assert_eq!(apply_typeset("真的吗。。。"), "真的吗……");
        assert_eq!(apply_typeset("什么？！"), "什么？！");
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
        let paras = paragraphs_from_text(text, "/api/books/abc/images/", false);
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
