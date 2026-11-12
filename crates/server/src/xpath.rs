//! Minimal XPath subset evaluated over scraper's HTML DOM.
//!
//! Supported: axes `//`, `/`, `.//`, `.`, `..` and named axes
//! `child::`, `descendant::`, `descendant-or-self::`, `parent::`,
//! `ancestor::`, `ancestor-or-self::`, `self::`, `following-sibling::`,
//! `preceding-sibling::`; node tests `tag`, `*`, `@attr`, `text()`
//! (final step only); predicates `[@a='v']`, `[@a]`, `[n]`, `[last()]`,
//! `[position() >/>=/</<=/= n]`, `[contains(@a,'v')]`,
//! `[contains(text(),'v')]`, `[starts-with(@a,'v')]`, and several
//! predicates in a row.
//!
//! Not supported (loud error, never silent empty): `following::` /
//! `preceding::` (whole-document order), `attribute::` (use `@attr`),
//! `node()`.
//!
//! Anything else fails with a message naming the unsupported part —
//! never silently returns an empty result for syntax we cannot run.
//! (No libxml2: a native dependency would break the musl static build,
//! so this is hand-rolled on the DOM scraper already parses.)

use anyhow::{anyhow, Result};
use scraper::ElementRef;

#[derive(Debug, Clone)]
pub enum XValue<'a> {
    El(ElementRef<'a>),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Axis {
    Child,
    Descendant,
    DescOrSelf,
    Parent,
    Ancestor,
    AncestorOrSelf,
    SelfStep,
    FollowingSibling,
    PrecedingSibling,
}

#[derive(Debug, Clone)]
enum NodeTest {
    Tag(String),
    Any,
    Text,
    Attr(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cmp {
    Gt,
    Lt,
    Ge,
    Le,
    Eq,
}

#[derive(Debug, Clone)]
enum Pred {
    AttrEq(String, String),
    AttrExists(String),
    AttrContains(String, String),
    AttrStartsWith(String, String),
    TextContains(String),
    Index(usize),
    Last,
    PosCmp(Cmp, i64),
}

#[derive(Debug, Clone)]
struct Step {
    axis: Axis,
    test: NodeTest,
    preds: Vec<Pred>,
}

pub fn validate_xpath(expr: &str) -> Result<()> {
    parse_steps(expr).map(|_| ())
}

pub fn eval_xpath<'a>(scope: ElementRef<'a>, expr: &str) -> Result<Vec<XValue<'a>>> {
    let steps = parse_steps(expr)?;
    let mut els: Vec<ElementRef<'a>> = vec![scope];
    for (si, step) in steps.iter().enumerate() {
        let last = si == steps.len() - 1;
        match &step.test {
            NodeTest::Text => {
                if !last {
                    return Err(anyhow!("xpath {expr:?}: text() must be the last step"));
                }
                // text() reads the text of the elements the previous step
                // selected — it is not a child-axis step of its own.
                let cands = apply_preds(els.clone(), &step.preds)?;
                return Ok(cands
                    .into_iter()
                    .map(|e| XValue::Str(e.text().collect::<Vec<_>>().join("").trim().to_string()))
                    .collect());
            }
            NodeTest::Attr(name) => {
                if !last {
                    return Err(anyhow!("xpath {expr:?}: @attr must be the last step"));
                }
                // Likewise, @attr reads an attribute of the elements the
                // previous step selected.
                let cands = apply_preds(els.clone(), &step.preds)?;
                return Ok(cands
                    .into_iter()
                    .map(|e| XValue::Str(e.value().attr(name).unwrap_or("").to_string()))
                    .collect());
            }
            _ => {
                els = apply_preds(apply_test(apply_axis(&els, &step.axis), &step.test), &step.preds)?;
            }
        }
    }
    Ok(els.into_iter().map(XValue::El).collect())
}

