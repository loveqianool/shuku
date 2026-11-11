//! Minimal JSONPath subset over serde_json::Value.
//!
//! Supported: `$`, `.field`, `['field']`, `["field"]`, `[n]`, `[-n]`,
//! `[*]`, chained segments (`.a[0].b[*].c`), recursive descent (`..`,
//! e.g. `$..title`, `$..book[*].title`), and filters
//! (`[?(...)]`, e.g. `$.data[?(@.price > 10)]`,
//! `$.data[?(@.vip)]`, conditions joined by `&&` / `||`, operators
//! `==` `!=` `>` `<` `>=` `<=`, literals: numbers, quoted strings,
//! `true` / `false` / `null`). No slices. Anything else fails with
//! a clear message instead of silently returning nothing.

use anyhow::{anyhow, Result};
use serde_json::Value;

#[derive(Debug, Clone)]
enum Seg {
    Field(String),
    Index(i64),
    Wildcard,
    /// `..`: the next segment applies to every descendant (self included).
    Descend,
    Filter(Vec<(Cond, Option<Join>)>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Join {
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FOp {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
}

#[derive(Debug, Clone)]
struct Cond {
    /// Left side: a path from `@` (empty = the item itself) or a literal.
    left: Side,
    op: Option<(FOp, Side)>,
}

#[derive(Debug, Clone)]
enum Side {
    Path(Vec<PathSeg>),
    Lit(Value),
}

#[derive(Debug, Clone)]
enum PathSeg {
    Field(String),
    Index(i64),
}

pub fn validate_json_path(path: &str) -> Result<()> {
    parse_path(path).map(|_| ())
}

pub fn eval_json_path<'a>(root: &'a Value, path: &str) -> Result<Vec<&'a Value>> {
    let segs = parse_path(path)?;
    let mut cur: Vec<&'a Value> = vec![root];
    let mut recursive = false;
    for seg in &segs {
        if matches!(seg, Seg::Descend) {
            recursive = true;
            continue;
        }
        if recursive {
            // Expand to every descendant (self included), depth-first,
            // pre-order, then apply the next segment to that set.
            let mut expanded: Vec<&'a Value> = Vec::new();
            for v in &cur {
                collect_descendants(v, &mut expanded);
            }
            cur = expanded;
            recursive = false;
        }
        let mut next: Vec<&'a Value> = Vec::new();
        match seg {
            Seg::Field(name) => {
                for v in &cur {
                    if let Some(f) = v.get(name) {
                        next.push(f);
                    }
                }
            }
            Seg::Index(n) => {
                for v in &cur {
                    if let Some(arr) = v.as_array() {
                        let idx = if *n < 0 { arr.len() as i64 + n } else { *n };
                        if idx >= 0 {
                            if let Some(f) = arr.get(idx as usize) {
                                next.push(f);
                            }
                        }
                    }
                }
            }
            Seg::Wildcard => {
                for v in &cur {
                    match v {
                        Value::Array(arr) => next.extend(arr.iter()),
                        Value::Object(map) => next.extend(map.values()),
                        _ => {}
                    }
                }
            }
            Seg::Filter(conds) => {
                for v in &cur {
                    let candidates: Vec<&Value> = match v {
                        Value::Array(arr) => arr.iter().collect(),
                        Value::Object(map) => map.values().collect(),
                        _ => vec![],
                    };
                    for item in candidates {
                        if eval_filter(item, conds) {
                            next.push(item);
                        }
                    }
                }
            }
            Seg::Descend => unreachable!(),
        }
        cur = next;
        if cur.is_empty() {
            break;
        }
    }
    if recursive {
        return Err(anyhow!("jsonpath {path:?}: '..' must be followed by a segment"));
    }
    Ok(cur)
}

fn collect_descendants<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(v);
    match v {
        Value::Array(arr) => {
            for item in arr {
                collect_descendants(item, out);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_descendants(item, out);
            }
        }
        _ => {}
    }
}

fn eval_filter(item: &Value, conds: &[(Cond, Option<Join>)]) -> bool {
    let mut result: Option<bool> = None;
    for (cond, join) in conds {
        let v = eval_cond(item, cond);
        result = Some(match (result, join) {
            (None, _) => v,
            (Some(prev), Some(Join::And)) => prev && v,
            (Some(prev), Some(Join::Or)) => prev || v,
            (Some(prev), None) => prev && v,
        });
    }
    result.unwrap_or(false)
}

