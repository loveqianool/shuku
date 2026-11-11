//! loginUi: fill a login form with JS, in the sandbox.
//!
//! The server has no browser DOM (Reader Next JS doc), so loginUi runs
//! against a minimal `document` shim built from the parsed login page:
//! `querySelector` / `querySelectorAll` / `getElementById` /
//! `getElementsByClassName` / `getElementsByTagName`, element `.value` /
//! `.textContent` / `.checked` / `.click()` / `.submit()` / `.closest()`,
//! plus the usual `java.*` pure helpers and `result` / `input` / `src`.
//! After the script runs, the filled form is submitted through the
//! engine (same per-source cookie jar) and loginCheckJs runs on the
//! submit response.
//!
//! Supported selector subset (enough for real login forms): `tag`,
//! `#id`, `.class`, `[attr]`, `[attr=v]`, descendant `A B`, child `A > B`,
//! comma lists. No pseudo-classes, no `attribute::` axis, no navigation
//! (`location`), no `fetch`/`XMLHttpRequest` — scripts using anything
//! else fail loudly instead of silently doing nothing.

use anyhow::{anyhow, Context, Result};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

use crate::source::{strip_js_wrapper, JS_HELPERS, JS_MEMORY_LIMIT, JS_TIMEOUT};

#[derive(Debug, Clone)]
pub struct FormSubmit {
    pub url: String,
    pub method: String,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ElRec {
    tag: String,
    id: String,
    name: String,
    #[serde(rename = "type")]
    typ: String,
    cls: Vec<String>,
    attrs: HashMap<String, String>,
    text: String,
    value: String,
    checked: bool,
    form: i64,
    parent: i64,
    options: Vec<(String, String)>,
    selected: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FormRec {
    action: String,
    method: String,
    controls: Vec<usize>,
}

/// The `document` shim. Element wrappers share state with the `__els`
/// table (value/checked are accessors over the table record), so after
/// the script runs the table holds the filled values.
const DOCUMENT_SHIM: &str = r##"
function __matchCompound(el, sel) {
  var i = 0, tag = null;
  var m = sel.match(/^([a-zA-Z][a-zA-Z0-9_-]*|\*)/);
  if (m) { tag = m[1]; i = m[1].length; }
  if (tag && tag !== "*" && el.tag !== tag.toLowerCase()) return false;
  while (i < sel.length) {
    var c = sel[i];
    if (c === "#") { var j = i + 1; while (j < sel.length && /[A-Za-z0-9_-]/.test(sel[j])) j++; if (el.id !== sel.slice(i + 1, j)) return false; i = j; }
    else if (c === ".") { var j = i + 1; while (j < sel.length && /[A-Za-z0-9_-]/.test(sel[j])) j++; if (el.cls.indexOf(sel.slice(i + 1, j)) < 0) return false; i = j; }
    else if (c === "[") { var j = sel.indexOf("]", i); if (j < 0) return false; var inner = sel.slice(i + 1, j); var eq = inner.indexOf("="); if (eq < 0) { if (!(inner.trim() in el.attrs)) return false; } else { var k = inner.slice(0, eq).trim(); var v = inner.slice(eq + 1).trim().replace(/^['"]|['"]$/g, ""); if (el.attrs[k] !== v) return false; } i = j + 1; }
    else return false;
  }
  return true;
}
function __parseChain(sel) {
  var parts = [], cur = "", comb = " ";
  for (var i = 0; i < sel.length; i++) {
    var c = sel[i];
    if (c === ">") { if (cur.trim()) parts.push({sel: cur.trim(), comb: comb}); cur = ""; comb = ">"; }
    else if (c === " ") { if (cur.trim()) { parts.push({sel: cur.trim(), comb: comb}); cur = ""; comb = " "; } }
    else if (c === "[") { var j = sel.indexOf("]", i); cur += sel.slice(i, j + 1); i = j; }
    else if (c === "'" || c === '"') { var j = sel.indexOf(c, i + 1); cur += sel.slice(i, j + 1); i = j; }
    else cur += c;
  }
  if (cur.trim()) parts.push({sel: cur.trim(), comb: comb});
  return parts;
}
function __matchChain(idx, parts) {
  var cur = idx, p = parts.length - 1;
  if (!__matchCompound(__els[cur], parts[p].sel)) return false;
  while (p > 0) {
    var comb = parts[p].comb; p--;
    if (comb === ">") { cur = __els[cur].parent; if (cur < 0 || !__matchCompound(__els[cur], parts[p].sel)) return false; }
    else { var found = false, a = __els[cur].parent; while (a >= 0) { if (__matchCompound(__els[a], parts[p].sel)) { cur = a; found = true; break; } a = __els[a].parent; } if (!found) return false; }
  }
  return true;
}
function __queryAll(sel) {
  var out = [];
  var groups = String(sel).split(",");
  for (var g = 0; g < groups.length; g++) {
    var parts = __parseChain(groups[g].trim());
    if (!parts.length) continue;
    for (var i = 0; i < __els.length; i++) if (__matchChain(i, parts)) out.push(i);
  }
  return out;
}
function __wrap(i) {
  var rec = __els[i];
  var el = { __idx: i, tagName: rec.tag.toUpperCase(), id: rec.id || "", name: rec.name || "", type: rec.type || "" };
  Object.defineProperty(el, "value", { get: function(){ return rec.value; }, set: function(v){ rec.value = String(v); } });
  Object.defineProperty(el, "checked", { get: function(){ return !!rec.checked; }, set: function(v){ rec.checked = !!v; } });
  Object.defineProperty(el, "textContent", { get: function(){ return rec.text; }, set: function(v){ rec.text = String(v); } });
  el.getAttribute = function(n){ var v = rec.attrs[n]; return v === undefined ? null : v; };
  el.setAttribute = function(n, v){ rec.attrs[n] = String(v); };
  el.click = function(){ __clicked = i; if (rec.form >= 0) __submittedForm = rec.form; };
  el.submit = function(){ if (rec.form >= 0) __submittedForm = rec.form; };
  el.closest = function(s){ var p = i; while (p >= 0) { if (__matchCompound(__els[p], s)) return __wrap(p); p = __els[p].parent; } return null; };
  el.querySelector = function(s){ var r = __queryWithin(i, s); return r.length ? r[0] : null; };
  el.querySelectorAll = function(s){ return __queryWithin(i, s); };
  return el;
}
function __queryWithin(root, sel) {
  var out = [];
  var groups = String(sel).split(",");
  for (var g = 0; g < groups.length; g++) {
    var parts = __parseChain(groups[g].trim());
    if (!parts.length) continue;
    for (var i = 0; i < __els.length; i++) {
      if (i === root) continue;
      var p = __els[i].parent, inside = false;
      while (p >= 0) { if (p === root) { inside = true; break; } p = __els[p].parent; }
      if (inside && __matchChain(i, parts)) out.push(__wrap(i));
    }
  }
  return out;
}
function __wrapForm(fi) {
  var f = __forms[fi];
  return { action: f.action, method: f.method, submit: function(){ __submittedForm = fi; }, elements: f.controls.map(function(ci){ return __wrap(ci); }) };
}
var document = {
  querySelector: function(s){ var r = __queryAll(s); return r.length ? __wrap(r[0]) : null; },
  querySelectorAll: function(s){ return __queryAll(s).map(function(i){ return __wrap(i); }); },
  getElementById: function(id){ for (var i = 0; i < __els.length; i++) if (__els[i].id === id) return __wrap(i); return null; },
  getElementsByClassName: function(c){ var o = []; for (var i = 0; i < __els.length; i++) if (__els[i].cls.indexOf(c) >= 0) o.push(__wrap(i)); return o; },
  getElementsByTagName: function(t){ var o = []; var lt = String(t).toLowerCase(); for (var i = 0; i < __els.length; i++) if (__els[i].tag === lt) o.push(__wrap(i)); return o; },
  forms: __forms.map(function(_, fi){ return __wrapForm(fi); })
};
"##;

/// Run a loginUi script against the login page. Returns the form submit
/// to perform, or Ok(None) when the page has no form at all. Blocking:
/// call from spawn_blocking only.
pub fn run_login_ui(html: &str, script: &str, js_lib: &str, page_url: &Url) -> Result<Option<FormSubmit>> {
    let (els, forms) = build_dom(html);
    if forms.is_empty() {
        return Ok(None);
    }
    let els_json = serde_json::to_string(&els)?;
    let forms_json = serde_json::to_string(&forms)?;
    let script = strip_js_wrapper(script.trim()).to_string();
    let html_json = serde_json::to_string(html)?;
    let js_lib = js_lib.to_string();

    let rt = rquickjs::Runtime::new().context("create js runtime")?;
    rt.set_memory_limit(JS_MEMORY_LIMIT);
    rt.set_max_stack_size(256 * 1024);
    let deadline = std::time::Instant::now() + JS_TIMEOUT;
    rt.set_interrupt_handler(Some(Box::new(move || std::time::Instant::now() > deadline)));
    let ctx = rquickjs::Context::full(&rt).context("create js context")?;
    let script_json = serde_json::to_string(&script)?;
    let setup = format!(
        "{js_lib} let result = {html_json}; let input = result; let src = result; {JS_HELPERS} var __els = {els_json}; var __forms = {forms_json}; var __clicked = -1; var __submittedForm = -1; {DOCUMENT_SHIM}"
    );
    let code = format!(
        "(function(){{ try {{ {setup} let __out; try {{ __out = eval({script_json}); }} catch(e) {{ if (String(e).indexOf('return') !== -1) {{ __out = (function(){{ {setup} {script} }})(); }} else {{ throw e; }} }} return JSON.stringify({{els: __els, clicked: __clicked, submittedForm: __submittedForm}}); }} catch(e) {{ return 'LOGINUI_ERROR:' + String(e); }} }})()"
    );
    let out: String = ctx
        .with(|ctx| ctx.eval::<String, _>(code))
        .map_err(|e| anyhow!("loginUi exec: {e}"))?;
    if let Some(msg) = out.strip_prefix("LOGINUI_ERROR:") {
        return Err(anyhow!("loginUi exec: {msg}"));
    }
    let state: serde_json::Value = serde_json::from_str(&out)?;
    let els: Vec<ElRec> = serde_json::from_value(state["els"].clone())?;
    let clicked = state["clicked"].as_i64().unwrap_or(-1);
    let submitted = state["submittedForm"].as_i64().unwrap_or(-1);
    Ok(build_submit(&els, &forms, clicked, submitted, page_url))
}

fn build_submit(els: &[ElRec], forms: &[FormRec], clicked: i64, submitted: i64, page_url: &Url) -> Option<FormSubmit> {
    // Which form: the one submitted/clicked, else the first form with a
    // password field, else the first form.
    let mut form_idx: i64 = if submitted >= 0 {
        submitted
    } else if clicked >= 0 && els.get(clicked as usize).map(|e| e.form).unwrap_or(-1) >= 0 {
        els[clicked as usize].form
    } else {
        -1
    };
    if form_idx < 0 {
        for (i, f) in forms.iter().enumerate() {
            if f.controls.iter().any(|&ci| els[ci].tag == "input" && els[ci].typ == "password") {
                form_idx = i as i64;
                break;
            }
        }
        if form_idx < 0 {
            form_idx = 0;
        }
    }
    let form = forms.get(form_idx as usize)?;
    let mut fields: Vec<(String, String)> = Vec::new();
    for &ci in &form.controls {
        let el = &els[ci];
        if el.name.is_empty() {
            continue;
        }
        match el.tag.as_str() {
            "input" => match el.typ.as_str() {
                "checkbox" | "radio" => {
                    if el.checked {
                        fields.push((el.name.clone(), el.value.clone()));
                    }
                }
                "file" | "submit" | "button" | "image" | "reset" => {}
                _ => fields.push((el.name.clone(), el.value.clone())),
            },
            "select" => fields.push((el.name.clone(), select_value(el))),
            "textarea" => fields.push((el.name.clone(), el.value.clone())),
            _ => {}
        }
    }
    let method = {
        let m = form.method.trim().to_ascii_uppercase();
        if m == "POST" { "POST".to_string() } else { "GET".to_string() }
    };
    let url = page_url
        .join(&form.action)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| page_url.to_string());
    let body = form_urlencode(&fields);
    if method == "POST" {
        Some(FormSubmit { url, method, body: Some(body) })
    } else if body.is_empty() {
        Some(FormSubmit { url, method, body: None })
    } else {
        let sep = if url.contains('?') { "&" } else { "?" };
        Some(FormSubmit { url: format!("{url}{sep}{body}"), method, body: None })
    }
}

fn select_value(el: &ElRec) -> String {
    // A script-set value wins when it matches an option; otherwise the
    // tracked selected option.
    if el.options.iter().any(|(v, _)| *v == el.value) {
        return el.value.clone();
    }
    el.options
        .get(el.selected.max(0) as usize)
        .map(|(v, _)| v.clone())
        .unwrap_or_else(|| el.value.clone())
}

fn form_urlencode(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode_component(k), urlencode_component(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn urlencode_component(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Parse the login page into the element table + form list.
fn build_dom(html: &str) -> (Vec<ElRec>, Vec<FormRec>) {
    let doc = Html::parse_document(html);
    let sel_all = Selector::parse("*").unwrap();
    let refs: Vec<ElementRef> = doc.select(&sel_all).collect();
    let option_sel = Selector::parse("option").unwrap();
    let mut recs: Vec<ElRec> = Vec::with_capacity(refs.len());
    for el in &refs {
        let v = el.value();
        let tag = v.name().to_string();
        let attrs: HashMap<String, String> = v.attrs().map(|(k, val)| (k.to_string(), val.to_string())).collect();
        let id = attrs.get("id").cloned().unwrap_or_default();
        let name = attrs.get("name").cloned().unwrap_or_default();
        let typ = attrs.get("type").cloned().unwrap_or_default().to_ascii_lowercase();
        let cls: Vec<String> = attrs
            .get("class")
            .map(|c| c.split_whitespace().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let text = el.text().collect::<Vec<_>>().join("").trim().to_string();
        let parent = el
            .parent()
            .and_then(ElementRef::wrap)
            .and_then(|pe| refs.iter().position(|e| *e == pe))
            .map(|i| i as i64)
            .unwrap_or(-1);
        let (value, checked, options, selected) = match tag.as_str() {
            "input" => (
                attrs.get("value").cloned().unwrap_or_default(),
                attrs.contains_key("checked"),
                Vec::new(),
                -1,
            ),
            "textarea" => (el.text().collect::<Vec<_>>().join(""), false, Vec::new(), -1),
            "select" => {
                let mut options: Vec<(String, String)> = Vec::new();
                let mut selected = -1i64;
                for (i, opt) in el.select(&option_sel).enumerate() {
                    let ov = opt.value();
                    let val = ov.attr("value").map(|s| s.to_string()).unwrap_or_else(|| opt.text().collect::<Vec<_>>().join("").trim().to_string());
                    if ov.attr("selected").is_some() && selected < 0 {
                        selected = i as i64;
                    }
                    options.push((val, opt.text().collect::<Vec<_>>().join("").trim().to_string()));
                }
                let sel_idx = if selected >= 0 { selected } else if options.is_empty() { -1 } else { 0 };
                let value = if sel_idx >= 0 { options[sel_idx as usize].0.clone() } else { String::new() };
                (value, false, options, sel_idx)
            }
            "button" => (attrs.get("value").cloned().unwrap_or_default(), false, Vec::new(), -1),
            _ => (String::new(), false, Vec::new(), -1),
        };
        recs.push(ElRec { tag, id, name, typ, cls, attrs, text, value, checked, form: -1, parent, options, selected });
    }
    // Forms, in document order; each element's nearest ancestor form.
    let mut forms: Vec<FormRec> = Vec::new();
    let mut form_of: Vec<i64> = vec![-1; recs.len()];
    for (i, rec) in recs.iter().enumerate() {
        if rec.tag == "form" {
            forms.push(FormRec {
                action: rec.attrs.get("action").cloned().unwrap_or_default(),
                method: rec.attrs.get("method").cloned().unwrap_or_default(),
                controls: Vec::new(),
            });
            form_of[i] = (forms.len() - 1) as i64;
        }
    }
    for i in 0..recs.len() {
        if form_of[i] >= 0 {
            continue;
        }
        let mut p = recs[i].parent;
        while p >= 0 {
            if form_of[p as usize] >= 0 {
                form_of[i] = form_of[p as usize];
                break;
            }
            p = recs[p as usize].parent;
        }
    }
    for (i, rec) in recs.iter_mut().enumerate() {
        rec.form = form_of[i];
        if matches!(rec.tag.as_str(), "input" | "select" | "textarea" | "button") && form_of[i] >= 0 {
            forms[form_of[i] as usize].controls.push(i);
        }
    }
    (recs, forms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN_HTML: &str = r#"<html><body>
<form id="loginForm" action="/login" method="post">
  <input type="text" name="username" value="">
  <input type="password" name="password">
  <input type="hidden" name="token" value="abc123">
  <input type="checkbox" name="remember" checked>
  <button class="login-btn" type="submit">登录</button>
</form></body></html>"#;

    #[test]
    fn login_ui_fills_and_submits() {
        let url = Url::parse("https://example.com/login").unwrap();
        let script = r#"document.querySelector('input[name=username]').value = 'alice';
document.querySelector('input[name=password]').value = 's3cret';
document.querySelector('.login-btn').click();"#;
        let submit = run_login_ui(LOGIN_HTML, script, "", &url).unwrap().expect("submit");
        assert_eq!(submit.method, "POST");
        assert_eq!(submit.url, "https://example.com/login");
        let body = submit.body.unwrap();
        assert!(body.contains("username=alice"), "{body}");
        assert!(body.contains("password=s3cret"), "{body}");
        assert!(body.contains("token=abc123"), "{body}");
        assert!(body.contains("remember="), "{body}");
    }

    #[test]
    fn login_ui_get_form_and_id_selector() {
        let html = r#"<html><body><form action="/go" method="get">
<input type="text" id="u" name="q"><input type="submit" value="Go"></form></body></html>"#;
        let url = Url::parse("https://example.com/search").unwrap();
        let submit = run_login_ui(html, "document.getElementById('u').value = 'x y'; document.forms[0].submit();", "", &url)
            .unwrap()
            .expect("submit");
        assert_eq!(submit.method, "GET");
        assert_eq!(submit.url, "https://example.com/go?q=x+y");
        assert!(submit.body.is_none());
    }

    #[test]
    fn login_ui_no_form_is_none() {
        let url = Url::parse("https://example.com/").unwrap();
        assert!(run_login_ui("<html><body>hi</body></html>", "1", "", &url).unwrap().is_none());
    }

    #[test]
    fn login_ui_unsupported_api_errors() {
        let url = Url::parse("https://example.com/login").unwrap();
        let err = run_login_ui(LOGIN_HTML, "document.querySelector('input').value = 'x'; window.location.href = '/';", "", &url).unwrap_err();
        assert!(err.to_string().contains("window"), "{err}");
    }
}
