//! Missing / scrambled chapter detection.
//!
//! Two signals, both computed from chapter titles + content emptiness:
//! * **gap**: chapter numbers (第 N 章 / 第 N 回 / leading digits) are not
//!   consecutive — a jump means chapters are missing, a repeat or a
//!   backward step means the order is scrambled.
//! * **empty**: a chapter with no content at all (cached book whose fetch
//!   failed, or a local chapter that is only a heading).
//!
//! Number parsing handles Arabic digits and Chinese numerals up to
//! 万 (一千二百三十四 -> 1234). Titles without a number (序章、后记、
//! 楔子) are skipped, and a new 卷 resets the sequence, so volume
//! restarts don't false-positive as duplicates.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChapterIssue {
    pub idx: i64,
    pub title: String,
    pub kind: String, // "gap" | "duplicate" | "empty"
    pub detail: String,
}

fn cn_digit(c: char) -> Option<i64> {
    match c {
        '一' => Some(1),
        '二' | '两' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    }
}

/// Chinese numeral (一千二百三十四) -> number. None on unknown chars.
pub fn cn_numeral(s: &str) -> Option<i64> {
    let mut result: i64 = 0;
    let mut num: i64 = 0;
    let mut any = false;
    for c in s.chars() {
        match c {
            '零' => {
                any = true;
            }
            '十' => {
                result += if num == 0 { 10 } else { num * 10 };
                num = 0;
                any = true;
            }
            '百' => {
                result += if num == 0 { 100 } else { num * 100 };
                num = 0;
                any = true;
            }
            '千' => {
                result += if num == 0 { 1000 } else { num * 1000 };
                num = 0;
                any = true;
            }
            '万' => {
                result = (result + num) * 10_000;
                num = 0;
                any = true;
            }
            _ => {
                num = cn_digit(c)?;
                any = true;
            }
        }
    }
    if !any {
        return None;
    }
    Some(result + num)
}

fn numbered_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // NOTE: 卷 is deliberately NOT in the class — in "第一卷 第二章"
        // the chapter number is 2, not the volume number 1.
        Regex::new(r"第\s*([0-9]+|[一二三四五六七八九十百千零两]+)\s*[章节回集话篇]")
            .expect("numbered chapter regex")
    })
}

fn leading_digits_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(\d+)[、\.\s]").expect("leading digits regex"))
}

fn volume_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"第\s*([0-9]+|[一二三四五六七八九十百千零两]+)\s*卷")
            .expect("volume regex")
    })
}

/// The chapter number in a title, if any. `None` for 序章/后记 etc.
pub fn chapter_number(title: &str) -> Option<i64> {
    if let Some(cap) = numbered_re().captures(title) {
        let raw = &cap[1];
        if let Ok(n) = raw.parse::<i64>() {
            return Some(n);
        }
        return cn_numeral(raw);
    }
    if let Some(cap) = leading_digits_re().captures(title) {
        return cap[1].parse::<i64>().ok();
    }
    None
}

/// Volume number in a title, if any （第一卷 / 第二卷 …).
fn volume_number(title: &str) -> Option<i64> {
    let cap = volume_re().captures(title)?;
    let raw = &cap[1];
    if let Ok(n) = raw.parse::<i64>() {
        return Some(n);
    }
    cn_numeral(raw)
}