fn eval_cond(item: &Value, cond: &Cond) -> bool {
    match &cond.op {
        None => truthy(resolve_side(item, &cond.left)),
        Some((op, right)) => {
            let l = resolve_side(item, &cond.left);
            let r = resolve_side(item, right);
            match op {
                FOp::Eq => l == r,
                FOp::Ne => l != r,
                FOp::Gt | FOp::Lt | FOp::Ge | FOp::Le => match (l, r) {
                    (Some(Value::Number(a)), Some(Value::Number(b))) => {
                        let (a, b) = (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(0.0));
                        match op {
                            FOp::Gt => a > b,
                            FOp::Lt => a < b,
                            FOp::Ge => a >= b,
                            FOp::Le => a <= b,
                            _ => unreachable!(),
                        }
                    }
                    (Some(Value::String(a)), Some(Value::String(b))) => match op {
                        FOp::Gt => a > b,
                        FOp::Lt => a < b,
                        FOp::Ge => a >= b,
                        FOp::Le => a <= b,
                        _ => unreachable!(),
                    },
                    _ => false,
                },
            }
        }
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None => false,
        Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn resolve_side<'a>(item: &'a Value, side: &'a Side) -> Option<&'a Value> {
    match side {
        Side::Lit(v) => Some(v),
        Side::Path(segs) => {
            let mut cur = item;
            for seg in segs {
                match seg {
                    PathSeg::Field(name) => cur = cur.get(name)?,
                    PathSeg::Index(n) => {
                        let arr = cur.as_array()?;
                        let idx = if *n < 0 { arr.len() as i64 + n } else { *n };
                        if idx < 0 {
                            return None;
                        }
                        cur = arr.get(idx as usize)?;
                    }
                }
            }
            Some(cur)
        }
    }
}

fn parse_path(path: &str) -> Result<Vec<Seg>> {
    let path = path.trim();
    if path.is_empty() {
        return Err(anyhow!("empty jsonpath"));
    }
    let b = path.as_bytes();
    let mut i = 0usize;
    if b[0] == b'$' {
        i = 1;
    }
    let mut segs = Vec::new();
    let mut after_descend = false;
    while i < b.len() {
        if b[i] == b'.' {
            if i + 1 < b.len() && b[i + 1] == b'.' {
                segs.push(Seg::Descend);
                i += 2;
                after_descend = true;
                continue;
            }
            i += 1;
            let start = i;
            while i < b.len() && b[i] != b'.' && b[i] != b'[' {
                i += 1;
            }
            let field = &path[start..i];
            if field.is_empty() || !field.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                return Err(anyhow!("jsonpath {path:?}: invalid field name {field:?} (dot notation needs an identifier; use ['field'] for other names)"));
            }
            segs.push(Seg::Field(field.to_string()));
            after_descend = false;
        } else if b[i] == b'[' {
            // Find the matching ']' respecting quotes.
            let mut j = i + 1;
            let mut quote: Option<u8> = None;
            while j < b.len() {
                match (quote, b[j]) {
                    (None, b'\'' | b'"') => quote = Some(b[j]),
                    (Some(q), c) if c == q => quote = None,
                    (None, b']') => break,
                    _ => {}
                }
                j += 1;
            }
            if j >= b.len() {
                return Err(anyhow!("jsonpath {path:?}: unclosed '['"));
            }
            let inner = path[i + 1..j].trim();
            if let Some(filter) = inner.strip_prefix("?(") {
                let filter = filter.strip_suffix(')').ok_or_else(|| anyhow!("jsonpath {path:?}: filter must end with ')'"))?;
                segs.push(Seg::Filter(parse_filter(filter, path)?));
            } else if inner == "*" {
                segs.push(Seg::Wildcard);
            } else if let Some(f) = unquote(inner) {
                segs.push(Seg::Field(f));
            } else if let Ok(n) = inner.parse::<i64>() {
                segs.push(Seg::Index(n));
            } else {
                return Err(anyhow!("jsonpath {path:?}: unsupported bracket [{inner}] (use [n], [*], ['field'] or [?(...)])"));
            }
            i = j + 1;
            after_descend = false;
        } else if after_descend {
            // A bare field name right after `..` (e.g. `$..title`).
            let start = i;
            while i < b.len() && b[i] != b'.' && b[i] != b'[' {
                i += 1;
            }
            let field = &path[start..i];
            if field.is_empty() || !field.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                return Err(anyhow!("jsonpath {path:?}: invalid field name {field:?} after '..' (dot notation needs an identifier; use ['field'] for other names)"));
            }
            segs.push(Seg::Field(field.to_string()));
            after_descend = false;
        } else {
            return Err(anyhow!("jsonpath {path:?}: expected '.' or '[' at {:?}", &path[i..]));
        }
    }
    if segs.is_empty() {
        return Err(anyhow!("jsonpath {path:?}: no segments"));
    }
    Ok(segs)
}