fn apply_axis<'a>(ctx: &[ElementRef<'a>], axis: &Axis) -> Vec<ElementRef<'a>> {
    let mut out: Vec<ElementRef<'a>> = Vec::new();
    match axis {
        Axis::Child => {
            for e in ctx {
                out.extend(e.child_elements());
            }
        }
        Axis::Descendant => {
            for e in ctx {
                // descendent_elements() yields the element itself first.
                out.extend(e.descendent_elements().skip(1));
            }
        }
        Axis::DescOrSelf => {
            for e in ctx {
                // descendent_elements() yields the element itself first.
                out.extend(e.descendent_elements());
            }
        }
        Axis::Parent => {
            for e in ctx {
                if let Some(p) = e.parent().and_then(ElementRef::wrap) {
                    out.push(p);
                }
            }
        }
        Axis::Ancestor => {
            for e in ctx {
                // XPath ancestor order: nearest first.
                let mut p = e.parent().and_then(ElementRef::wrap);
                while let Some(par) = p {
                    out.push(par);
                    p = par.parent().and_then(ElementRef::wrap);
                }
            }
        }
        Axis::AncestorOrSelf => {
            for e in ctx {
                out.push(*e);
                let mut p = e.parent().and_then(ElementRef::wrap);
                while let Some(par) = p {
                    out.push(par);
                    p = par.parent().and_then(ElementRef::wrap);
                }
            }
        }
        Axis::SelfStep => out.extend_from_slice(ctx),
        Axis::FollowingSibling => {
            for e in ctx {
                if let Some(par) = e.parent().and_then(ElementRef::wrap) {
                    let sibs: Vec<ElementRef> = par.child_elements().collect();
                    if let Some(pos) = sibs.iter().position(|s| s == e) {
                        out.extend(sibs[pos + 1..].iter().copied());
                    }
                }
            }
        }
        Axis::PrecedingSibling => {
            for e in ctx {
                if let Some(par) = e.parent().and_then(ElementRef::wrap) {
                    let sibs: Vec<ElementRef> = par.child_elements().collect();
                    if let Some(pos) = sibs.iter().position(|s| s == e) {
                        // XPath preceding-sibling order: nearest first.
                        out.extend(sibs[..pos].iter().rev().copied());
                    }
                }
            }
        }
    }
    dedup(out)
}

/// Node-sets are sets: drop duplicates (overlapping subtrees from `//`)
/// while keeping document order.
fn dedup<'a>(els: Vec<ElementRef<'a>>) -> Vec<ElementRef<'a>> {
    // ElementRef is PartialEq but not Hash; node sets are small.
    let mut out: Vec<ElementRef<'a>> = Vec::with_capacity(els.len());
    for e in els {
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

fn apply_test<'a>(els: Vec<ElementRef<'a>>, test: &NodeTest) -> Vec<ElementRef<'a>> {
    match test {
        NodeTest::Any => els,
        NodeTest::Tag(name) => els
            .into_iter()
            .filter(|e| e.value().name().eq_ignore_ascii_case(name))
            .collect(),
        // Text/Attr are handled in eval_xpath before reaching here.
        _ => els,
    }
}

/// Group `els` (document order, already node-test filtered) by parent and
/// keep, per group, the elements whose 1-based position within that group
/// satisfies `keep`. This is standard XPath positional semantics: `div[1]`
/// is the first `div` child of EACH parent, not the first `div` in the
/// whole result set (the old code did the latter, so `//li[1]` on a page
/// with several lists returned only the very first `li`). Groups are
/// built in document order and a parent's children are contiguous in it,
/// so concatenating the groups preserves document order.
fn apply_pos_pred<'a, F>(els: Vec<ElementRef<'a>>, keep: F) -> Vec<ElementRef<'a>>
where
    F: Fn(usize, usize) -> bool, // (1-based position within the parent group, group size)
{
    let mut groups: Vec<(Option<ElementRef<'a>>, Vec<ElementRef<'a>>)> = Vec::new();
    for e in els {
        let key = e.parent().and_then(ElementRef::wrap);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push(e),
            None => groups.push((key, vec![e])),
        }
    }
    let mut out = Vec::new();
    for (_, g) in groups {
        let len = g.len();
        for (i, e) in g.into_iter().enumerate() {
            if keep(i + 1, len) {
                out.push(e);
            }
        }
    }
    out
}