/// Gap / duplicate / out-of-order issues from a title sequence.
/// `titles` is (idx, title) in chapter order.
pub fn detect_gaps(titles: &[(i64, String)]) -> Vec<ChapterIssue> {
    let mut issues = Vec::new();
    let mut prev: Option<i64> = None;
    let mut prev_vol: Option<i64> = None;
    for (idx, title) in titles {
        // A new volume restarts numbering — don't flag the reset.
        if let Some(v) = volume_number(title) {
            if prev_vol != Some(v) {
                prev_vol = Some(v);
                prev = None;
            }
        }
        let Some(n) = chapter_number(title) else {
            continue;
        };
        match prev {
            None => {
                if n > 1 {
                    issues.push(ChapterIssue {
                        idx: *idx,
                        title: title.clone(),
                        kind: "gap".into(),
                        detail: format!("从第 {n} 章开始，缺第 1–{} 章", n - 1),
                    });
                }
            }
            Some(p) if n == p + 1 => {}
            Some(p) if n == p => {
                issues.push(ChapterIssue {
                    idx: *idx,
                    title: title.clone(),
                    kind: "duplicate".into(),
                    detail: format!("与上一章同为第 {n} 章，顺序可能乱了"),
                });
            }
            Some(p) if n > p + 1 => {
                issues.push(ChapterIssue {
                    idx: *idx,
                    title: title.clone(),
                    kind: "gap".into(),
                    detail: format!("第 {p} 章后直接是第 {n} 章，缺第 {}–{} 章", p + 1, n - 1),
                });
            }
            Some(p) => {
                issues.push(ChapterIssue {
                    idx: *idx,
                    title: title.clone(),
                    kind: "duplicate".into(),
                    detail: format!("第 {n} 章出现在第 {p} 章后，顺序异常"),
                });
            }
        }
        prev = Some(n);
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cn_numeral_parses() {
        assert_eq!(cn_numeral("一"), Some(1));
        assert_eq!(cn_numeral("十"), Some(10));
        assert_eq!(cn_numeral("十二"), Some(12));
        assert_eq!(cn_numeral("二十五"), Some(25));
        assert_eq!(cn_numeral("一百零三"), Some(103));
        assert_eq!(cn_numeral("一千二百三十四"), Some(1234));
        assert_eq!(cn_numeral("一万零五"), Some(10005));
        assert_eq!(cn_numeral("两"), Some(2));
        assert_eq!(cn_numeral("abc"), None);
    }

    #[test]
    fn chapter_number_extracts() {
        assert_eq!(chapter_number("第一章 开始"), Some(1));
        assert_eq!(chapter_number("第十二章 练功"), Some(12));
        assert_eq!(chapter_number("第 100 回 大战"), Some(100));
        assert_eq!(chapter_number("001、开局"), Some(1));
        assert_eq!(chapter_number("序章"), None);
        assert_eq!(chapter_number("后记"), None);
        assert_eq!(chapter_number("第一千二百三十四章"), Some(1234));
    }

    #[test]
    fn detect_gaps_finds_missing_duplicate_and_late_start() {
        let titles: Vec<(i64, String)> = vec![
            (0, "第一章".into()),
            (1, "第二章".into()),
            (2, "第五章".into()),
            (3, "第五章".into()),
            (4, "第七章".into()),
        ];
        let issues = detect_gaps(&titles);
        assert_eq!(issues.len(), 3, "{issues:?}");
        assert_eq!(issues[0].kind, "gap");
        assert!(issues[0].detail.contains("缺第 3–4 章"), "{}", issues[0].detail);
        assert_eq!(issues[1].kind, "duplicate");
        assert_eq!(issues[2].kind, "gap");
    }

    #[test]
    fn detect_gaps_skips_unnumbered_and_resets_on_volume() {
        let titles: Vec<(i64, String)> = vec![
            (0, "序章".into()),
            (1, "第一章".into()),
            (2, "第二章".into()),
            (3, "后记".into()),
        ];
        assert!(detect_gaps(&titles).is_empty());
        let titles: Vec<(i64, String)> = vec![
            (0, "第一卷 第一章".into()),
            (1, "第一卷 第二章".into()),
            (2, "第二卷 第一章".into()),
            (3, "第二卷 第二章".into()),
        ];
        assert!(detect_gaps(&titles).is_empty(), "volume restart is not a duplicate");
    }

    #[test]
    fn detect_gaps_flags_late_start() {
        let titles: Vec<(i64, String)> = vec![(0, "第五章".into()), (1, "第六章".into())];
        let issues = detect_gaps(&titles);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].detail.contains("缺第 1–4 章"), "{}", issues[0].detail);
    }
}