/// Parse the inside of `[?(...)]`: conditions joined by `&&` / `||`.
fn parse_filter(inner: &str, path: &str) -> Result<Vec<(Cond, Option<Join>)>> {
    let mut out: Vec<(Cond, Option<Join>)> = Vec::new();
    let mut pending_join: Option<Join> = None;
    let mut start = 0usize;
    let mut quote: Option<u8> = None;
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (None, b'\'' | b'"') => quote = Some(bytes[i]),
            (Some(q), c) if c == q => quote = None,
            (None, b'&') if i + 1 < bytes.len() && bytes[i + 1] == b'&' => {
                let piece = inner[start..i].trim();
                if !piece.is_empty() {
                    out.push((parse_cond(piece, path)?, pending_join));
                }
                pending_join = Some(Join::And);
                i += 1;
                start = i + 1;
            }
            (None, b'|') if i + 1 < bytes.len() && bytes[i + 1] == b'|' => {
                let piece = inner[start..i].trim();
                if !piece.is_empty() {
                    out.push((parse_cond(piece, path)?, pending_join));
                }
                pending_join = Some(Join::Or);
                i += 1;
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let piece = inner[start..].trim();
    if piece.is_empty() {
        return Err(anyhow!("jsonpath {path:?}: empty filter condition"));
    }
    out.push((parse_cond(piece, path)?, pending_join));
    Ok(out)
}

fn parse_cond(s: &str, path: &str) -> Result<Cond> {
    // Find a comparison operator outside quotes.
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (None, b'\'' | b'"') => quote = Some(bytes[i]),
            (Some(q), c) if c == q => quote = None,
            (None, _) => {
                for (op_str, op) in [("==", FOp::Eq), ("!=", FOp::Ne), (">=", FOp::Ge), ("<=", FOp::Le), (">", FOp::Gt), ("<", FOp::Lt)] {
                    if s[i..].starts_with(op_str) {
                        let left = parse_side(s[..i].trim(), path)?;
                        let right = parse_side(s[i + op_str.len()..].trim(), path)?;
                        return Ok(Cond { left, op: Some((op, right)) });
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(Cond { left: parse_side(s.trim(), path)?, op: None })
}

fn parse_side(s: &str, path: &str) -> Result<Side> {
    if let Some(rest) = s.strip_prefix('@') {
        if rest.is_empty() {
            return Ok(Side::Path(vec![]));
        }
        return Ok(Side::Path(parse_cond_path(rest, path)?));
    }
    // Literal: number, quoted string, true / false / null.
    if s == "true" {
        return Ok(Side::Lit(Value::Bool(true)));
    }
    if s == "false" {
        return Ok(Side::Lit(Value::Bool(false)));
    }
    if s == "null" {
        return Ok(Side::Lit(Value::Null));
    }
    if let Some(v) = unquote(s) {
        return Ok(Side::Lit(Value::String(v)));
    }
    if let Ok(n) = s.parse::<i64>() {
        return Ok(Side::Lit(Value::Number(n.into())));
    }
    if let Ok(n) = s.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Ok(Side::Lit(Value::Number(num)));
        }
    }
    Err(anyhow!("jsonpath {path:?}: unsupported filter operand {s:?} (use @.path or a literal)"))
}

fn parse_cond_path(s: &str, path: &str) -> Result<Vec<PathSeg>> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut segs = Vec::new();
    while i < b.len() {
        if b[i] == b'.' {
            i += 1;
            let start = i;
            while i < b.len() && b[i] != b'.' && b[i] != b'[' {
                i += 1;
            }
            let field = &s[start..i];
            if field.is_empty() || !field.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                return Err(anyhow!("jsonpath {path:?}: invalid field name {field:?} in filter path"));
            }
            segs.push(PathSeg::Field(field.to_string()));
        } else if b[i] == b'[' {
            let mut j = i + 1;
            let mut quote: Option<u8> = None;
            while j < b.len() {
                match (quote, b[j]) {
                    (None, b'\'' | b'"') => quote = Some(b[j]),
                    (Some(q), c) if c == q => quote = None,
                    (None, b']') => break,
                    _ => {}
                }
                j += 1;
            }
            if j >= b.len() {
                return Err(anyhow!("jsonpath {path:?}: unclosed '[' in filter path"));
            }
            let inner = s[i + 1..j].trim();
            if let Some(f) = unquote(inner) {
                segs.push(PathSeg::Field(f));
            } else if let Ok(n) = inner.parse::<i64>() {
                segs.push(PathSeg::Index(n));
            } else {
                return Err(anyhow!("jsonpath {path:?}: unsupported [{inner}] in filter path"));
            }
            i = j + 1;
        } else {
            return Err(anyhow!("jsonpath {path:?}: expected '.' or '[' in filter path at {:?}", &s[i..]));
        }
    }
    Ok(segs)
}