fn apply_preds<'a>(mut els: Vec<ElementRef<'a>>, preds: &[Pred]) -> Result<Vec<ElementRef<'a>>> {
    for p in preds {
        match p {
            Pred::Index(n) => {
                els = apply_pos_pred(els, |pos, _| pos == *n);
            }
            Pred::Last => {
                els = apply_pos_pred(els, |pos, len| pos == len);
            }
            Pred::PosCmp(cmp, n) => {
                els = apply_pos_pred(els, |pos, _| {
                    let pos = pos as i64;
                    match cmp {
                        Cmp::Gt => pos > *n,
                        Cmp::Lt => pos < *n,
                        Cmp::Ge => pos >= *n,
                        Cmp::Le => pos <= *n,
                        Cmp::Eq => pos == *n,
                    }
                });
            }
            Pred::AttrEq(name, val) => {
                els.retain(|e| e.value().attr(name).map(|v| v == val).unwrap_or(false))
            }
            Pred::AttrExists(name) => els.retain(|e| e.value().attr(name).is_some()),
            Pred::AttrContains(name, val) => {
                els.retain(|e| e.value().attr(name).map(|v| v.contains(val.as_str())).unwrap_or(false))
            }
            Pred::AttrStartsWith(name, val) => {
                els.retain(|e| e.value().attr(name).map(|v| v.starts_with(val.as_str())).unwrap_or(false))
            }
            Pred::TextContains(val) => {
                els.retain(|e| e.text().collect::<Vec<_>>().join("").contains(val.as_str()))
            }
        }
    }
    Ok(els)
}

// ---------------------------------------------------------------------------
// Parser: expression -> steps
// ---------------------------------------------------------------------------

fn parse_steps(expr: &str) -> Result<Vec<Step>> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Err(anyhow!("empty xpath"));
    }
    let b = expr.as_bytes();
    let mut i = 0usize;
    let mut steps: Vec<Step> = Vec::new();
    // Leading axis.
    let mut axis = if expr.starts_with("//") {
        i = 2;
        Axis::DescOrSelf
    } else if expr.starts_with(".//") {
        i = 3;
        Axis::DescOrSelf
    } else if expr.starts_with("./") {
        i = 2;
        Axis::Child
    } else if expr.starts_with('/') {
        i = 1;
        Axis::Child
    } else if expr == "." {
        return Ok(vec![Step { axis: Axis::SelfStep, test: NodeTest::Any, preds: vec![] }]);
    } else {
        // Relative path ("div/a", "@href", "text()"): first step is relative
        // to the scope node. A lone @attr/text() reads the scope itself.
        Axis::Child
    };
    loop {
        if i >= b.len() {
            return Err(anyhow!("xpath {expr:?}: ends with a bare '/'"));
        }
        // "." / ".." steps carry their own axis.
        if expr[i..].starts_with("..") {
            steps.push(Step { axis: Axis::Parent, test: NodeTest::Any, preds: vec![] });
            i += 2;
        } else if expr[i..].starts_with('.') && (i + 1 >= b.len() || b[i + 1] == b'/') {
            steps.push(Step { axis: Axis::SelfStep, test: NodeTest::Any, preds: vec![] });
            i += 1;
        } else {
            // A named axis (`ancestor::div`, `following-sibling::li`, ...)
            // overrides the separator-derived axis for this step.
            if let Some((ax, ni)) = parse_axis_prefix(expr, i)? {
                axis = ax;
                i = ni;
            }
            let (test, ni) = parse_node_test(expr, i)?;
            i = ni;
            let mut preds = Vec::new();
            while i < b.len() && b[i] == b'[' {
                let (pred, nj) = parse_predicate(expr, i)?;
                preds.push(pred);
                i = nj;
            }
            // A lone @attr/text() as the whole expression reads the scope node.
            let axis = if steps.is_empty() && matches!(test, NodeTest::Attr(_) | NodeTest::Text) && axis == Axis::Child {
                Axis::SelfStep
            } else {
                axis
            };
            steps.push(Step { axis, test, preds });
        }
        if i >= b.len() {
            break;
        }
        if expr[i..].starts_with("//") {
            axis = Axis::DescOrSelf;
            i += 2;
        } else if b[i] == b'/' {
            axis = Axis::Child;
            i += 1;
        } else {
            return Err(anyhow!("xpath {expr:?}: expected '/' at {:?}", &expr[i..]));
        }
    }
    Ok(steps)
}

/// A named axis prefix at position `i` (`ancestor::`, `child::`, ...).
/// Returns None when there is no `name::` prefix. Unknown axes and the
/// unsupported `following::` / `preceding::` / `attribute::` fail loudly.
fn parse_axis_prefix(expr: &str, i: usize) -> Result<Option<(Axis, usize)>> {
    let b = expr.as_bytes();
    let mut j = i;
    while j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'-') {
        j += 1;
    }
    if j == i || !expr[j..].starts_with("::") {
        return Ok(None);
    }
    let name = &expr[i..j];
    let axis = match name {
        "child" => Axis::Child,
        "descendant" => Axis::Descendant,
        "descendant-or-self" => Axis::DescOrSelf,
        "parent" => Axis::Parent,
        "ancestor" => Axis::Ancestor,
        "ancestor-or-self" => Axis::AncestorOrSelf,
        "self" => Axis::SelfStep,
        "following-sibling" => Axis::FollowingSibling,
        "preceding-sibling" => Axis::PrecedingSibling,
        "attribute" => return Err(anyhow!("xpath {expr:?}: attribute:: axis is not supported (use @attr)")),
        "following" | "preceding" => {
            return Err(anyhow!("xpath {expr:?}: {name}:: axis is not supported (whole-document order)"))
        }
        _ => return Err(anyhow!("xpath {expr:?}: unknown axis {name}::")),
    };
    Ok(Some((axis, j + 2)))
}

fn parse_node_test(expr: &str, i: usize) -> Result<(NodeTest, usize)> {
    let b = expr.as_bytes();
    if b[i] == b'@' {
        let (name, ni) = parse_name(expr, i + 1)?;
        if name.is_empty() {
            return Err(anyhow!("xpath {expr:?}: '@' without an attribute name"));
        }
        return Ok((NodeTest::Attr(name), ni));
    }
    if expr[i..].starts_with("text()") {
        return Ok((NodeTest::Text, i + 6));
    }
    if b[i] == b'*' {
        return Ok((NodeTest::Any, i + 1));
    }
    let (name, ni) = parse_name(expr, i)?;
    if name.is_empty() {
        return Err(anyhow!("xpath {expr:?}: expected a tag name at {:?}", &expr[i..]));
    }
    Ok((NodeTest::Tag(name.to_ascii_lowercase()), ni))
}

/// `..` and `.` steps are handled directly in parse_steps (they carry
/// their own axis), so parse_node_test only sees tag/`*`/`@attr`/`text()`.
fn parse_name(expr: &str, mut i: usize) -> Result<(String, usize)> {
    let b = expr.as_bytes();
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'-' || b[i] == b':') {
        i += 1;
    }
    Ok((expr[start..i].to_string(), i))
}

fn parse_predicate(expr: &str, i: usize) -> Result<(Pred, usize)> {
    let b = expr.as_bytes();
    debug_assert_eq!(b[i], b'[');
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
        return Err(anyhow!("xpath {expr:?}: unclosed '['"));
    }
    let inner = expr[i + 1..j].trim().to_string();
    let pred = parse_predicate_inner(&inner, expr)?;
    Ok((pred, j + 1))
}