/// A JSON value as plain text: strings unwrapped, numbers/bools as-is,
/// objects/arrays as compact JSON.
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() >= 2 && ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"'))) {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jsonpath_basic() {
        let v = json!({"data": [{"title": "书一", "author": "甲"}, {"title": "书二", "author": "乙"}], "total": 2});
        let titles: Vec<String> = eval_json_path(&v, "$.data[*].title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(titles, vec!["书一", "书二"]);
        assert_eq!(value_to_string(eval_json_path(&v, "$.data[0].author").unwrap()[0]), "甲");
        assert_eq!(value_to_string(eval_json_path(&v, "$.data[-1].title").unwrap()[0]), "书二");
        assert_eq!(value_to_string(eval_json_path(&v, "$.total").unwrap()[0]), "2");
        assert_eq!(value_to_string(eval_json_path(&v, "$['data'][1]['title']").unwrap()[0]), "书二");
        // Missing keys yield an empty list, not an error.
        assert!(eval_json_path(&v, "$.nope[*].title").unwrap().is_empty());
        assert!(eval_json_path(&v, "$.data[9]").unwrap().is_empty());
    }

    #[test]
    fn jsonpath_recursive_descent() {
        let v = json!({"store": {"book": [{"title": "A"}, {"title": "B"}], "bicycle": {"title": "C"}}});
        let titles: Vec<String> = eval_json_path(&v, "$..title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(titles, vec!["A", "B", "C"]);
        let books: Vec<String> = eval_json_path(&v, "$..book[*].title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(books, vec!["A", "B"]);
        // ..[*] walks every value; the root itself is included.
        assert!(!eval_json_path(&v, "$..store").unwrap().is_empty());
        assert_eq!(value_to_string(eval_json_path(&v, "$..bicycle.title").unwrap()[0]), "C");
    }

    #[test]
    fn jsonpath_filter() {
        let v = json!({"data": [
            {"title": "A", "price": 5, "vip": true},
            {"title": "B", "price": 15},
            {"title": "C", "price": 20, "vip": false}
        ]});
        let t: Vec<String> = eval_json_path(&v, "$.data[?(@.price > 10)].title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(t, vec!["B", "C"]);
        let t: Vec<String> = eval_json_path(&v, "$.data[?(@.vip)].title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(t, vec!["A"]);
        let t: Vec<String> = eval_json_path(&v, "$.data[?(@.price >= 15 && @.title != 'C')].title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(t, vec!["B"]);
        let t: Vec<String> = eval_json_path(&v, "$.data[?(@.title == 'A' || @.title == 'C')].title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(t, vec!["A", "C"]);
        // String and missing-field comparisons.
        let t: Vec<String> = eval_json_path(&v, "$.data[?(@.title < 'B')].title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(t, vec!["A"]);
        assert!(eval_json_path(&v, "$.data[?(@.nope == 1)]").unwrap().is_empty());
        // Filter + recursive descent together.
        let t: Vec<String> = eval_json_path(&v, "$..[?(@.price > 10)].title").unwrap().into_iter().map(value_to_string).collect();
        assert_eq!(t, vec!["B", "C"]);
    }

    #[test]
    fn jsonpath_invalid() {
        let v = json!({"a": 1});
        assert!(eval_json_path(&v, "$..").is_err());
        assert!(eval_json_path(&v, "$.a[?(@.b==1)").is_err());
        assert!(eval_json_path(&v, "$.a[?(@.b =~ 1)]").is_err());
        assert!(eval_json_path(&v, "$.a[1:2]").is_err());
        assert!(eval_json_path(&v, "$.a[").is_err());
        assert!(eval_json_path(&v, "").is_err());
    }
}