fn parse_predicate_inner(inner: &str, expr: &str) -> Result<Pred> {
    let inner = inner.trim();
    if inner == "last()" {
        return Ok(Pred::Last);
    }
    if let Some(num) = inner.strip_prefix("position()") {
        let num = num.trim();
        for (op, cmp) in [(">=", Cmp::Ge), ("<=", Cmp::Le), (">", Cmp::Gt), ("<", Cmp::Lt), ("=", Cmp::Eq)] {
            if let Some(v) = num.strip_prefix(op) {
                let n: i64 = v.trim().parse().map_err(|_| anyhow!("xpath {expr:?}: bad position number in [{inner}]"))?;
                return Ok(Pred::PosCmp(cmp, n));
            }
        }
        return Err(anyhow!("xpath {expr:?}: unsupported position() predicate [{inner}]"));
    }
    if let Ok(n) = inner.parse::<usize>() {
        return Ok(Pred::Index(n));
    }
    // contains(@attr,'v') / contains(text(),'v') / starts-with(@attr,'v')
    for (fname, is_contains) in [("contains", true), ("starts-with", false)] {
        if let Some(rest) = inner.strip_prefix(fname) {
            let rest = rest.trim().strip_prefix('(').and_then(|r| r.strip_suffix(')')).ok_or_else(|| anyhow!("xpath {expr:?}: malformed {fname}(...) in [{inner}]"))?;
            let args: Vec<&str> = split_args(rest);
            if args.len() != 2 {
                return Err(anyhow!("xpath {expr:?}: {fname} needs 2 arguments in [{inner}]"));
            }
            let val = unquote(args[1].trim()).ok_or_else(|| anyhow!("xpath {expr:?}: {fname} value must be quoted in [{inner}]"))?;
            if args[0].trim() == "text()" {
                if !is_contains {
                    return Err(anyhow!("xpath {expr:?}: starts-with(text(),...) is not supported"));
                }
                return Ok(Pred::TextContains(val));
            }
            let attr = args[0].trim().strip_prefix('@').ok_or_else(|| anyhow!("xpath {expr:?}: {fname} first argument must be @attr or text() in [{inner}]"))?;
            return Ok(if is_contains {
                Pred::AttrContains(attr.to_string(), val)
            } else {
                Pred::AttrStartsWith(attr.to_string(), val)
            });
        }
    }
    if let Some(attr) = inner.strip_prefix('@') {
        // @attr='v' or @attr="v" or bare @attr.
        if let Some(eq) = attr.find('=') {
            let name = attr[..eq].trim();
            let val = unquote(attr[eq + 1..].trim()).ok_or_else(|| anyhow!("xpath {expr:?}: attribute value must be quoted in [{inner}]"))?;
            return Ok(Pred::AttrEq(name.to_string(), val));
        }
        if !attr.trim().is_empty() && attr.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == ':') {
            return Ok(Pred::AttrExists(attr.trim().to_string()));
        }
    }
    Err(anyhow!("xpath {expr:?}: unsupported predicate [{inner}]"))
}

/// Split function-call arguments on commas outside quotes.
fn split_args(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quote: Option<u8> = None;
    let mut start = 0usize;
    for (i, c) in s.bytes().enumerate() {
        match (quote, c) {
            (None, b'\'' | b'"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, b',') => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
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
    use scraper::Html;

    const DOC: &str = r#"<html><body><div id="main">
        <div class="book"><a href="/book/1">修仙传</a><span class="author">甲</span></div>
        <div class="book"><a href="/book/2">都市传奇</a><span class="author">乙</span></div>
        <ul><li>一</li><li>二</li><li>三</li></ul>
    </div></body></html>"#;

    fn scope() -> ElementRef<'static> {
        // Leak the Html so the ElementRef can outlive the test body.
        let doc: &'static Html = Box::leak(Box::new(Html::parse_document(DOC)));
        doc.root_element()
    }

    fn els<'a>(vals: Vec<XValue<'a>>) -> Vec<ElementRef<'a>> {
        vals.into_iter().filter_map(|v| match v {
            XValue::El(e) => Some(e),
            _ => None,
        }).collect()
    }

    fn strs(vals: Vec<XValue<'_>>) -> Vec<String> {
        vals.into_iter().filter_map(|v| match v {
            XValue::Str(s) => Some(s),
            _ => None,
        }).collect()
    }

    #[test]
    fn xpath_basic() {
        let root = scope();
        assert_eq!(els(eval_xpath(root, "//div[@class='book']").unwrap()).len(), 2);
        assert_eq!(strs(eval_xpath(root, "//div[@class='book']/a/@href").unwrap()), vec!["/book/1", "/book/2"]);
        assert_eq!(strs(eval_xpath(root, "//div[@class='book']/a/text()").unwrap()), vec!["修仙传", "都市传奇"]);
        assert_eq!(strs(eval_xpath(root, "//li[2]/text()").unwrap()), vec!["二"]);
        assert_eq!(strs(eval_xpath(root, "//li[last()]/text()").unwrap()), vec!["三"]);
        assert_eq!(strs(eval_xpath(root, "//li[position()>1]/text()").unwrap()), vec!["二", "三"]);
        assert_eq!(els(eval_xpath(root, "//*[@id='main']//a").unwrap()).len(), 2);
        assert_eq!(strs(eval_xpath(root, "//a[contains(@href,'/book/2')]/text()").unwrap()), vec!["都市传奇"]);
        assert_eq!(strs(eval_xpath(root, "//div[contains(text(),'修仙')]/a/text()").unwrap()), vec!["修仙传"]);
        assert_eq!(els(eval_xpath(root, "//div[@class='book'][1]").unwrap()).len(), 1);
    }

    #[test]
    fn xpath_positional_predicates_group_by_parent() {
        // Standard XPath: `li[1]` is the first `li` of EACH parent, not
        // the first `li` in the whole document.
        let doc: &'static Html = Box::leak(Box::new(Html::parse_document(
            r#"<html><body><ul><li>A1</li><li>A2</li></ul><ul><li>B1</li><li>B2</li><li>B3</li></ul></body></html>"#,
        )));
        let root = doc.root_element();
        assert_eq!(strs(eval_xpath(root, "//li[1]/text()").unwrap()), vec!["A1", "B1"]);
        assert_eq!(strs(eval_xpath(root, "//li[2]/text()").unwrap()), vec!["A2", "B2"]);
        assert_eq!(strs(eval_xpath(root, "//li[last()]/text()").unwrap()), vec!["A2", "B3"]);
        assert_eq!(strs(eval_xpath(root, "//li[position()>1]/text()").unwrap()), vec!["A2", "B2", "B3"]);
    }

    #[test]
    fn xpath_relative_and_parent() {
        let root = scope();
        let book = els(eval_xpath(root, "//div[@class='book']").unwrap())[0];
        assert_eq!(strs(eval_xpath(book, ".//span/text()").unwrap()), vec!["甲"]);
        assert_eq!(strs(eval_xpath(book, "./a/@href").unwrap()), vec!["/book/1"]);
        assert_eq!(strs(eval_xpath(book, "a/text()").unwrap()), vec!["修仙传"]);
        // .. goes back to #main, whose id is readable from the parent.
        let parent = els(eval_xpath(book, "..").unwrap());
        assert_eq!(parent.len(), 1);
        assert_eq!(parent[0].value().attr("id"), Some("main"));
    }

    #[test]
    fn xpath_axes() {
        let root = scope();
        // ancestor:: (nearest first) and ancestor-or-self::.
        let ancs = els(eval_xpath(root, "//a[@href='/book/1']/ancestor::*").unwrap());
        assert_eq!(ancs.len(), 4);
        assert_eq!(ancs[0].value().attr("class"), Some("book"));
        assert_eq!(ancs[1].value().attr("id"), Some("main"));
        let divs = els(eval_xpath(root, "//a[@href='/book/1']/ancestor::div").unwrap());
        assert_eq!(divs.len(), 2);
        let aos = els(eval_xpath(root, "//a[@href='/book/1']/ancestor-or-self::*").unwrap());
        assert_eq!(aos.len(), 5);
        assert_eq!(aos[0].value().name(), "a");
        // following-sibling:: / preceding-sibling:: (nearest first).
        assert_eq!(strs(eval_xpath(root, "//li[1]/following-sibling::li/text()").unwrap()), vec!["二", "三"]);
        assert_eq!(strs(eval_xpath(root, "//li[3]/preceding-sibling::li/text()").unwrap()), vec!["二", "一"]);
        // parent:: / child:: / self:: / descendant:: / descendant-or-self::.
        assert_eq!(els(eval_xpath(root, "//a/parent::div").unwrap()).len(), 2);
        assert_eq!(els(eval_xpath(root, "//div[@id='main']/child::div").unwrap()).len(), 2);
        assert_eq!(els(eval_xpath(root, "//div[@id='main']/self::div").unwrap()).len(), 1);
        assert_eq!(els(eval_xpath(root, "//div[@id='main']/descendant::span").unwrap()).len(), 2);
        assert_eq!(els(eval_xpath(root, "//div[@id='main']/descendant-or-self::div").unwrap()).len(), 3);
        // Whole-document and attribute axes are rejected loudly.
        assert!(eval_xpath(root, "//a/following::div").is_err());
        assert!(eval_xpath(root, "//a/preceding::div").is_err());
        assert!(eval_xpath(root, "//a/attribute::href").is_err());
        assert!(eval_xpath(root, "//a/bogus-axis::div").is_err());
    }

    #[test]
    fn xpath_invalid() {
        let root = scope();
        assert!(eval_xpath(root, "//div[foo()]").is_err());
        assert!(eval_xpath(root, "//div/").is_err());
        assert!(eval_xpath(root, "//div[@class=").is_err());
        assert!(eval_xpath(root, "").is_err());
        assert!(eval_xpath(root, "//div/text()/a").is_err());
    }
}
