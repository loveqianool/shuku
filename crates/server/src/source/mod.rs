//! Legado (Reading 3) JSON book-source engine.
//! - Rule model: string rules (selector chain with `&&`, `@attr`, `##regex##repl`,
//!   `js:` scripts) and object rules ({bookList, name, ...}).
//! - Html is parsed once per fetch; Selector/Regex are precompiled and cached.
//! - rquickjs sandbox: string cleaning only — no host functions, no modules,
//!   memory limit + interrupt-handler timeout. All HTML/JS work runs inside
//!   spawn_blocking (callers are responsible).

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;
use url::Url;

use crate::cache::{Cache, CacheKind};

// ---------------------------------------------------------------------------
// Source model (Reading 3 JSON subset)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceDef {
    pub id: String,
    pub name: String,
    pub url: String,
    pub group: String,
    #[serde(default)]
    pub source_type: i64,
    #[serde(default)]
    pub search_url: String,
    #[serde(default)]
    pub explore_url: String,
    #[serde(default)]
    pub book_url_pattern: String,
    #[serde(default)]
    pub rule_search: Value,
    #[serde(default)]
    pub rule_explore: Value,
    #[serde(default)]
    pub rule_book_info: Value,
    #[serde(default)]
    pub rule_toc: Value,
    #[serde(default)]
    pub rule_content: Value,
    #[serde(default)]
    pub js_lib: String,
    #[serde(default)]
    pub login_url: String,
    #[serde(default)]
    pub login_check_js: String,
    #[serde(default)]
    pub login_ui: String,
    /// Global request headers, JSON object string, e.g. {"User-Agent": "..."}.
    #[serde(default)]
    pub header: String,
    /// Per-source cookie jar (Legado `enabledCookieJar`, default true).
    #[serde(default = "default_true")]
    pub enabled_cookie_jar: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub raw: Value,
}

fn default_true() -> bool {
    true
}

impl SourceDef {
    pub fn from_raw(raw: &Value) -> Self {
        let s = |k: &str| raw.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let rule = |k: &str| raw.get(k).cloned().unwrap_or(Value::Null);
        Self {
            id: s("bookSourceUrl"),
            name: s("bookSourceName"),
            url: s("bookSourceUrl"),
            group: s("bookSourceGroup"),
            source_type: raw.get("bookSourceType").and_then(|v| v.as_i64()).unwrap_or(0),
            search_url: s("searchUrl"),
            explore_url: s("exploreUrl"),
            book_url_pattern: s("bookUrlPattern"),
            rule_search: rule("ruleSearch"),
            rule_explore: rule("ruleExplore"),
            rule_book_info: rule("ruleBookInfo"),
            rule_toc: rule("ruleToc"),
            rule_content: rule("ruleContent"),
            js_lib: s("jsLib"),
            login_url: s("loginUrl"),
            login_check_js: s("loginCheckJs"),
            login_ui: s("loginUi"),
            header: s("header"),
            enabled_cookie_jar: raw.get("enabledCookieJar").and_then(|v| v.as_bool()).unwrap_or(true),
            enabled: raw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
            raw: raw.clone(),
        }
    }

    /// Base URL for resolving relative request-rule URLs (bookSourceUrl).
    pub fn base_url(&self) -> Url {
        Url::parse(&self.url).unwrap_or_else(|_| Url::parse("https://example.com/").unwrap())
    }
}

// ---------------------------------------------------------------------------
// Precompiled selector / regex caches
// ---------------------------------------------------------------------------

static SELECTOR_CACHE: LazyLock<RwLock<HashMap<String, Arc<Selector>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static REGEX_CACHE: LazyLock<RwLock<HashMap<String, Arc<Regex>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

pub fn get_selector(s: &str) -> Result<Arc<Selector>> {
    if let Some(v) = SELECTOR_CACHE.read().unwrap().get(s) {
        return Ok(v.clone());
    }
    let sel = Selector::parse(s).map_err(|e| anyhow!("invalid selector {s:?}: {e:?}"))?;
    let sel = Arc::new(sel);
    SELECTOR_CACHE.write().unwrap().insert(s.to_string(), sel.clone());
    Ok(sel)
}

pub fn get_regex(s: &str) -> Result<Arc<Regex>> {
    if let Some(v) = REGEX_CACHE.read().unwrap().get(s) {
        return Ok(v.clone());
    }
    let re = Regex::new(s).map_err(|e| anyhow!("invalid regex {s:?}: {e}"))?;
    let re = Arc::new(re);
    REGEX_CACHE.write().unwrap().insert(s.to_string(), re.clone());
    Ok(re)
}

// ---------------------------------------------------------------------------
// Rule validation (run when a source is saved)
// ---------------------------------------------------------------------------

fn validate_rule_value(v: &Value) -> Result<()> {
    match v {
        Value::String(s) => validate_rule_string(s),
        Value::Object(map) => {
            for (k, sub) in map {
                // JS-code fields are scripts, not selectors — validate syntax.
                if matches!(k.as_str(), "webJs" | "preUpdateJs" | "formatJs" | "init") {
                    if let Value::String(s) = sub {
                        if !s.trim().is_empty() {
                            validate_js_syntax(strip_js_wrapper(s))
                                .map_err(|e| anyhow!("{k}: {e}"))?;
                        }
                    }
                    continue;
                }
                // checkKeyWord is a plain string (the debug interface's
                // default keyword per the source doc) — not a selector.
                if k == "checkKeyWord" {
                    continue;
                }
                validate_rule_value(sub)?;
            }
            Ok(())
        }
        Value::Array(arr) => {
            for sub in arr {
                validate_rule_value(sub)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_rule_string(s: &str) -> Result<()> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(());
    }
    if let Some(js) = s.strip_prefix("js:") {
        return validate_js_syntax(js).map_err(|e| anyhow!("invalid js rule: {e}"));
    }
    if s.starts_with("<js>") && s.ends_with("</js>") {
        return validate_js_syntax(strip_js_wrapper(s)).map_err(|e| anyhow!("invalid js rule: {e}"));
    }
    let (core, regex, _replacement) = split_rule_parts(s);
    // `@js:` postfix: validate the head as a rule and the tail as JS.
    let (core, post_js) = match core.find("@js:") {
        Some(i) => (&core[..i], Some(core[i + 4..].trim())),
        None => (core, None),
    };
    if let Some(js) = post_js {
        if !js.is_empty() {
            validate_js_syntax(js).map_err(|e| anyhow!("invalid @js rule: {e}"))?;
        }
    }
    let core = core.trim();
    if let Some(xp) = core.strip_prefix("@xpath:") {
        crate::xpath::validate_xpath(xp).map_err(|e| anyhow!("invalid xpath rule: {e}"))?;
    } else if core.starts_with('/') || core.starts_with("./") {
        crate::xpath::validate_xpath(core).map_err(|e| anyhow!("invalid xpath rule: {e}"))?;
    } else if let Some(jp) = core.strip_prefix("@json:") {
        crate::jsonpath::validate_json_path(jp).map_err(|e| anyhow!("invalid @json rule: {e}"))?;
    } else if core.starts_with('$') {
        crate::jsonpath::validate_json_path(core).map_err(|e| anyhow!("invalid @json rule: {e}"))?;
    } else if let Some(rx) = core.strip_prefix("@regex:").or_else(|| core.strip_prefix(':')) {
        get_regex(rx).map_err(|e| anyhow!("invalid @regex rule: {e}"))?;
    } else {
        // CSS selector chain; @css: prefix is optional.
        let core = core.strip_prefix("@css:").unwrap_or(core);
        let segs: Vec<&str> = core.split("&&").collect();
        for (i, seg) in segs.iter().enumerate() {
            let seg = seg.trim();
            if seg.is_empty() {
                continue;
            }
            // Only the last segment may carry @attr.
            let sel_part = if i == segs.len() - 1 {
                seg.split('@').next().unwrap_or(seg)
            } else {
                seg
            };
            if !sel_part.trim().is_empty() {
                get_selector(sel_part.trim())?;
            }
        }
    }
    if let Some(pat) = regex {
        get_regex(pat)?;
    }
    Ok(())
}

/// Validate a whole source JSON (called on save/import). Checks selectors,
/// regexes, JS syntax, request-rule templates and the `header` JSON.
pub fn validate_source(raw: &Value) -> Result<()> {
    // A source is a JSON object with an identity: anything else (a bare
    // string, a number, an array) must not slip through as a nameless
    // source with a generated UUID id.
    if !raw.is_object() {
        return Err(anyhow!("source must be a JSON object"));
    }
    for key in ["bookSourceUrl", "bookSourceName"] {
        let ok = raw.get(key).and_then(|v| v.as_str()).map(|s| !s.trim().is_empty()).unwrap_or(false);
        if !ok {
            return Err(anyhow!("{key} is required"));
        }
    }
    for key in ["ruleSearch", "ruleExplore", "ruleBookInfo", "ruleToc", "ruleContent"] {
        if let Some(v) = raw.get(key) {
            validate_rule_value(v).map_err(|e| anyhow!("{key}: {e}"))?;
        }
    }
    if let Some(js) = raw.get("jsLib").and_then(|v| v.as_str()) {
        if !js.trim().is_empty() {
            validate_js_syntax(js).map_err(|e| anyhow!("jsLib: {e}"))?;
        }
    }
    // Request rules: render with a dummy keyword so both `{key}` and
    // `{{key}}` forms validate (plain Url::parse rejects `{key}` outright).
    // Relative URLs resolve against the source's own bookSourceUrl.
    let base = raw
        .get("bookSourceUrl")
        .and_then(|v| v.as_str())
        .and_then(|s| Url::parse(s).ok())
        .unwrap_or_else(|| Url::parse("https://example.com/").unwrap());
    for key in ["searchUrl", "exploreUrl", "loginUrl"] {
        if let Some(u) = raw.get(key).and_then(|v| v.as_str()) {
            if !u.trim().is_empty() {
                let js_lib = raw.get("jsLib").and_then(|v| v.as_str()).unwrap_or("");
                parse_request_rule(u, &base, "test", 1, js_lib)
                    .map_err(|e| anyhow!("{key}: {e}"))?;
            }
        }
    }
    if let Some(u) = raw.get("loginCheckJs").and_then(|v| v.as_str()) {
        if !u.trim().is_empty() {
            validate_js_syntax(u).map_err(|e| anyhow!("loginCheckJs: {e}"))?;
        }
    }
    if let Some(u) = raw.get("loginUi").and_then(|v| v.as_str()) {
        if !u.trim().is_empty() {
            validate_js_syntax(strip_js_wrapper(u)).map_err(|e| anyhow!("loginUi: {e}"))?;
        }
    }
    if let Some(h) = raw.get("header").and_then(|v| v.as_str()) {
        if !h.trim().is_empty() {
            parse_header_json(h).map_err(|e| anyhow!("header: {e}"))?;
        }
    }
    // ruleContent pipeline fields (webJs is already syntax-checked by the
    // object walk above).
    if let Some(rule) = raw.get("ruleContent").and_then(|v| v.as_object()) {
        for key in ["sourceRegex", "replaceRegex"] {
            if let Some(s) = rule.get(key).and_then(|v| v.as_str()) {
                if !s.trim().is_empty() {
                    let pat = s.split("##").next().unwrap_or("");
                    get_regex(pat).map_err(|e| anyhow!("ruleContent.{key}: {e}"))?;
                }
            }
        }
        if let Some(s) = rule.get("nextContentUrl").and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                validate_rule_string(s).map_err(|e| anyhow!("ruleContent.nextContentUrl: {e}"))?;
            }
        }
    }
    if let Some(rule) = raw.get("ruleToc").and_then(|v| v.as_object()) {
        if let Some(s) = rule.get("nextTocUrl").and_then(|v| v.as_str()) {
            if !s.trim().is_empty() {
                validate_rule_string(s).map_err(|e| anyhow!("ruleToc.nextTocUrl: {e}"))?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rquickjs sandbox — string cleaning only
// ---------------------------------------------------------------------------

pub const JS_TIMEOUT: Duration = Duration::from_secs(5);
pub const JS_MEMORY_LIMIT: usize = 16 * 1024 * 1024;

/// Syntax-only check: compile via `new Function(body)` without executing.
pub fn validate_js_syntax(script: &str) -> Result<()> {
    let script = script.to_string();
    run_js_inner(&script, "", &[], "", Duration::from_secs(2), true, "", "")
        .map(|_| ())
        .map_err(|e| anyhow!("js syntax error: {e}"))
}

/// Execute a cleaning script. `input` is bound as `result` (aliases
/// `input`, `src`); `base_url` / `baseUrl` / `url` hold the page URL.
/// The script's return value (or the mutated `result`) is the output.
/// No host functions, no modules, no network/file/timer access — QuickJS
/// has none by default. Blocking: call from spawn_blocking only.
pub fn run_js(script: &str, input: &str, js_lib: &str, timeout: Duration) -> Result<String> {
    run_js_inner(script, input, &[], js_lib, timeout, false, "", "")
}

/// Like `run_js`, but `vars` are bound as additional string globals
/// (e.g. `key`, `page` for `{{...}}` expressions in URL templates).
/// Variable names must be valid JS identifiers.
pub fn run_js_vars(
    script: &str,
    input: &str,
    vars: &[(&str, &str)],
    js_lib: &str,
    timeout: Duration,
) -> Result<String> {
    run_js_inner(script, input, vars, js_lib, timeout, false, "", "")
}

/// Like `run_js`, but with the page base URL bound (`base_url` /
/// `baseUrl` / `url`) and an extra `prelude` of JS run before the script
/// (used by formatJs to bind `index` / `title` / `chapter`).
pub fn run_js_ext(
    script: &str,
    input: &str,
    js_lib: &str,
    base: &str,
    prelude: &str,
    timeout: Duration,
) -> Result<String> {
    run_js_inner(script, input, &[], js_lib, timeout, false, base, prelude)
}

fn run_js_inner(script: &str, input: &str, vars: &[(&str, &str)], js_lib: &str, timeout: Duration, syntax_only: bool, base: &str, prelude: &str) -> Result<String> {
    let rt = rquickjs::Runtime::new().context("create js runtime")?;
    rt.set_memory_limit(JS_MEMORY_LIMIT);
    rt.set_max_stack_size(256 * 1024);
    let deadline = std::time::Instant::now() + timeout;
    rt.set_interrupt_handler(Some(Box::new(move || std::time::Instant::now() > deadline)));
    let ctx = rquickjs::Context::full(&rt).context("create js context")?;
    let script = script.to_string();
    let input_json = serde_json::to_string(input)?;
    let js_lib = js_lib.to_string();
    let base_json = serde_json::to_string(base)?;
    let prelude = prelude.to_string();
    let vars_code: String = vars
        .iter()
        .map(|(k, v)| format!("let {} = {};", k, serde_json::to_string(v).unwrap_or_default()))
        .collect();
    ctx.with(|ctx| {
        if syntax_only {
            // Compile only: `new Function(body)` never runs the body.
            let code = format!("new Function({})", serde_json::to_string(&script)?);
            let _f: rquickjs::Function = ctx.eval(code).map_err(|e| anyhow!("js compile: {e}"))?;
            return Ok(String::new());
        }
        // Semantics: `result` holds the input string. The script is run via
        // direct eval so its completion value is the output (expression
        // scripts like `result.toUpperCase()` work); if it only mutates
        // `result`, the mutated value is used. Scripts using `return` are
        // retried inside a function wrapper (eval forbids top-level return).
        // The prelude (jsLib + vars + aliases + java.* helpers + the
        // caller's prelude) is shared by both paths so `return`-style
        // scripts see the same bindings.
        // A script returning an array/object yields its JSON text (the
        // source doc: js: rules returning arrays/objects are handed to
        // the next stage as JSON strings).
        let script_json = serde_json::to_string(&script)?;
        let setup = format!(
            "{js_lib} {vars_code} let result = {input_json}; let input = result; let src = result; let base_url = {base_json}; let baseUrl = base_url; let url = base_url; {JS_HELPERS} {prelude}"
        );
        let code = format!(
            "(function(){{ try {{ {setup} let __out; try {{ __out = eval({script_json}); }} catch(e) {{ if (String(e).indexOf('return') !== -1) {{ __out = (function(){{ {setup} {script} }})(); }} else {{ throw e; }} }} return (typeof __out === 'object' && __out !== null) ? JSON.stringify(__out) : String(__out === undefined ? result : __out); }} catch(e) {{ return 'JS_ERROR:' + String(e); }} }})()"
        );
        let out: String = ctx.eval(code).map_err(|e| anyhow!("js exec: {e}"))?;
        if let Some(msg) = out.strip_prefix("JS_ERROR:") {
            return Err(anyhow!("js exec: {msg}"));
        }
        Ok(out)
    })
}

/// Pure-JS helpers available to every rule script (Reader Next JS doc):
/// `java.md5Encode` / `base64Encode` / `base64Decode` /
/// `encodeURIComponent` / `decodeURIComponent` / `encodeURI` / `decodeURI` /
/// `timeFormat` / `now` / `uuid`, plus the globals `regex_replace` and
/// `strip_ws`. Deliberately absent (sandbox rule: no network from JS):
/// `java.ajax`, `java.get`, `java.post`, `java.put`, `kv_get`/`kv_put`,
/// `cache`, `cookie`, `document`, `window`.
pub(crate) const JS_HELPERS: &str = r##"
function __utf8enc(s){var o="";for(var i=0;i<s.length;i++){var c=s.charCodeAt(i);if(c<128)o+=String.fromCharCode(c);else if(c<2048)o+=String.fromCharCode((c>>6)|192,(c&63)|128);else if(c>=0xD800&&c<=0xDBFF){var c2=s.charCodeAt(++i);var cp=((c-0xD800)<<10)+(c2-0xDC00)+0x10000;o+=String.fromCharCode((cp>>18)|240,((cp>>12)&63)|128,((cp>>6)&63)|128,(cp&63)|128);}else o+=String.fromCharCode((c>>12)|224,((c>>6)&63)|128,(c&63)|128);}return o;}
function __utf8dec(s){var o="",i=0;while(i<s.length){var c=s.charCodeAt(i++);if(c<128)o+=String.fromCharCode(c);else if(c>191&&c<224){var c2=s.charCodeAt(i++);o+=String.fromCharCode(((c&31)<<6)|(c2&63));}else if(c>239&&c<244){var c2=s.charCodeAt(i++),c3=s.charCodeAt(i++),c4=s.charCodeAt(i++);var cp=(((c&7)<<18)|((c2&63)<<12)|((c3&63)<<6)|(c4&63))-0x10000;o+=String.fromCharCode(0xD800+(cp>>10),0xDC00+(cp&1023));}else{var c2=s.charCodeAt(i++),c3=s.charCodeAt(i++);o+=String.fromCharCode(((c&15)<<12)|((c2&63)<<6)|(c3&63));}}return o;}
function __b64e(s){var u=__utf8enc(String(s));var ch="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";var out="";for(var i=0;i<u.length;i+=3){var n=(u.charCodeAt(i)<<16)|((u.charCodeAt(i+1)||0)<<8)|(u.charCodeAt(i+2)||0);out+=ch[(n>>18)&63]+ch[(n>>12)&63]+(i+1<u.length?ch[(n>>6)&63]:"=")+(i+2<u.length?ch[n&63]:"=");}return out;}
function __b64d(s){var ch="ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";var c=String(s).replace(/[^A-Za-z0-9+/=]/g,"");var out="";for(var i=0;i<c.length;i+=4){var n=(ch.indexOf(c[i])<<18)|(ch.indexOf(c[i+1])<<12)|((c[i+2]==="="?0:ch.indexOf(c[i+2]))<<6)|(c[i+3]==="="?0:ch.indexOf(c[i+3]));out+=String.fromCharCode((n>>16)&255);if(c[i+2]!=="=")out+=String.fromCharCode((n>>8)&255);if(c[i+3]!=="=")out+=String.fromCharCode(n&255);}return __utf8dec(out);}
function __md5(input){function rl(v,s){return (v<<s)|(v>>>(32-s));}function au(x,y){var x4=x&0x40000000,y4=y&0x40000000,x8=x&0x80000000,y8=y&0x80000000,r=(x&0x3FFFFFFF)+(y&0x3FFFFFFF);if(x4&y4)return(r^0x80000000^x8^y8);if(x4|y4){if(r&0x40000000)return(r^0xC0000000^x8^y8);else return(r^0x40000000^x8^y8);}return(r^x8^y8);}function F(x,y,z){return(x&y)|((~x)&z);}function G(x,y,z){return(x&z)|(y&(~z));}function H(x,y,z){return(x^y^z);}function I(x,y,z){return(y^(x|(~z)));}function FF(a,b,c,d,x,s,ac){a=au(a,au(au(F(b,c,d),x),ac));return au(rl(a,s),b);}function GG(a,b,c,d,x,s,ac){a=au(a,au(au(G(b,c,d),x),ac));return au(rl(a,s),b);}function HH(a,b,c,d,x,s,ac){a=au(a,au(au(H(b,c,d),x),ac));return au(rl(a,s),b);}function II(a,b,c,d,x,s,ac){a=au(a,au(au(I(b,c,d),x),ac));return au(rl(a,s),b);}function cw(str){var lc,ml=str.length,t1=ml+8,t2=(t1-(t1%64))/64,nw=(t2+1)*16,wa=new Array(nw-1),bp=0,bc=0;while(bc<ml){lc=(bc-(bc%4))/4;bp=(bc%4)*8;wa[lc]=(wa[lc]|(str.charCodeAt(bc)<<bp));bc++;}lc=(bc-(bc%4))/4;bp=(bc%4)*8;wa[lc]=wa[lc]|(0x80<<bp);wa[nw-2]=ml<<3;wa[nw-1]=ml>>>29;return wa;}function wh(v){var wv="",t="",b,c;for(c=0;c<=3;c++){b=(v>>>(c*8))&255;t="0"+b.toString(16);wv+=t.substr(t.length-2,2);}return wv;}var x=[],k,AA,BB,CC,DD,a,b,c,d;var S11=7,S12=12,S13=17,S14=22,S21=5,S22=9,S23=14,S24=20,S31=4,S32=11,S33=16,S34=23,S41=6,S42=10,S43=15,S44=21;input=__utf8enc(input);x=cw(input);a=0x67452301;b=0xEFCDAB89;c=0x98BADCFE;d=0x10325476;for(k=0;k<x.length;k+=16){AA=a;BB=b;CC=c;DD=d;a=FF(a,b,c,d,x[k+0],S11,0xD76AA478);d=FF(d,a,b,c,x[k+1],S12,0xE8C7B756);c=FF(c,d,a,b,x[k+2],S13,0x242070DB);b=FF(b,c,d,a,x[k+3],S14,0xC1BDCEEE);a=FF(a,b,c,d,x[k+4],S11,0xF57C0FAF);d=FF(d,a,b,c,x[k+5],S12,0x4787C62A);c=FF(c,d,a,b,x[k+6],S13,0xA8304613);b=FF(b,c,d,a,x[k+7],S14,0xFD469501);a=FF(a,b,c,d,x[k+8],S11,0x698098D8);d=FF(d,a,b,c,x[k+9],S12,0x8B44F7AF);c=FF(c,d,a,b,x[k+10],S13,0xFFFF5BB1);b=FF(b,c,d,a,x[k+11],S14,0x895CD7BE);a=FF(a,b,c,d,x[k+12],S11,0x6B901122);d=FF(d,a,b,c,x[k+13],S12,0xFD987193);c=FF(c,d,a,b,x[k+14],S13,0xA679438E);b=FF(b,c,d,a,x[k+15],S14,0x49B40821);a=GG(a,b,c,d,x[k+1],S21,0xF61E2562);d=GG(d,a,b,c,x[k+6],S22,0xC040B340);c=GG(c,d,a,b,x[k+11],S23,0x265E5A51);b=GG(b,c,d,a,x[k+0],S24,0xE9B6C7AA);a=GG(a,b,c,d,x[k+5],S21,0xD62F105D);d=GG(d,a,b,c,x[k+10],S22,0x2441453);c=GG(c,d,a,b,x[k+15],S23,0xD8A1E681);b=GG(b,c,d,a,x[k+4],S24,0xE7D3FBC8);a=GG(a,b,c,d,x[k+9],S21,0x21E1CDE6);d=GG(d,a,b,c,x[k+14],S22,0xC33707D6);c=GG(c,d,a,b,x[k+3],S23,0xF4D50D87);b=GG(b,c,d,a,x[k+8],S24,0x455A14ED);a=GG(a,b,c,d,x[k+13],S21,0xA9E3E905);d=GG(d,a,b,c,x[k+2],S22,0xFCEFA3F8);c=GG(c,d,a,b,x[k+7],S23,0x676F02D9);b=GG(b,c,d,a,x[k+12],S24,0x8D2A4C8A);a=HH(a,b,c,d,x[k+5],S31,0xFFFA3942);d=HH(d,a,b,c,x[k+8],S32,0x8771F681);c=HH(c,d,a,b,x[k+11],S33,0x6D9D6122);b=HH(b,c,d,a,x[k+14],S34,0xFDE5380C);a=HH(a,b,c,d,x[k+1],S31,0xA4BEEA44);d=HH(d,a,b,c,x[k+4],S32,0x4BDECFA9);c=HH(c,d,a,b,x[k+7],S33,0xF6BB4B60);b=HH(b,c,d,a,x[k+10],S34,0xBEBFBC70);a=HH(a,b,c,d,x[k+13],S31,0x289B7EC6);d=HH(d,a,b,c,x[k+0],S32,0xEAA127FA);c=HH(c,d,a,b,x[k+3],S33,0xD4EF3085);b=HH(b,c,d,a,x[k+6],S34,0x4881D05);a=HH(a,b,c,d,x[k+9],S31,0xD9D4D039);d=HH(d,a,b,c,x[k+12],S32,0xE6DB99E5);c=HH(c,d,a,b,x[k+15],S33,0x1FA27CF8);b=HH(b,c,d,a,x[k+2],S34,0xC4AC5665);a=II(a,b,c,d,x[k+0],S41,0xF4292244);d=II(d,a,b,c,x[k+7],S42,0x432AFF97);c=II(c,d,a,b,x[k+14],S43,0xAB9423A7);b=II(b,c,d,a,x[k+5],S44,0xFC93A039);a=II(a,b,c,d,x[k+12],S41,0x655B59C3);d=II(d,a,b,c,x[k+3],S42,0x8F0CCC92);c=II(c,d,a,b,x[k+10],S43,0xFFEFF47D);b=II(b,c,d,a,x[k+1],S44,0x85845DD1);a=II(a,b,c,d,x[k+8],S41,0x6FA87E4F);d=II(d,a,b,c,x[k+15],S42,0xFE2CE6E0);c=II(c,d,a,b,x[k+6],S43,0xA3014314);b=II(b,c,d,a,x[k+13],S44,0x4E0811A1);a=II(a,b,c,d,x[k+4],S41,0xF7537E82);d=II(d,a,b,c,x[k+11],S42,0xBD3AF235);c=II(c,d,a,b,x[k+2],S43,0x2AD7D2BB);b=II(b,c,d,a,x[k+9],S44,0xEB86D391);a=au(a,AA);b=au(b,BB);c=au(c,CC);d=au(d,DD);}return (wh(a)+wh(b)+wh(c)+wh(d)).toLowerCase();}
var java={md5Encode:function(s){return __md5(String(s));},base64Encode:function(s){return __b64e(String(s));},base64Decode:function(s){return __b64d(String(s));},encodeURIComponent:function(s){return encodeURIComponent(String(s));},decodeURIComponent:function(s){return decodeURIComponent(String(s));},encodeURI:function(s){return encodeURI(String(s));},decodeURI:function(s){return decodeURI(String(s));},timeFormat:function(ts){var d=new Date(Number(ts));function p(n){return(n<10?"0":"")+n;}return d.getFullYear()+"-"+p(d.getMonth()+1)+"-"+p(d.getDate())+" "+p(d.getHours())+":"+p(d.getMinutes())+":"+p(d.getSeconds());},now:function(){return Date.now();},uuid:function(){function h(){return Math.floor((1+Math.random())*0x10000).toString(16).substring(1);}return h()+h()+"-"+h()+"-4"+h().substring(1)+"-"+(8+Math.floor(Math.random()*4)).toString(16)+h().substring(1)+"-"+h()+h()+h();}};
function regex_replace(input,pattern,replace){return String(input).replace(new RegExp(pattern,"g"),replace===undefined?"":replace);}
function strip_ws(input){return String(input).replace(/\s+/g," ").trim();}
// Minimal URL (QuickJS has no URL global): new URL(input, base) with
// href / origin / protocol / host / hostname / pathname / search / hash.
function __parseUrl(u){var m=String(u).match(/^([a-zA-Z][a-zA-Z0-9+.-]*):\/\/([^/?#]*)([^?#]*)(\?[^#]*)?(#.*)?$/);if(!m)return null;return{protocol:m[1]+":",host:m[2],hostname:m[2].split(":")[0],pathname:m[3]||"/",search:m[4]||"",hash:m[5]||""};}
function __normPath(p){var parts=p.split("/"),out=[];for(var i=0;i<parts.length;i++){var s=parts[i];if(s===".")continue;if(s===".."){if(out.length&&out[out.length-1]!=="")out.pop();}else out.push(s);}var r=out.join("/");if(p.charAt(0)==="/"&&r.charAt(0)!=="/")r="/"+r;return r||"/";}
function URL(input,base){var s=String(input);var u=__parseUrl(s);if(!u){if(!base)throw new Error("Invalid URL");var b=__parseUrl(String(base));if(!b)throw new Error("Invalid base URL");if(s.slice(0,2)==="//"){u=__parseUrl(b.protocol+s);}else if(s.charAt(0)==="/"){u={protocol:b.protocol,host:b.host,hostname:b.hostname,pathname:__normPath(s),search:"",hash:""};}else if(s.charAt(0)==="?"||s.charAt(0)==="#"){u={protocol:b.protocol,host:b.host,hostname:b.hostname,pathname:b.pathname,search:s.charAt(0)==="?"?s:b.search,hash:s.charAt(0)==="#"?s:""};}else{var dir=b.pathname.slice(0,b.pathname.lastIndexOf("/")+1);u={protocol:b.protocol,host:b.host,hostname:b.hostname,pathname:__normPath(dir+s),search:"",hash:""};var q=u.pathname.indexOf("?");if(q>=0){u.search=u.pathname.slice(q);u.pathname=u.pathname.slice(0,q);}var h=u.pathname.indexOf("#");if(h>=0){u.hash=u.pathname.slice(h);u.pathname=u.pathname.slice(0,h);}}if(!u)throw new Error("Invalid URL");}this.protocol=u.protocol;this.host=u.host;this.hostname=u.hostname;this.pathname=u.pathname;this.search=u.search;this.hash=u.hash;this.origin=u.protocol+"//"+u.host;this.href=u.protocol+"//"+u.host+u.pathname+u.search+u.hash;this.toString=function(){return this.href;};}
"##;

// ---------------------------------------------------------------------------
// Request rules: searchUrl / exploreUrl / loginUrl
// Legado format: `url` with `{key}`/`{page}` placeholders, `{{js}}`
// expressions, and an optional trailing `,{...}` JSON config
// ({"method":"POST","body":"q={{key}}","headers":{...},"charset":"gbk"}).
// Blocking ({{...}} runs QuickJS): call from spawn_blocking only, or from
// contexts that already run there (extract_*); Engine wraps the call.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RequestSpec {
    pub url: String,
    pub method: String,
    pub body: Option<String>,
    pub headers: Vec<(String, String)>,
    pub charset: Option<String>,
}

/// Render `{key}` / `{{key}}` / `{page}` / `{{page}}` placeholders, then any
/// remaining `{{expr}}` as JS (`key`, `page`, `result` bound to the keyword).
/// Double-brace forms are replaced first: `{{key}}` contains `{key}`.
fn render_template(template: &str, keyword: &str, page: i64, js_lib: &str) -> Result<String> {
    let encoded = urlencoding(keyword);
    let page_s = page.to_string();
    let mut out = template
        .replace("{{key}}", &encoded)
        .replace("{{keyword}}", &encoded)
        .replace("{{page}}", &page_s)
        .replace("{key}", &encoded)
        .replace("{keyword}", &encoded)
        .replace("{page}", &page_s);
    loop {
        let Some(start) = out.find("{{") else { break };
        let Some(end_rel) = out[start + 2..].find("}}") else { break };
        let end = start + 2 + end_rel;
        let expr = out[start + 2..end].trim().to_string();
        if expr.is_empty() {
            break;
        }
        let val = run_js_vars(&expr, keyword, &[("key", keyword), ("page", &page_s)], js_lib, JS_TIMEOUT)
            .map_err(|e| anyhow!("js in url template {{{{{expr}}}}}: {e}"))?;
        out.replace_range(start..end + 2, &val);
    }
    Ok(out)
}

pub fn parse_request_rule(template: &str, base: &Url, keyword: &str, page: i64, js_lib: &str) -> Result<RequestSpec> {
    let template = template.trim();
    if template.is_empty() {
        return Err(anyhow!("empty request rule"));
    }
    // Split trailing `,{...}` config JSON: the first `,{` whose remainder
    // parses as a JSON object wins (a URL may itself contain commas). If
    // the template contains `,{` but no remainder parses, the config is
    // malformed — fail loudly instead of sending a garbage URL.
    let mut url_part = template;
    let mut config = Value::Null;
    let mut search_from = 0;
    let mut saw_comma_brace = false;
    while let Some(idx) = template[search_from..].find(",{") {
        saw_comma_brace = true;
        let cut = search_from + idx;
        let rest = &template[cut + 1..];
        if let Ok(v) = serde_json::from_str::<Value>(rest) {
            if v.is_object() {
                url_part = template[..cut].trim();
                config = v;
                break;
            }
        }
        search_from = cut + 2;
    }
    if saw_comma_brace && config.is_null() {
        return Err(anyhow!("malformed request config JSON after ',{{' in {template:?}"));
    }
    let url = render_template(url_part, keyword, page, js_lib)?;
    // Legado sources usually give a relative URL; resolve against the
    // source's bookSourceUrl. An absolute URL wins as-is.
    let url = match Url::parse(&url) {
        Ok(u) => u.to_string(),
        Err(_) => base
            .join(&url)
            .map(|u| u.to_string())
            .map_err(|e| anyhow!("bad url {url:?}: {e}"))?,
    };
    let method = config
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .trim()
        .to_uppercase();
    if method != "GET" && method != "POST" {
        return Err(anyhow!("unsupported method {method:?} (only GET/POST)"));
    }
    let body = match config.get("body") {
        Some(Value::String(b)) => Some(render_template(b, keyword, page, js_lib)?),
        Some(other) => Some(render_template(&other.to_string(), keyword, page, js_lib)?),
        None => None,
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(Value::Object(map)) = config.get("headers") {
        for (k, v) in map {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            headers.push((k.clone(), render_template(&val, keyword, page, js_lib)?));
        }
    }
    let charset = config.get("charset").and_then(|v| v.as_str()).map(|s| s.to_string());
    Ok(RequestSpec { url, method, body, headers, charset })
}

/// Source-level `header` field: JSON object string -> header pairs.
fn parse_header_json(raw: &str) -> Result<Vec<(String, String)>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(vec![]);
    }
    let v: Value = serde_json::from_str(raw).map_err(|e| anyhow!("header is not a JSON object: {e}"))?;
    let map = v.as_object().ok_or_else(|| anyhow!("header is not a JSON object"))?;
    Ok(map
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                match v {
                    Value::String(s) => s.clone(),
                    o => o.to_string(),
                },
            )
        })
        .collect())
}

/// Decode response bytes: explicit `charset` hint first, then the
/// `charset=` in Content-Type, then strict UTF-8, then chardetng sniffing
/// (covers gbk sites that send no charset at all).
fn decode_body(bytes: &[u8], charset_hint: Option<&str>, content_type: Option<&str>) -> String {
    let mut label: Option<String> = charset_hint.map(|s| s.to_string());
    if label.is_none() {
        if let Some(ct) = content_type {
            for part in ct.split(';') {
                if let Some(v) = part.trim().strip_prefix("charset=") {
                    label = Some(v.trim_matches('"').to_string());
                    break;
                }
            }
        }
    }
    if let Some(l) = label {
        if let Some(enc) = encoding_rs::Encoding::for_label(l.as_bytes()) {
            let (cow, _, _) = enc.decode(bytes);
            return cow.into_owned();
        }
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let mut det = chardetng::EncodingDetector::new();
    det.feed(bytes, true);
    let enc = det.guess(None, true);
    let (cow, _, _) = enc.decode(bytes);
    cow.into_owned()
}

/// Strip a `js:` prefix or `<js>...</js>` wrapper from a raw JS rule value.
pub(crate) fn strip_js_wrapper(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("js:").unwrap_or(s).trim();
    if let Some(inner) = s.strip_prefix("<js>").and_then(|x| x.strip_suffix("</js>")) {
        return inner.trim();
    }
    s
}

/// Apply a `pattern##replacement` regex replace to raw text
/// (ruleContent sourceRegex / replaceRegex). Bare pattern = delete matches.
fn apply_regex_replace(text: &str, spec: &str) -> Result<String> {
    let mut parts = spec.splitn(2, "##");
    let pat = parts.next().unwrap_or("");
    let repl = parts.next().unwrap_or("");
    let re = get_regex(pat)?;
    Ok(re.replace_all(text, repl).into_owned())
}

// ---------------------------------------------------------------------------
// Rule evaluation against a parsed Html document
// ---------------------------------------------------------------------------

enum Node<'a> {
    Root(&'a Html),
    El(scraper::ElementRef<'a>),
}

impl<'a> Node<'a> {
    fn select(&self, sel: &Selector) -> Vec<scraper::ElementRef<'a>> {
        match self {
            Node::Root(h) => h.select(sel).collect(),
            Node::El(e) => e.select(sel).collect(),
        }
    }

    fn text(&self) -> String {
        match self {
            Node::Root(h) => h.root_element().text().collect::<Vec<_>>().join(""),
            Node::El(e) => e.text().collect::<Vec<_>>().join(""),
        }
    }
}

struct RuleStr {
    chain: Vec<String>,
    attr: Option<String>,
    js: Option<String>,
}

fn parse_rule_str(s: &str) -> RuleStr {
    let s = s.trim();
    if let Some(js) = s.strip_prefix("js:") {
        return RuleStr { chain: vec![], attr: None, js: Some(js.to_string()) };
    }
    let parts: Vec<&str> = s.split("##").collect();
    let chain_raw = parts[0];
    let mut chain: Vec<String> = chain_raw.split("&&").map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    let mut attr = None;
    if let Some(last) = chain.last_mut() {
        if let Some(idx) = last.find('@') {
            let (sel, a) = (last[..idx].trim().to_string(), last[idx + 1..].trim().to_string());
            if sel.is_empty() {
                // "@text" / "@href" with no selector: operate on the element itself.
                chain.pop();
            } else {
                *last = sel;
            }
            attr = Some(a);
        }
    }
    RuleStr {
        chain,
        attr,
        js: None,
    }
}

/// Evaluate a string rule against a scope node; returns one value per match.
///
/// Rule modes by prefix (Legado): `js:` / `<js>...</js>` (QuickJS on the
/// node text), `@xpath:` / `/...` / `./...` (XPath subset, see
/// `crate::xpath`), `@json:` / `$...` (JSON — only when the response is
/// JSON; on an HTML page this errors instead of returning empty), `@regex:`
/// / `:` (regex search over the node text), `@css:` or bare (CSS selector
/// chain with `&&`, `@attr`, `##regex##repl`). The `##regex##replacement`
/// suffix applies to every mode; a `@js:...` suffix post-processes each
/// extracted value (`input` = the value, `base_url` bound). JS rules see
/// `result` / `input` / `src` (aliases), `base_url` / `baseUrl` / `url`,
/// and the `java.*` pure helpers.
fn eval_rule_str(node: &Node, rule: &str, js_lib: &str, js_out: &mut Vec<String>, base: &Url) -> Result<Vec<String>> {
    let rule = rule.trim();
    let (core, regex, replacement) = split_rule_parts(rule);
    // `js:` / `<js>...</js>` is the whole rule — never split `@js:` out
    // of it (the script may contain that literal). Otherwise `@js:`
    // is a postfix: head extracts, tail post-processes each value.
    let is_js_rule = core.starts_with("js:") || (core.starts_with("<js>") && core.ends_with("</js>"));
    let (core, post_js) = if is_js_rule {
        (core, None)
    } else {
        match core.find("@js:") {
            Some(i) => (&core[..i], Some(core[i + 4..].trim())),
            None => (core, None),
        }
    };
    let apply_re = |vals: Vec<String>| -> Result<Vec<String>> {
        if let Some(pat) = regex {
            let re = get_regex(pat)?;
            let repl = replacement.unwrap_or("");
            Ok(vals.into_iter().map(|v| re.replace_all(&v, repl).into_owned()).collect())
        } else {
            Ok(vals)
        }
    };
    let mut apply_post_js = |vals: Vec<String>| -> Result<Vec<String>> {
        match post_js {
            Some(js) => {
                let mut out = Vec::new();
                for v in vals {
                    let r = run_js_ext(js, &v, js_lib, base.as_str(), "", JS_TIMEOUT)?;
                    js_out.push(r.clone());
                    out.push(r);
                }
                Ok(out)
            }
            None => Ok(vals),
        }
    };
    // `js:` / `<js>...</js>` is the whole rule: no @js: postfix inside.
    if is_js_rule {
        let js = strip_js_wrapper(core);
        let input = node.text();
        let out = run_js_ext(js, &input, js_lib, base.as_str(), "", JS_TIMEOUT)?;
        js_out.push(out.clone());
        return apply_re(vec![out]);
    }
    let vals = if is_xpath_rule(core) {
        eval_xpath_strs(node, xpath_of(core))?
    } else if core.starts_with("@json:") || core.starts_with('$') {
        return Err(anyhow!("rule {core:?} needs a JSON response, but this page is HTML"));
    } else if let Some(rx) = core.strip_prefix("@regex:").or_else(|| core.strip_prefix(':')) {
        let re = get_regex(rx)?;
        let text = node.text();
        let mut vals = Vec::new();
        for cap in re.captures_iter(&text) {
            let m = cap.get(1).or_else(|| cap.get(0)).map(|m| m.as_str()).unwrap_or("");
            vals.push(m.to_string());
        }
        vals
    } else {
        // CSS selector chain (existing behavior).
        let core = core.strip_prefix("@css:").unwrap_or(core);
        let r = parse_rule_str(core);
        let mut nodes: Vec<Node> = vec![match node {
            Node::Root(h) => Node::Root(h),
            Node::El(e) => Node::El(*e),
        }];
        for seg in &r.chain {
            let sel = get_selector(seg)?;
            let mut next: Vec<Node> = Vec::new();
            for n in &nodes {
                for el in n.select(&sel) {
                    next.push(Node::El(el));
                }
            }
            nodes = next;
            if nodes.is_empty() {
                return Ok(vec![]);
            }
        }
        let mut out = Vec::new();
        for n in &nodes {
            let v = match n {
                Node::Root(_) => node.text(),
                Node::El(e) => match r.attr.as_deref() {
                    Some("text") | None => e.text().collect::<Vec<_>>().join("").trim().to_string(),
                    Some("html") => e.inner_html(),
                    Some(a) => e.value().attr(a).unwrap_or("").to_string(),
                },
            };
            out.push(v);
        }
        out
    };
    apply_post_js(apply_re(vals)?)
}

/// Split a rule string into (core, regex, replacement) on `##`.
fn split_rule_parts(s: &str) -> (&str, Option<&str>, Option<&str>) {
    let mut parts = s.split("##");
    (parts.next().unwrap_or(""), parts.next(), parts.next())
}

/// True when a rule string is written in XPath form (`@xpath:` or a
/// leading `/` / `./`).
fn is_xpath_rule(s: &str) -> bool {
    let core = split_rule_parts(s.trim()).0;
    core.starts_with("@xpath:") || core.starts_with('/') || core.starts_with("./")
}

fn xpath_of(s: &str) -> &str {
    let core = split_rule_parts(s.trim()).0;
    core.strip_prefix("@xpath:").unwrap_or(core)
}

/// Evaluate an XPath rule against a scope node, one string per match
/// (elements yield their text; `@attr`/`text()` steps yield strings).
fn eval_xpath_strs(node: &Node, expr: &str) -> Result<Vec<String>> {
    let scope = match node {
        Node::Root(h) => h.root_element(),
        Node::El(e) => *e,
    };
    let vals = crate::xpath::eval_xpath(scope, expr)?;
    Ok(vals
        .into_iter()
        .map(|v| match v {
            crate::xpath::XValue::El(e) => e.text().collect::<Vec<_>>().join("").trim().to_string(),
            crate::xpath::XValue::Str(s) => s,
        })
        .collect())
}

/// Resolve a list/content rule to scope nodes: CSS chain (default) or
/// XPath (`@xpath:` / `/...`). A `##regex##` suffix is ignored here —
/// it only applies to string-valued rules.
fn resolve_nodes<'a>(doc: &'a Html, rule: &str) -> Result<Vec<Node<'a>>> {
    let core = split_rule_parts(rule.trim()).0;
    if core.starts_with("@json:") || core.starts_with('$') {
        return Err(anyhow!("rule {core:?} needs a JSON response, but this page is HTML"));
    }
    if is_xpath_rule(rule) {
        let scope = doc.root_element();
        let vals = crate::xpath::eval_xpath(scope, xpath_of(rule))?;
        return Ok(vals
            .into_iter()
            .filter_map(|v| match v {
                crate::xpath::XValue::El(e) => Some(Node::El(e)),
                crate::xpath::XValue::Str(_) => None,
            })
            .collect());
    }
    let parsed = parse_rule_str(rule);
    let mut nodes: Vec<Node> = vec![Node::Root(doc)];
    for seg in &parsed.chain {
        let sel = get_selector(seg)?;
        let mut next = Vec::new();
        for n in &nodes {
            for el in n.select(&sel) {
                next.push(Node::El(el));
            }
        }
        nodes = next;
    }
    Ok(nodes)
}

fn eval_rule_first(node: &Node, rule: &Value, js_lib: &str, js_out: &mut Vec<String>, base: &Url) -> Result<String> {
    match rule {
        Value::String(s) => Ok(eval_rule_str(node, s, js_lib, js_out, base)?.into_iter().next().unwrap_or_default()),
        _ => Ok(String::new()),
    }
}

fn resolve_url(base: &Url, href: &str) -> String {
    let href = href.trim();
    if href.is_empty() {
        return String::new();
    }
    base.join(href).map(|u| u.to_string()).unwrap_or_else(|_| href.to_string())
}

// ---------------------------------------------------------------------------
// Extraction (blocking: Html parse + selectors + JS)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookHit {
    pub name: String,
    pub author: String,
    #[serde(rename = "bookUrl")]
    pub book_url: String,
    #[serde(rename = "coverUrl")]
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cover_url: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub intro: String,
    #[serde(default, rename = "kind")]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, rename = "lastChapter")]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_chapter: String,
    #[serde(default, rename = "updateTime")]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub update_time: String,
    #[serde(default, rename = "wordCount")]
    #[serde(skip_serializing_if = "String::is_empty")]
    pub word_count: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChapterHit {
    pub name: String,
    pub url: String,
    #[serde(default, rename = "isVolume")]
    #[serde(skip_serializing_if = "is_false")]
    pub is_volume: bool,
    #[serde(default, rename = "isVip")]
    #[serde(skip_serializing_if = "is_false")]
    pub is_vip: bool,
    #[serde(default, rename = "isPay")]
    #[serde(skip_serializing_if = "is_false")]
    pub is_pay: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paragraph {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img: Option<String>,
}

fn sub_rule<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    obj.as_object().and_then(|m| m.get(key))
}

/// Extract search results. JSON responses (search APIs) take the JSON
/// branch; HTML pages take the selector/XPath branch.
pub fn extract_search(html: &str, rule: &Value, base: &Url, js_lib: &str) -> Result<(Vec<BookHit>, Vec<String>)> {
    if looks_like_json(html) {
        let json: Value = serde_json::from_str(html.trim())
            .map_err(|e| anyhow!("response looks like JSON but failed to parse: {e}"))?;
        return extract_search_json(&json, rule, base, js_lib);
    }
    let doc = Html::parse_document(html);
    let root = Node::Root(&doc);
    let mut js_out = Vec::new();
    let list_rule = sub_rule(rule, "bookList").cloned().unwrap_or_else(|| rule.clone());
    let mut books = Vec::new();
    match &list_rule {
        Value::String(s) => {
            let (list_str, reverse) = list_order(s);
            if is_js_rule_str(list_str) {
                let items = json_items_from_js_list(html, list_str, base, js_lib, &mut js_out)?;
                for item in &items {
                    if let Some(b) = book_hit_from_json(item, rule, base, js_lib, &mut js_out)? {
                        books.push(b);
                    }
                }
            } else {
                let nodes = resolve_nodes(&doc, list_str)?;
                for n in nodes {
                    if let Some(b) = book_hit_from_html(&n, rule, base, js_lib, &mut js_out)? {
                        books.push(b);
                    }
                }
            }
            if reverse {
                books.reverse();
            }
            dedupe_books(&mut books);
        }
        _ => {
            // No list rule: treat the whole page as one result.
            let name = eval_rule_first(&root, &list_rule, js_lib, &mut js_out, base)?;
            if !name.trim().is_empty() {
                books.push(BookHit {
                    name: name.trim().to_string(),
                    author: String::new(),
                    book_url: String::new(),
                    cover_url: String::new(),
                    intro: String::new(),
                    kind: String::new(),
                    last_chapter: String::new(),
                    update_time: String::new(),
                    word_count: String::new(),
                });
            }
        }
    }
    Ok((books, js_out))
}

/// Extract one book from a detail page with ruleBookInfo (search fallback:
/// when a search returns no books and the page is actually a book detail
/// page). book_url defaults to the page URL.
pub fn extract_book_info(html: &str, rule: &Value, base: &Url, js_lib: &str) -> Result<Option<BookHit>> {
    if looks_like_json(html) {
        let json: Value = serde_json::from_str(html.trim())
            .map_err(|e| anyhow!("response looks like JSON but failed to parse: {e}"))?;
        let mut js_out = Vec::new();
        let mut b = book_hit_from_json(&json, rule, base, js_lib, &mut js_out)?;
        if let Some(b) = &mut b {
            if b.book_url.is_empty() {
                b.book_url = base.to_string();
            }
        }
        return Ok(b);
    }
    // HTML detail pages need an explicit name rule; without one the whole
    // page text would be mistaken for the title.
    if sub_rule(rule, "name").is_none() {
        return Ok(None);
    }
    let doc = Html::parse_document(html);
    let root = Node::Root(&doc);
    let mut js_out = Vec::new();
    let mut b = book_hit_from_html(&root, rule, base, js_lib, &mut js_out)?;
    if let Some(b) = &mut b {
        if b.book_url.is_empty() {
            b.book_url = base.to_string();
        }
    }
    Ok(b)
}

/// A list rule may carry a `-` (reverse results) or `+` (keep order)
/// prefix; strip it and report whether to reverse.
fn list_order(rule: &str) -> (&str, bool) {
    let r = rule.trim();
    if let Some(rest) = r.strip_prefix('-') {
        (rest, true)
    } else if let Some(rest) = r.strip_prefix('+') {
        (rest, false)
    } else {
        (r, false)
    }
}

fn is_js_rule_str(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("js:") || (t.starts_with("<js>") && t.ends_with("</js>"))
}

/// Truthiness for isVolume / isVip / isPay rules: any non-empty value
/// other than "false" / "0" / "no" counts as true.
fn truthy(s: &str) -> bool {
    let t = s.trim().to_ascii_lowercase();
    !t.is_empty() && t != "false" && t != "0" && t != "no"
}

fn opt_html_field(node: &Node, rule: &Value, key: &str, js_lib: &str, js_out: &mut Vec<String>, base: &Url) -> Result<String> {
    match sub_rule(rule, key) {
        Some(r) => eval_rule_first(node, r, js_lib, js_out, base),
        None => Ok(String::new()),
    }
}

fn opt_json_field(item: &Value, rule: &Value, key: &str, fallbacks: &[&str], js_lib: &str, js_out: &mut Vec<String>, base: &Url) -> Result<String> {
    match sub_rule(rule, key) {
        Some(r) => eval_json_rule_first(item, r, js_lib, js_out, base),
        None => Ok(json_field(item, fallbacks)),
    }
}

/// One book from an HTML element (None when the name is empty).
fn book_hit_from_html(node: &Node, rule: &Value, base: &Url, js_lib: &str, js_out: &mut Vec<String>) -> Result<Option<BookHit>> {
    let name = match sub_rule(rule, "name") {
        Some(r) => eval_rule_first(node, r, js_lib, js_out, base)?,
        None => node.text(),
    };
    if name.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(BookHit {
        name: name.trim().to_string(),
        author: opt_html_field(node, rule, "author", js_lib, js_out, base)?.trim().to_string(),
        book_url: resolve_url(base, &opt_html_field(node, rule, "bookUrl", js_lib, js_out, base)?),
        cover_url: resolve_url(base, &opt_html_field(node, rule, "coverUrl", js_lib, js_out, base)?),
        intro: opt_html_field(node, rule, "intro", js_lib, js_out, base)?.trim().to_string(),
        kind: opt_html_field(node, rule, "kind", js_lib, js_out, base)?.trim().to_string(),
        last_chapter: opt_html_field(node, rule, "lastChapter", js_lib, js_out, base)?.trim().to_string(),
        update_time: opt_html_field(node, rule, "updateTime", js_lib, js_out, base)?.trim().to_string(),
        word_count: opt_html_field(node, rule, "wordCount", js_lib, js_out, base)?.trim().to_string(),
    }))
}

/// One book from a JSON item (None when the name is empty).
fn book_hit_from_json(item: &Value, rule: &Value, base: &Url, js_lib: &str, js_out: &mut Vec<String>) -> Result<Option<BookHit>> {
    let name = match sub_rule(rule, "name") {
        Some(r) => eval_json_rule_first(item, r, js_lib, js_out, base)?,
        None => json_field(item, &["name", "title"]),
    };
    if name.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(BookHit {
        name: name.trim().to_string(),
        author: opt_json_field(item, rule, "author", &["author"], js_lib, js_out, base)?.trim().to_string(),
        book_url: resolve_url(base, &opt_json_field(item, rule, "bookUrl", &["bookUrl", "url"], js_lib, js_out, base)?),
        cover_url: resolve_url(base, &opt_json_field(item, rule, "coverUrl", &["coverUrl", "cover"], js_lib, js_out, base)?),
        intro: opt_json_field(item, rule, "intro", &["intro", "description"], js_lib, js_out, base)?.trim().to_string(),
        kind: opt_json_field(item, rule, "kind", &["kind"], js_lib, js_out, base)?.trim().to_string(),
        last_chapter: opt_json_field(item, rule, "lastChapter", &["lastChapter", "latestChapter"], js_lib, js_out, base)?.trim().to_string(),
        update_time: opt_json_field(item, rule, "updateTime", &["updateTime", "updated"], js_lib, js_out, base)?.trim().to_string(),
        word_count: opt_json_field(item, rule, "wordCount", &["wordCount", "words"], js_lib, js_out, base)?.trim().to_string(),
    }))
}

/// A `js:` / `<js>...</js>` list rule: run it on the whole page (HTML
/// string or JSON text) and parse the output as a JSON array of items
/// (a single object becomes one item; unparsable output is empty).
fn json_items_from_js_list(input: &str, list_rule: &str, base: &Url, js_lib: &str, js_out: &mut Vec<String>) -> Result<Vec<Value>> {
    let js = strip_js_wrapper(list_rule.trim());
    let out = run_js_ext(js, input, js_lib, base.as_str(), "", JS_TIMEOUT)?;
    js_out.push(out.clone());
    match serde_json::from_str::<Value>(&out) {
        Ok(Value::Array(arr)) => Ok(arr),
        Ok(v) => Ok(vec![v]),
        Err(_) => Ok(vec![]),
    }
}

fn dedupe_books(books: &mut Vec<BookHit>) {
    let mut seen = std::collections::HashSet::new();
    books.retain(|b| b.book_url.is_empty() || seen.insert(b.book_url.clone()));
}

fn dedupe_chapters(chapters: &mut Vec<ChapterHit>) {
    let mut seen = std::collections::HashSet::new();
    chapters.retain(|c| seen.insert(c.url.clone()));
}

/// Test-only wrapper: production calls `extract_toc_page` directly.
#[cfg(test)]
pub fn extract_toc(html: &str, rule: &Value, base: &Url, js_lib: &str) -> Result<(Vec<ChapterHit>, Vec<String>)> {
    let (chapters, _next, js_out) = extract_toc_page(html, rule, base, js_lib)?;
    Ok((chapters, js_out))
}

/// Extract one toc page: chapters + the next-page URL (ruleToc.nextTocUrl,
/// resolved against the page URL; empty when there is no next page).
/// ruleToc.preUpdateJs runs on the raw response before any parsing;
/// ruleToc.formatJs rewrites each title afterwards (bindings `index`,
/// `title`, `chapter`).
pub fn extract_toc_page(html: &str, rule: &Value, base: &Url, js_lib: &str) -> Result<(Vec<ChapterHit>, String, Vec<String>)> {
    let mut html_owned = html.to_string();
    if let Some(Value::String(s)) = sub_rule(rule, "preUpdateJs") {
        if !s.trim().is_empty() {
            html_owned = run_js_ext(strip_js_wrapper(s), &html_owned, js_lib, base.as_str(), "", JS_TIMEOUT)?;
        }
    }
    if looks_like_json(&html_owned) {
        let json: Value = serde_json::from_str(html_owned.trim())
            .map_err(|e| anyhow!("response looks like JSON but failed to parse: {e}"))?;
        return extract_toc_page_json(&json, rule, base, js_lib);
    }
    let doc = Html::parse_document(&html_owned);
    let mut js_out = Vec::new();
    let list_rule = sub_rule(rule, "chapterList").cloned().unwrap_or_else(|| rule.clone());
    let mut chapters = Vec::new();
    if let Value::String(s) = &list_rule {
        let (list_str, reverse) = list_order(s);
        if is_js_rule_str(list_str) {
            let items = json_items_from_js_list(&html_owned, list_str, base, js_lib, &mut js_out)?;
            for (i, item) in items.iter().enumerate() {
                if let Some(c) = chapter_hit_from_json(item, rule, base, base.as_str(), i + 1, js_lib, &mut js_out)? {
                    chapters.push(c);
                }
            }
        } else {
            let nodes = resolve_nodes(&doc, list_str)?;
            for (i, n) in nodes.iter().enumerate() {
                if let Some(c) = chapter_hit_from_html(n, rule, base, base.as_str(), i + 1, js_lib, &mut js_out)? {
                    chapters.push(c);
                }
            }
        }
        if reverse {
            chapters.reverse();
        }
        dedupe_chapters(&mut chapters);
    }
    let next = match sub_rule(rule, "nextTocUrl") {
        Some(r) => {
            let root = Node::Root(&doc);
            resolve_url(base, &eval_rule_first(&root, r, js_lib, &mut js_out, base)?)
        }
        None => String::new(),
    };
    Ok((chapters, next, js_out))
}

/// Run ruleToc.formatJs on one chapter title (index is 1-based within
/// the page). Bindings: `index`, `title`, `chapter` ({name, url} object),
/// plus the usual `input` / `result` (= title). An empty result keeps
/// the original title.
fn apply_format_js(name: &str, url: &str, index: usize, rule: &Value, js_lib: &str, base: &Url, js_out: &mut Vec<String>) -> Result<String> {
    let Some(Value::String(script)) = sub_rule(rule, "formatJs") else {
        return Ok(name.to_string());
    };
    if script.trim().is_empty() {
        return Ok(name.to_string());
    }
    let chapter_json = serde_json::json!({"name": name, "url": url}).to_string();
    let prelude = format!(
        "let index = {}; let title = {}; let chapter = {};",
        index,
        serde_json::to_string(name).unwrap_or_default(),
        chapter_json,
    );
    let out = run_js_ext(strip_js_wrapper(script), name, js_lib, base.as_str(), &prelude, JS_TIMEOUT)?;
    js_out.push(out.clone());
    let out = out.trim().to_string();
    Ok(if out.is_empty() { name.to_string() } else { out })
}

/// Shared tail of chapter extraction: empty URL falls back to a
/// `volume://<index>` placeholder for volume nodes, else to the toc page
/// URL; then formatJs rewrites the title.
fn finish_chapter_hit(name: &str, url: &str, is_volume: bool, is_vip: bool, is_pay: bool, index: usize, rule: &Value, base: &Url, page_url: &str, js_lib: &str, js_out: &mut Vec<String>) -> Result<ChapterHit> {
    let mut url = url.trim().to_string();
    if url.is_empty() {
        url = if is_volume { format!("volume://{index}") } else { page_url.to_string() };
    }
    let url = resolve_url(base, &url);
    let name = apply_format_js(name, &url, index, rule, js_lib, base, js_out)?;
    Ok(ChapterHit { name, url, is_volume, is_vip, is_pay })
}

fn chapter_hit_from_html(node: &Node, rule: &Value, base: &Url, page_url: &str, index: usize, js_lib: &str, js_out: &mut Vec<String>) -> Result<Option<ChapterHit>> {
    let name = match sub_rule(rule, "chapterName") {
        Some(r) => eval_rule_first(node, r, js_lib, js_out, base)?,
        None => node.text(),
    };
    if name.trim().is_empty() {
        return Ok(None);
    }
    let url = opt_html_field(node, rule, "chapterUrl", js_lib, js_out, base)?;
    let is_volume = truthy(&opt_html_field(node, rule, "isVolume", js_lib, js_out, base)?);
    let is_vip = truthy(&opt_html_field(node, rule, "isVip", js_lib, js_out, base)?);
    let is_pay = truthy(&opt_html_field(node, rule, "isPay", js_lib, js_out, base)?);
    Ok(Some(finish_chapter_hit(name.trim(), &url, is_volume, is_vip, is_pay, index, rule, base, page_url, js_lib, js_out)?))
}

fn chapter_hit_from_json(item: &Value, rule: &Value, base: &Url, page_url: &str, index: usize, js_lib: &str, js_out: &mut Vec<String>) -> Result<Option<ChapterHit>> {
    let name = match sub_rule(rule, "chapterName") {
        Some(r) => eval_json_rule_first(item, r, js_lib, js_out, base)?,
        None => json_field(item, &["name", "chapterName", "title"]),
    };
    if name.trim().is_empty() {
        return Ok(None);
    }
    let url = opt_json_field(item, rule, "chapterUrl", &["url", "chapterUrl"], js_lib, js_out, base)?;
    let is_volume = truthy(&opt_json_field(item, rule, "isVolume", &["isVolume"], js_lib, js_out, base)?);
    let is_vip = truthy(&opt_json_field(item, rule, "isVip", &["isVip", "vip"], js_lib, js_out, base)?);
    let is_pay = truthy(&opt_json_field(item, rule, "isPay", &["isPay", "pay"], js_lib, js_out, base)?);
    Ok(Some(finish_chapter_hit(name.trim(), &url, is_volume, is_vip, is_pay, index, rule, base, page_url, js_lib, js_out)?))
}

fn paragraphs_in_block(el: scraper::ElementRef, base: &Url) -> Vec<Paragraph> {
    let mut out: Vec<Paragraph> = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Paragraph>| {
        let t = buf.trim().to_string();
        if !t.is_empty() {
            out.push(Paragraph { text: Some(t), img: None });
        }
        buf.clear();
    };
    // descendants() yields the element itself first; skip it.
    let mut first = true;
    for node in el.descendants() {
        if first {
            first = false;
            continue;
        }
        match node.value() {
            scraper::node::Node::Text(t) => buf.push_str(t),
            scraper::node::Node::Element(e) => match e.name() {
                "img" => {
                    flush(&mut buf, &mut out);
                    if let Some(src) = e.attr("src").or_else(|| e.attr("data-src")) {
                        let abs = resolve_url(base, src);
                        if !abs.is_empty() {
                            out.push(Paragraph { text: None, img: Some(abs) });
                        }
                    }
                }
                // Block boundaries split paragraphs.
                "br" | "p" | "div" | "h1" | "h2" | "h3" | "h4" | "li" | "blockquote" => {
                    flush(&mut buf, &mut out);
                }
                _ => {}
            },
            _ => {}
        }
    }
    flush(&mut buf, &mut out);
    out
}

/// Test-only wrapper: production calls `extract_content_page` directly.
#[cfg(test)]
pub fn extract_content(html: &str, rule: &Value, base: &Url, js_lib: &str) -> Result<(Vec<Paragraph>, Vec<String>)> {
    let (paras, _next, js_out) = extract_content_page(html, rule, base, js_lib)?;
    Ok((paras, js_out))
}

/// Extract one content page, following Legado's ruleContent order:
/// sourceRegex (raw HTML) -> webJs (raw HTML in, transformed HTML out;
/// the QuickJS sandbox has no DOM — `result` is the HTML string) ->
/// content -> replaceRegex (per-paragraph text).
/// Returns (paragraphs, next-page URL, js outputs).
pub fn extract_content_page(html: &str, rule: &Value, base: &Url, js_lib: &str) -> Result<(Vec<Paragraph>, String, Vec<String>)> {
    let mut js_out = Vec::new();
    let mut html_owned = html.to_string();
    if let Some(Value::String(s)) = sub_rule(rule, "sourceRegex") {
        if !s.trim().is_empty() {
            html_owned = apply_regex_replace(&html_owned, s)?;
        }
    }
    if let Some(Value::String(s)) = sub_rule(rule, "webJs") {
        if !s.trim().is_empty() {
            let script = strip_js_wrapper(s);
            let out = run_js(script, &html_owned, js_lib, JS_TIMEOUT)?;
            // The transformed HTML can be huge; keep a truncated copy in the
            // test panel output instead of the whole page.
            js_out.push(if out.chars().count() > 2000 {
                format!("{}…[truncated, {} chars]", out.chars().take(2000).collect::<String>(), out.len())
            } else {
                out.clone()
            });
            html_owned = out;
        }
    }
    let doc = Html::parse_document(&html_owned);
    // JSON chapter APIs: {"content": "..."} or {"data": {"content": ...}}.
    if looks_like_json(&html_owned) {
        let json: Value = serde_json::from_str(html_owned.trim())
            .map_err(|e| anyhow!("response looks like JSON but failed to parse: {e}"))?;
        let (paras, next, mut js_out2) = extract_content_page_json(&json, rule, base, js_lib)?;
        js_out.append(&mut js_out2);
        return Ok((paras, next, js_out));
    }
    let mut js_out2 = Vec::new();
    let content_rule = sub_rule(rule, "content").cloned().unwrap_or_else(|| rule.clone());
    let mut paras: Vec<Paragraph> = Vec::new();
    if let Value::String(s) = &content_rule {
        let parsed = parse_rule_str(&s.clone());
        if parsed.js.is_some() {
            let root = Node::Root(&doc);
            let vals = eval_rule_str(&root, s, js_lib, &mut js_out2, base)?;
            for v in vals {
                for line in v.lines() {
                    let t = line.trim();
                    if !t.is_empty() {
                        paras.push(Paragraph { text: Some(t.to_string()), img: None });
                    }
                }
            }
        } else {
            // CSS chain or XPath: resolve content elements, then walk each
            // in document order (text runs -> paragraphs, <img> -> {img}).
            let (core, regex, replacement) = split_rule_parts(s);
            let nodes = resolve_nodes(&doc, core)?;
            for n in nodes {
                if let Node::El(el) = n {
                    paras.extend(paragraphs_in_block(el, base));
                }
            }
            // Optional regex post-processing on joined text.
            if let Some(pat) = regex {
                let re = get_regex(pat)?;
                let repl = replacement.unwrap_or("");
                for p in paras.iter_mut() {
                    if let Some(t) = &p.text {
                        p.text = Some(re.replace_all(t, repl).into_owned());
                    }
                }
            }
        }
    }
    // replaceRegex: final pass over paragraph text.
    apply_replace_regex(&mut paras, rule)?;
    let next = match sub_rule(rule, "nextContentUrl") {
        Some(r) => {
            let root = Node::Root(&doc);
            resolve_url(base, &eval_rule_first(&root, r, js_lib, &mut js_out2, base)?)
        }
        None => String::new(),
    };
    js_out.append(&mut js_out2);
    Ok((paras, next, js_out))
}

/// Apply ruleContent.replaceRegex (pattern##replacement) to paragraph text.
fn apply_replace_regex(paras: &mut [Paragraph], rule: &Value) -> Result<()> {
    if let Some(Value::String(s)) = sub_rule(rule, "replaceRegex") {
        if !s.trim().is_empty() {
            let mut parts = s.splitn(2, "##");
            let pat = parts.next().unwrap_or("");
            let repl = parts.next().unwrap_or("");
            let re = get_regex(pat)?;
            for p in paras.iter_mut() {
                if let Some(t) = &p.text {
                    p.text = Some(re.replace_all(t, repl).into_owned());
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON responses: @json: / $.path rules over serde_json::Value
// ---------------------------------------------------------------------------

fn looks_like_json(body: &str) -> bool {
    matches!(body.trim_start().chars().next(), Some('{') | Some('['))
}

fn is_bare_field(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Evaluate a string rule against a JSON value; one string per match.
/// Modes: `js:` / `<js>...</js>` (QuickJS on the item's JSON text),
/// `@json:$.path` / `$.path` (JSONPath subset), a bare field name (`title`
/// -> item["title"]), or a `{{...}}` template (see
/// `render_item_template`). The `##regex##replacement` suffix applies to
/// every mode, and an `@js:...` suffix post-processes each value.
fn eval_json_rule(item: &Value, rule: &str, js_lib: &str, js_out: &mut Vec<String>, base: &Url) -> Result<Vec<String>> {
    let rule = rule.trim();
    let (core, regex, replacement) = split_rule_parts(rule);
    let is_js_rule = core.starts_with("js:") || (core.starts_with("<js>") && core.ends_with("</js>"));
    let (core, post_js) = if is_js_rule {
        (core, None)
    } else {
        match core.find("@js:") {
            Some(i) => (&core[..i], Some(core[i + 4..].trim())),
            None => (core, None),
        }
    };
    let mut vals: Vec<String> = if is_js_rule {
        let js = strip_js_wrapper(core);
        let input = crate::jsonpath::value_to_string(item);
        let out = run_js_ext(js, &input, js_lib, base.as_str(), "", JS_TIMEOUT)?;
        js_out.push(out.clone());
        vec![out]
    } else if let Some(jp) = core.strip_prefix("@json:") {
        crate::jsonpath::eval_json_path(item, jp)?
            .into_iter()
            .map(crate::jsonpath::value_to_string)
            .collect()
    } else if core.starts_with('$') {
        crate::jsonpath::eval_json_path(item, core)?
            .into_iter()
            .map(crate::jsonpath::value_to_string)
            .collect()
    } else if core.contains("{{") {
        vec![render_item_template(core, item, js_lib, base)?]
    } else if is_bare_field(core) {
        match item.get(core) {
            Some(v) => vec![crate::jsonpath::value_to_string(v)],
            None => vec![],
        }
    } else {
        return Err(anyhow!("unsupported JSON rule {core:?} (use @json:$.path, $.path, a field name, a {{{{...}}}} template, or js:)"));
    };
    if let Some(pat) = regex {
        let re = get_regex(pat)?;
        let repl = replacement.unwrap_or("");
        vals = vals.into_iter().map(|v| re.replace_all(&v, repl).into_owned()).collect();
    }
    if let Some(js) = post_js {
        let mut out = Vec::new();
        for v in vals {
            let r = run_js_ext(js, &v, js_lib, base.as_str(), "", JS_TIMEOUT)?;
            js_out.push(r.clone());
            out.push(r);
        }
        vals = out;
    }
    Ok(vals)
}

/// Render a `{{...}}` template against a JSON item (e.g. a bookUrl of
/// `https://example.com/book/{{id}}`). `{{field}}` is a direct lookup of a
/// top-level field; anything else runs as JS with the item's top-level
/// fields bound as variables (valid identifiers only).
fn render_item_template(template: &str, item: &Value, js_lib: &str, base: &Url) -> Result<String> {
    let mut out = template.to_string();
    if let Some(obj) = item.as_object() {
        for (k, v) in obj {
            out = out.replace(&format!("{{{{{k}}}}}"), &crate::jsonpath::value_to_string(v));
        }
    }
    loop {
        let Some(start) = out.find("{{") else { break };
        let Some(end_rel) = out[start + 2..].find("}}") else { break };
        let end = start + 2 + end_rel;
        let expr = out[start + 2..end].trim().to_string();
        if expr.is_empty() {
            break;
        }
        let vars: Vec<(String, String)> = item
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(k, _)| {
                        let mut cs = k.chars();
                        matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
                            && cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
                    })
                    .map(|(k, v)| (k.clone(), crate::jsonpath::value_to_string(v)))
                    .collect()
            })
            .unwrap_or_default();
        let prelude: String = vars
            .iter()
            .map(|(k, v)| format!("let {} = {};", k, serde_json::to_string(v).unwrap_or_default()))
            .collect();
        let input = crate::jsonpath::value_to_string(item);
        let val = run_js_ext(&expr, &input, js_lib, base.as_str(), &prelude, JS_TIMEOUT)
            .map_err(|e| anyhow!("js in json template {{{{{expr}}}}}: {e}"))?;
        out.replace_range(start..end + 2, &val);
    }
    Ok(out)
}

fn eval_json_rule_first(item: &Value, rule: &Value, js_lib: &str, js_out: &mut Vec<String>, base: &Url) -> Result<String> {
    match rule {
        Value::String(s) => Ok(eval_json_rule(item, s, js_lib, js_out, base)?.into_iter().next().unwrap_or_default()),
        _ => Ok(String::new()),
    }
}

/// Resolve a list rule (bookList / chapterList) against a JSON document
/// into raw items, so sub-rules can run per item.
fn eval_json_list_items(json: &Value, list_rule: &str, js_lib: &str, js_out: &mut Vec<String>, base: &Url) -> Result<Vec<Value>> {
    let rule = list_rule.trim();
    let core = split_rule_parts(rule).0;
    if is_js_rule_str(core) {
        let js = strip_js_wrapper(core);
        let input = crate::jsonpath::value_to_string(json);
        let out = run_js_ext(js, &input, js_lib, base.as_str(), "", JS_TIMEOUT)?;
        js_out.push(out.clone());
        return match serde_json::from_str::<Value>(&out) {
            Ok(Value::Array(arr)) => Ok(arr),
            Ok(v) => Ok(vec![v]),
            Err(_) => Ok(vec![]),
        };
    }
    let path = core.strip_prefix("@json:").unwrap_or(core);
    if path.starts_with('$') {
        return Ok(crate::jsonpath::eval_json_path(json, path)?.into_iter().cloned().collect());
    }
    if is_bare_field(path) {
        return match json.get(path) {
            Some(Value::Array(arr)) => Ok(arr.clone()),
            Some(v) => Ok(vec![v.clone()]),
            None => Ok(vec![]),
        };
    }
    Err(anyhow!("unsupported JSON list rule {core:?} (use @json:$.path, $.path, a field name, or js:)"))
}

/// Fallback field lookup for JSON items when a sub-rule is absent.
fn json_field<'a>(item: &'a Value, names: &[&str]) -> String {
    for n in names {
        if let Some(v) = item.get(n) {
            let s = crate::jsonpath::value_to_string(v);
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}

fn extract_search_json(json: &Value, rule: &Value, base: &Url, js_lib: &str) -> Result<(Vec<BookHit>, Vec<String>)> {
    let mut js_out = Vec::new();
    let list_rule = sub_rule(rule, "bookList").cloned().unwrap_or_else(|| rule.clone());
    let mut books = Vec::new();
    match &list_rule {
        Value::String(s) => {
            let (list_str, reverse) = list_order(s);
            let items = eval_json_list_items(json, list_str, js_lib, &mut js_out, base)?;
            for item in &items {
                if let Some(b) = book_hit_from_json(item, rule, base, js_lib, &mut js_out)? {
                    books.push(b);
                }
            }
            if reverse {
                books.reverse();
            }
            dedupe_books(&mut books);
        }
        _ => match json {
            Value::Array(arr) => {
                for item in arr {
                    if let Some(b) = book_hit_from_json(item, rule, base, js_lib, &mut js_out)? {
                        books.push(b);
                    }
                }
            }
            _ => {
                if let Some(b) = book_hit_from_json(json, rule, base, js_lib, &mut js_out)? {
                    books.push(b);
                }
            }
        },
    }
    Ok((books, js_out))
}

fn extract_toc_page_json(json: &Value, rule: &Value, base: &Url, js_lib: &str) -> Result<(Vec<ChapterHit>, String, Vec<String>)> {
    let mut js_out = Vec::new();
    let list_rule = sub_rule(rule, "chapterList").cloned().unwrap_or_else(|| rule.clone());
    let mut chapters = Vec::new();
    match &list_rule {
        Value::String(s) => {
            let (list_str, reverse) = list_order(s);
            let items = eval_json_list_items(json, list_str, js_lib, &mut js_out, base)?;
            for (i, item) in items.iter().enumerate() {
                if let Some(c) = chapter_hit_from_json(item, rule, base, base.as_str(), i + 1, js_lib, &mut js_out)? {
                    chapters.push(c);
                }
            }
            if reverse {
                chapters.reverse();
            }
            dedupe_chapters(&mut chapters);
        }
        _ => match json {
            Value::Array(arr) => {
                for (i, item) in arr.iter().enumerate() {
                    if let Some(c) = chapter_hit_from_json(item, rule, base, base.as_str(), i + 1, js_lib, &mut js_out)? {
                        chapters.push(c);
                    }
                }
            }
            _ => {
                if let Some(c) = chapter_hit_from_json(json, rule, base, base.as_str(), 1, js_lib, &mut js_out)? {
                    chapters.push(c);
                }
            }
        },
    }
    let next = match sub_rule(rule, "nextTocUrl") {
        Some(r) => resolve_url(base, &eval_json_rule_first(json, r, js_lib, &mut js_out, base)?),
        None => String::new(),
    };
    Ok((chapters, next, js_out))
}

fn extract_content_page_json(json: &Value, rule: &Value, base: &Url, js_lib: &str) -> Result<(Vec<Paragraph>, String, Vec<String>)> {
    let mut js_out = Vec::new();
    let content_rule = sub_rule(rule, "content").cloned().unwrap_or_else(|| rule.clone());
    let mut paras: Vec<Paragraph> = Vec::new();
    if let Value::String(s) = &content_rule {
        let vals = eval_json_rule(json, s, js_lib, &mut js_out, base)?;
        for v in vals {
            for line in v.lines() {
                let t = line.trim();
                if !t.is_empty() {
                    paras.push(Paragraph { text: Some(t.to_string()), img: None });
                }
            }
        }
    }
    apply_replace_regex(&mut paras, rule)?;
    let next = match sub_rule(rule, "nextContentUrl") {
        Some(r) => resolve_url(base, &eval_json_rule_first(json, r, js_lib, &mut js_out, base)?),
        None => String::new(),
    };
    Ok((paras, next, js_out))
}

// ---------------------------------------------------------------------------
// HTTP pool: one reqwest Client per domain (connection-pool isolation)
// ---------------------------------------------------------------------------

pub struct HttpPool {
    clients: RwLock<HashMap<String, reqwest::Client>>,
}

impl HttpPool {
    pub fn new() -> Self {
        Self { clients: RwLock::new(HashMap::new()) }
    }

    /// One client per (domain, source): cookies live in the client's jar,
    /// so a login cookie never leaks across sources on the same domain.
    /// `enabled_cookie_jar=false` builds a client that stores no cookies.
    pub fn client_for_source(&self, url: &str, source: &SourceDef) -> Result<reqwest::Client> {
        let host = Url::parse(url)
            .map_err(|e| anyhow!("bad url {url}: {e}"))?
            .host_str()
            .unwrap_or("")
            .to_string();
        let key = format!("{host}|{}|{}", source.id, source.enabled_cookie_jar);
        if let Some(c) = self.clients.read().unwrap().get(&key) {
            return Ok(c.clone());
        }
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("shuku/0.1 (compatible; Reading3)")
            .pool_max_idle_per_host(8)
            .pool_idle_timeout(Duration::from_secs(30))
            .cookie_store(source.enabled_cookie_jar);
        if let Ok(proxy) = std::env::var("SHUKU_HTTP_PROXY").or_else(|_| std::env::var("SHUKU_SOCKS_PROXY")) {
            if !proxy.is_empty() {
                b = b.proxy(reqwest::Proxy::all(&proxy).map_err(|e| anyhow!("bad proxy: {e}"))?);
            }
        }
        let client = b.build().context("build http client")?;
        self.clients.write().unwrap().insert(key, client.clone());
        Ok(client)
    }
}

// ---------------------------------------------------------------------------
// Engine: fetch (cache + bucket + breaker) + rule execution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub level: String,
    pub module: String,
    pub msg: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_body_truncated: Option<String>,
    pub extracted: Value,
    pub js_output: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub logs: Vec<LogEntry>,
}

pub struct Engine {
    pub pool: HttpPool,
    pub buckets: crate::scheduler::RateLimiter,
    pub breaker: crate::scheduler::CircuitBreaker,
    pub cache: Cache,
    /// Last successful login per source id (loginUrl sources only).
    login_state: RwLock<HashMap<String, std::time::Instant>>,
}

/// Max pages followed for ruleToc.nextTocUrl / ruleContent.nextContentUrl.
const MAX_TOC_PAGES: usize = 20;
const MAX_CONTENT_PAGES: usize = 10;
/// A successful login is reused for this long before re-logging in.
const LOGIN_TTL: Duration = Duration::from_secs(30 * 60);

fn domain_of(url: &str) -> String {
    Url::parse(url).ok().and_then(|u| u.host_str().map(|s| s.to_string())).unwrap_or_default()
}

impl Engine {
    /// Test-only constructor with the documented defaults; production uses
    /// `with_limits` (env-configured throttling).
    #[cfg(test)]
    pub fn new(cache: Cache) -> Self {
        Self::with_limits(cache, 2.0, 5.0, 5, Duration::from_secs(60))
    }

    /// Same as `new`, but with the throttling parameters from Config
    /// (env vars). `new` keeps the documented defaults for tests.
    pub fn with_limits(
        cache: Cache,
        rate_per_sec: f64,
        rate_burst: f64,
        breaker_failures: u32,
        breaker_cooldown: Duration,
    ) -> Self {
        Self {
            pool: HttpPool::new(),
            buckets: crate::scheduler::RateLimiter::new(rate_per_sec, rate_burst),
            breaker: crate::scheduler::CircuitBreaker::new(breaker_failures, breaker_cooldown),
            cache,
            login_state: RwLock::new(HashMap::new()),
        }
    }

    /// Fetch with 3-level cache + per-domain token bucket + circuit breaker.
    /// `source` supplies per-source headers and the cookie jar; a 401/403
    /// triggers one loginUrl attempt + retry when `allow_login_retry`.
    /// Cache key includes method + body, so POST searches cache per query.
    /// Fetch with 3-level cache + per-domain token bucket + circuit breaker.
    /// `source` supplies per-source headers and the cookie jar; a 401/403
    /// triggers one loginUrl attempt + retry when `allow_login_retry`.
    /// Cache key includes method + body, so POST searches cache per query.
    /// (`fetch_spec` and `login` call each other, which async fns cannot do
    /// directly, so the raw round trip lives in `fetch_raw` and neither
    /// calls the other.)
    pub async fn fetch_spec(
        &self,
        source: &SourceDef,
        spec: &RequestSpec,
        kind: CacheKind,
        force_refresh: bool,
        logs: &mut Vec<LogEntry>,
        t0: std::time::Instant,
        allow_login_retry: bool,
    ) -> Result<(u16, String)> {
        let (status, body) = self.fetch_raw(source, spec, kind, force_refresh, logs, t0).await?;
        if (status == 401 || status == 403) && allow_login_retry && !source.login_url.is_empty() {
            logs.push(LogEntry { level: "info".into(), module: "login".into(), msg: format!("{status} for {}, trying loginUrl", spec.url), ms: t0.elapsed().as_millis() as u64 });
            match self.login(source, false, &mut *logs, t0).await {
                Ok((true, _)) => {
                    logs.push(LogEntry { level: "info".into(), module: "login".into(), msg: "login ok, retrying request".into(), ms: t0.elapsed().as_millis() as u64 });
                    let (status2, body2) = self.fetch_raw(source, spec, kind, force_refresh, &mut *logs, t0).await?;
                    return self.check_status(status2, body2, &spec.url);
                }
                Ok((false, out)) => logs.push(LogEntry { level: "warn".into(), module: "login".into(), msg: format!("login check failed ({out})"), ms: t0.elapsed().as_millis() as u64 }),
                Err(e) => logs.push(LogEntry { level: "warn".into(), module: "login".into(), msg: format!("login failed: {e}"), ms: t0.elapsed().as_millis() as u64 }),
            }
        }
        self.check_status(status, body, &spec.url)
    }

    /// Fetch one page for the rule picker: a request-rule template (the
    /// same syntax as searchUrl — `{key}`/`{{key}}`, trailing `,{...}`
    /// config for POST/headers/charset) is rendered with `keyword` and
    /// fetched through the engine's pool (charset decode, per-source
    /// cookie jar, rate limit, breaker). No caching, no login, no
    /// extraction — the raw page is returned for the picker to render.
    /// Returns (status, body). Bodies over 2 MB are refused: the picker
    /// renders the whole page in an iframe and a truncated DOM would
    /// generate wrong rules.
    pub async fn fetch_picker(
        &self,
        template: &str,
        keyword: &str,
        headers: Option<&HashMap<String, String>>,
    ) -> Result<(u16, String)> {
        // Base URL for relative templates: substitute the placeholders
        // and drop the trailing ,{...} config, then parse.
        let mut probe = template.trim().to_string();
        for pat in ["{{key}}", "{key}", "{{page}}", "{page}"] {
            probe = probe.replace(pat, "x");
        }
        if let Some(idx) = probe.find(",{") {
            probe.truncate(idx);
        }
        let base = Url::parse(&probe)
            .map_err(|_| anyhow!("invalid picker url {template:?} (need an absolute http(s) URL)"))?;
        if base.scheme() != "http" && base.scheme() != "https" {
            return Err(anyhow!("picker url must be http(s): {template:?}"));
        }
        let mut raw = serde_json::json!({
            "bookSourceUrl": base.origin().ascii_serialization(),
            "bookSourceName": "picker",
        });
        if let Some(h) = headers {
            raw["header"] = serde_json::Value::String(serde_json::to_string(h)?);
        }
        let source = SourceDef::from_raw(&raw);
        let spec = parse_request_rule(template, &base, keyword, 1, "")?;
        let mut logs = Vec::new();
        let t0 = std::time::Instant::now();
        let (status, body) = self
            .fetch_spec(&source, &spec, CacheKind::Search, true, &mut logs, t0, false)
            .await?;
        const MAX_PICKER_BODY: usize = 2_000_000;
        if body.len() > MAX_PICKER_BODY {
            return Err(anyhow!("page too large ({} bytes); picker supports up to 2 MB", body.len()));
        }
        Ok((status, body))
    }

    /// 4xx/5xx is a failed fetch, not an empty page: callers (crawl, test
    /// panel) must see it as an error instead of storing "".
    fn check_status(&self, status: u16, body: String, url: &str) -> Result<(u16, String)> {
        if status >= 400 {
            return Err(anyhow!("http {status} for {url}"));
        }
        Ok((status, body))
    }

    /// One HTTP round trip: cache, breaker, bucket, request, charset
    /// decode. Returns the status even for 4xx/5xx (callers decide); only
    /// transport errors are Err.
    async fn fetch_raw(
        &self,
        source: &SourceDef,
        spec: &RequestSpec,
        kind: CacheKind,
        force_refresh: bool,
        logs: &mut Vec<LogEntry>,
        t0: std::time::Instant,
    ) -> Result<(u16, String)> {
        let cache_key = match &spec.body {
            Some(b) => format!("{} {} {}", spec.method, spec.url, b),
            None => format!("{} {}", spec.method, spec.url),
        };
        if !force_refresh {
            if let Some(body) = self.cache.get(kind, &cache_key).await {
                logs.push(LogEntry { level: "info".into(), module: "crawl".into(), msg: format!("cache hit {}", spec.url), ms: t0.elapsed().as_millis() as u64 });
                return Ok((200, body));
            }
        }
        let domain = domain_of(&spec.url);
        self.breaker.check(&domain).map_err(|e| {
            logs.push(LogEntry { level: "warn".into(), module: "crawl".into(), msg: format!("circuit open for {domain}: {e}"), ms: t0.elapsed().as_millis() as u64 });
            e
        })?;
        self.buckets.acquire(&domain).await;
        // Source-level `header` (lowest priority), then per-request headers.
        let mut headers: Vec<(String, String)> = parse_header_json(&source.header).unwrap_or_default();
        headers.extend(spec.headers.iter().cloned());
        logs.push(LogEntry { level: "info".into(), module: "http".into(), msg: format!("{} {}", spec.method, spec.url), ms: t0.elapsed().as_millis() as u64 });
        let client = self.pool.client_for_source(&spec.url, source)?;
        let method = if spec.method == "POST" { reqwest::Method::POST } else { reqwest::Method::GET };
        let mut req = client.request(method, &spec.url);
        for (k, v) in &headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(b) = &spec.body {
            req = req.body(b.clone());
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                self.breaker.record_failure(&domain);
                logs.push(LogEntry { level: "error".into(), module: "http".into(), msg: format!("request failed: {e}"), ms: t0.elapsed().as_millis() as u64 });
                return Err(anyhow!("http request failed: {e}"));
            }
        };
        let status = resp.status().as_u16();
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = resp.bytes().await.unwrap_or_default();
        let body = decode_body(&bytes, spec.charset.as_deref(), ct.as_deref());
        if status >= 500 {
            self.breaker.record_failure(&domain);
        } else if status < 400 {
            self.breaker.record_success(&domain);
        }
        logs.push(LogEntry { level: "info".into(), module: "http".into(), msg: format!("{status} {} ({} bytes)", spec.url, body.len()), ms: t0.elapsed().as_millis() as u64 });
        if status < 400 {
            self.cache.put(kind, &cache_key, &body).await;
        }
        Ok((status, body))
    }


    /// Best-effort login before the first request of a loginUrl source.
    /// A failed login only warns: the site may still serve public content,
    /// and the test panel has a dedicated `login` kind that fails fast.
    async fn maybe_login(&self, source: &SourceDef, logs: &mut Vec<LogEntry>, t0: std::time::Instant) {
        if source.login_url.is_empty() {
            return;
        }
        match self.login(source, false, &mut *logs, t0).await {
            Ok((true, _)) => {}
            Ok((false, out)) => logs.push(LogEntry { level: "warn".into(), module: "login".into(), msg: format!("login check failed ({out}); continuing without auth"), ms: t0.elapsed().as_millis() as u64 }),
            Err(e) => logs.push(LogEntry { level: "warn".into(), module: "login".into(), msg: format!("login failed ({e}); continuing without auth"), ms: t0.elapsed().as_millis() as u64 }),
        }
    }

    /// Run the source's loginUrl (usually a POST; credentials live in the
    /// source JSON, as in real Legado sources). Cookies land in the
    /// per-source jar. Returns (logged_in, loginCheckJs output).
    /// With no loginCheckJs, any 2xx/3xx counts as logged in; with one,
    /// the script runs on the response body and must return "true".
    /// `force` ignores the cached login state.
    pub async fn login(&self, source: &SourceDef, force: bool, logs: &mut Vec<LogEntry>, t0: std::time::Instant) -> Result<(bool, String)> {
        if source.login_url.is_empty() {
            return Ok((false, "no loginUrl".into()));
        }
        if !force {
            if let Some(at) = self.login_state.read().unwrap().get(&source.id) {
                if at.elapsed() < LOGIN_TTL {
                    return Ok((true, "already logged in".into()));
                }
            }
        }
        let (template, js_lib) = (source.login_url.clone(), source.js_lib.clone());
        let base_url = source.base_url();
        let spec = tokio::task::spawn_blocking(move || parse_request_rule(&template, &base_url, "", 1, &js_lib))
            .await
            .map_err(|e| anyhow!("login task: {e}"))??;
        logs.push(LogEntry { level: "info".into(), module: "login".into(), msg: format!("{} {}", spec.method, spec.url), ms: t0.elapsed().as_millis() as u64 });
        let (status, body) = self.fetch_raw(source, &spec, CacheKind::Search, true, &mut *logs, t0).await?;
        // loginUi: fill the login form with JS (minimal document shim)
        // and submit it; the submit response is what loginCheckJs sees.
        let (status, body) = if source.login_ui.trim().is_empty() {
            (status, body)
        } else {
            let (script, body2, js_lib2, page_url2) = (
                source.login_ui.clone(),
                body.clone(),
                source.js_lib.clone(),
                spec.url.clone(),
            );
            let submit = tokio::task::spawn_blocking(move || {
                let page = Url::parse(&page_url2)?;
                crate::login_ui::run_login_ui(&body2, &script, &js_lib2, &page)
            })
            .await
            .map_err(|e| anyhow!("loginUi task: {e}"))??
            .ok_or_else(|| anyhow!("loginUi did not submit a form (no <form> on the login page, or the script never clicked/submitted)"))?;
            logs.push(LogEntry { level: "info".into(), module: "login".into(), msg: format!("loginUi submit {} {}", submit.method, submit.url), ms: t0.elapsed().as_millis() as u64 });
            let form_spec = crate::source::RequestSpec { url: submit.url, method: submit.method, body: submit.body, headers: vec![], charset: None };
            self.fetch_raw(source, &form_spec, CacheKind::Search, true, &mut *logs, t0).await?
        };
        let check = if source.login_check_js.trim().is_empty() {
            String::new()
        } else {
            let (script, body2, js_lib2, base2) = (source.login_check_js.clone(), body.clone(), source.js_lib.clone(), source.base_url());
            tokio::task::spawn_blocking(move || run_js_ext(&script, &body2, &js_lib2, base2.as_str(), "", JS_TIMEOUT))
                .await
                .map_err(|e| anyhow!("login check task: {e}"))??
        };
        let ok = if source.login_check_js.trim().is_empty() {
            status < 400
        } else {
            check.trim().eq_ignore_ascii_case("true")
        };
        if ok {
            self.login_state.write().unwrap().insert(source.id.clone(), std::time::Instant::now());
            logs.push(LogEntry { level: "info".into(), module: "login".into(), msg: "login ok".into(), ms: t0.elapsed().as_millis() as u64 });
        } else {
            logs.push(LogEntry { level: "warn".into(), module: "login".into(), msg: format!("login check failed (output: {check:?})"), ms: t0.elapsed().as_millis() as u64 });
        }
        Ok((ok, check))
    }

    /// Test-panel entry for the `login` kind.
    pub async fn run_login(&self, source: &SourceDef, force_refresh: bool) -> TestResult {
        let t0 = std::time::Instant::now();
        let mut logs = vec![];
        let fail = |logs: Vec<LogEntry>, err: String, t0: std::time::Instant| TestResult {
            ok: false,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            raw_status: None,
            raw_body_truncated: None,
            extracted: Value::Null,
            js_output: vec![],
            error: Some(err),
            logs,
        };
        match self.login(source, force_refresh, &mut logs, t0).await {
            Ok((ok, check)) => TestResult {
                ok,
                elapsed_ms: t0.elapsed().as_millis() as u64,
                raw_status: None,
                raw_body_truncated: None,
                extracted: serde_json::json!({ "logged_in": ok, "check_output": check }),
                js_output: vec![],
                error: if ok { None } else { Some("login check failed".into()) },
                logs,
            },
            Err(e) => fail(logs, e.to_string(), t0),
        }
    }

    /// Fetch an explore (discovery) page: exploreUrl + ruleExplore, same
    /// extraction as search. `page` is the {page} value (usually 1).
    pub async fn run_explore(&self, source: &SourceDef, page: i64, force_refresh: bool) -> TestResult {
        let t0 = std::time::Instant::now();
        let mut logs = vec![];
        let fail = |logs: Vec<LogEntry>, err: String, t0: std::time::Instant| TestResult {
            ok: false,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            raw_status: None,
            raw_body_truncated: None,
            extracted: Value::Null,
            js_output: vec![],
            error: Some(err),
            logs,
        };
        if source.explore_url.is_empty() {
            return fail(logs, "source has no exploreUrl".into(), t0);
        }
        self.maybe_login(source, &mut logs, t0).await;
        let (template, js_lib) = (source.explore_url.clone(), source.js_lib.clone());
        let base_url = source.base_url();
        let spec = match tokio::task::spawn_blocking(move || parse_request_rule(&template, &base_url, "", page, &js_lib)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return fail(logs, format!("bad explore url: {e}"), t0),
            Err(e) => return fail(logs, format!("blocking task: {e}"), t0),
        };
        let base = match Url::parse(&spec.url) {
            Ok(u) => u,
            Err(e) => return fail(logs, format!("bad explore url: {e}"), t0),
        };
        let (status, body) = match self.fetch_spec(source, &spec, CacheKind::Search, force_refresh, &mut logs, t0, true).await {
            Ok(v) => v,
            Err(e) => return fail(logs, e.to_string(), t0),
        };
        let raw_body_truncated: String = body.chars().take(8192).collect();
        let (rule, js_lib, base_url) = (source.rule_explore.clone(), source.js_lib.clone(), base.clone());
        let extracted = tokio::task::spawn_blocking(move || extract_search(&body, &rule, &base_url, &js_lib)).await;
        match extracted {
            Ok(Ok((books, js_out))) => {
                logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: format!("explore: extracted {} books", books.len()), ms: t0.elapsed().as_millis() as u64 });
                TestResult {
                    ok: true,
                    elapsed_ms: t0.elapsed().as_millis() as u64,
                    raw_status: Some(status),
                    raw_body_truncated: Some(raw_body_truncated),
                    extracted: serde_json::to_value(&books).unwrap_or(Value::Null),
                    js_output: js_out,
                    error: None,
                    logs,
                }
            }
            Ok(Err(e)) => fail(logs, format!("extract: {e}"), t0),
            Err(e) => fail(logs, format!("blocking task: {e}"), t0),
        }
    }

    /// Search across up to `max_pages` pages (clamped 1..=10), stopping
    /// early when a page returns no books. Page 1's status/body are kept
    /// as the raw response; all pages' books are aggregated.
    pub async fn run_search_pages(&self, source: &SourceDef, keyword: &str, start_page: i64, max_pages: i64, force_refresh: bool) -> TestResult {
        let t0 = std::time::Instant::now();
        let mut logs = vec![];
        let fail = |logs: Vec<LogEntry>, err: String, t0: std::time::Instant| TestResult {
            ok: false,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            raw_status: None,
            raw_body_truncated: None,
            extracted: Value::Null,
            js_output: vec![],
            error: Some(err),
            logs,
        };
        if source.search_url.is_empty() {
            return fail(logs, "source has no searchUrl".into(), t0);
        }
        self.maybe_login(source, &mut logs, t0).await;
        let max_pages = max_pages.clamp(1, 10);
        let mut all_books: Vec<BookHit> = Vec::new();
        let mut all_js: Vec<String> = Vec::new();
        let mut raw_status: Option<u16> = None;
        let mut raw_body_truncated: Option<String> = None;
        let mut first_page: Option<(String, String)> = None;
        for p in 0..max_pages {
            let page = start_page + p;
            let keyword_owned = keyword.to_string();
            let (template, js_lib) = (source.search_url.clone(), source.js_lib.clone());
            let base_url = source.base_url();
            let spec = match tokio::task::spawn_blocking(move || parse_request_rule(&template, &base_url, &keyword_owned, page, &js_lib)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return fail(logs, format!("bad search url: {e}"), t0),
                Err(e) => return fail(logs, format!("blocking task: {e}"), t0),
            };
            let base = match Url::parse(&spec.url) {
                Ok(u) => u,
                Err(e) => return fail(logs, format!("bad search url: {e}"), t0),
            };
            let (status, body) = match self.fetch_spec(source, &spec, CacheKind::Search, force_refresh, &mut logs, t0, true).await {
                Ok(v) => v,
                Err(e) => return fail(logs, e.to_string(), t0),
            };
            if raw_status.is_none() {
                raw_status = Some(status);
                raw_body_truncated = Some(body.chars().take(8192).collect());
                first_page = Some((body.clone(), spec.url.clone()));
            }
            let (rule, js_lib, base_url) = (source.rule_search.clone(), source.js_lib.clone(), base.clone());
            let extracted = tokio::task::spawn_blocking(move || extract_search(&body, &rule, &base_url, &js_lib)).await;
            match extracted {
                Ok(Ok((books, js_out))) => {
                    logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: format!("page {page}: extracted {} books", books.len()), ms: t0.elapsed().as_millis() as u64 });
                    if books.is_empty() {
                        break;
                    }
                    all_books.extend(books);
                    all_js.extend(js_out);
                }
                Ok(Err(e)) => return fail(logs, format!("extract: {e}"), t0),
                Err(e) => return fail(logs, format!("blocking task: {e}"), t0),
            }
        }
        // Fallback: no books at all, and the page may actually be a book
        // detail page — try ruleBookInfo on the first page (only when no
        // bookUrlPattern is configured, per the source doc).
        if all_books.is_empty() {
            if let Some((body, page_url)) = first_page {
                if source.book_url_pattern.trim().is_empty() && source.rule_book_info.is_object() {
                    let (rule, js_lib) = (source.rule_book_info.clone(), source.js_lib.clone());
                    let base = Url::parse(&page_url).unwrap_or_else(|_| source.base_url());
                    let info = tokio::task::spawn_blocking(move || extract_book_info(&body, &rule, &base, &js_lib)).await;
                    if let Ok(Ok(Some(b))) = info {
                        logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: "no search results; fell back to ruleBookInfo detail page".into(), ms: t0.elapsed().as_millis() as u64 });
                        all_books.push(b);
                    }
                }
            }
        }
        logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: format!("extracted {} books", all_books.len()), ms: t0.elapsed().as_millis() as u64 });
        TestResult {
            ok: true,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            raw_status,
            raw_body_truncated,
            extracted: serde_json::to_value(&all_books).unwrap_or(Value::Null),
            js_output: all_js,
            error: None,
            logs,
        }
    }

    /// Fetch the toc, following ruleToc.nextTocUrl up to MAX_TOC_PAGES
    /// pages (stops on empty / repeated / same-as-current next URL).
    pub async fn run_toc(&self, source: &SourceDef, book_url: &str, force_refresh: bool) -> TestResult {
        let t0 = std::time::Instant::now();
        let mut logs = vec![];
        let fail = |logs: Vec<LogEntry>, err: String, t0: std::time::Instant| TestResult {
            ok: false,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            raw_status: None,
            raw_body_truncated: None,
            extracted: Value::Null,
            js_output: vec![],
            error: Some(err),
            logs,
        };
        if Url::parse(book_url).is_err() {
            return fail(logs, format!("bad book url: {book_url}"), t0);
        }
        self.maybe_login(source, &mut logs, t0).await;
        let mut all: Vec<ChapterHit> = Vec::new();
        let mut all_js: Vec<String> = Vec::new();
        let mut raw_status: Option<u16> = None;
        let mut raw_body_truncated: Option<String> = None;
        let mut url = book_url.to_string();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for page_no in 1..=MAX_TOC_PAGES {
            if !seen.insert(url.clone()) {
                logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: "toc next url repeats, stopping".into(), ms: t0.elapsed().as_millis() as u64 });
                break;
            }
            let spec = RequestSpec { url: url.clone(), method: "GET".into(), body: None, headers: vec![], charset: None };
            let (status, body) = match self.fetch_spec(source, &spec, CacheKind::Toc, force_refresh, &mut logs, t0, true).await {
                Ok(v) => v,
                Err(e) => return fail(logs, e.to_string(), t0),
            };
            if raw_status.is_none() {
                raw_status = Some(status);
                raw_body_truncated = Some(body.chars().take(8192).collect());
            }
            let page_base = Url::parse(&url).unwrap_or_else(|_| Url::parse(book_url).unwrap());
            let (rule, js_lib) = (source.rule_toc.clone(), source.js_lib.clone());
            let extracted = tokio::task::spawn_blocking(move || extract_toc_page(&body, &rule, &page_base, &js_lib)).await;
            match extracted {
                Ok(Ok((chapters, next, js_out))) => {
                    logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: format!("toc page {page_no}: extracted {} chapters", chapters.len()), ms: t0.elapsed().as_millis() as u64 });
                    all.extend(chapters);
                    all_js.extend(js_out);
                    let next = next.trim().to_string();
                    if next.is_empty() || next == url {
                        break;
                    }
                    url = next;
                }
                Ok(Err(e)) => return fail(logs, format!("extract: {e}"), t0),
                Err(e) => return fail(logs, format!("blocking task: {e}"), t0),
            }
        }
        logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: format!("extracted {} chapters", all.len()), ms: t0.elapsed().as_millis() as u64 });
        TestResult {
            ok: true,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            raw_status,
            raw_body_truncated,
            extracted: serde_json::to_value(&all).unwrap_or(Value::Null),
            js_output: all_js,
            error: None,
            logs,
        }
    }

    /// Fetch the chapter, following ruleContent.nextContentUrl up to
    /// MAX_CONTENT_PAGES pages (stops on empty / repeated / same-as-current
    /// next URL). Paragraphs from all pages are concatenated in order.
    pub async fn run_content(&self, source: &SourceDef, chapter_url: &str, force_refresh: bool) -> TestResult {
        let t0 = std::time::Instant::now();
        let mut logs = vec![];
        let fail = |logs: Vec<LogEntry>, err: String, t0: std::time::Instant| TestResult {
            ok: false,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            raw_status: None,
            raw_body_truncated: None,
            extracted: Value::Null,
            js_output: vec![],
            error: Some(err),
            logs,
        };
        if Url::parse(chapter_url).is_err() {
            return fail(logs, format!("bad chapter url: {chapter_url}"), t0);
        }
        self.maybe_login(source, &mut logs, t0).await;
        let mut all: Vec<Paragraph> = Vec::new();
        let mut all_js: Vec<String> = Vec::new();
        let mut raw_status: Option<u16> = None;
        let mut raw_body_truncated: Option<String> = None;
        let mut url = chapter_url.to_string();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for page_no in 1..=MAX_CONTENT_PAGES {
            if !seen.insert(url.clone()) {
                logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: "content next url repeats, stopping".into(), ms: t0.elapsed().as_millis() as u64 });
                break;
            }
            let spec = RequestSpec { url: url.clone(), method: "GET".into(), body: None, headers: vec![], charset: None };
            let (status, body) = match self.fetch_spec(source, &spec, CacheKind::Content, force_refresh, &mut logs, t0, true).await {
                Ok(v) => v,
                Err(e) => return fail(logs, e.to_string(), t0),
            };
            if raw_status.is_none() {
                raw_status = Some(status);
                raw_body_truncated = Some(body.chars().take(8192).collect());
            }
            let page_base = Url::parse(&url).unwrap_or_else(|_| Url::parse(chapter_url).unwrap());
            let (rule, js_lib) = (source.rule_content.clone(), source.js_lib.clone());
            let extracted = tokio::task::spawn_blocking(move || extract_content_page(&body, &rule, &page_base, &js_lib)).await;
            match extracted {
                Ok(Ok((paras, next, js_out))) => {
                    logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: format!("content page {page_no}: extracted {} paragraphs", paras.len()), ms: t0.elapsed().as_millis() as u64 });
                    all.extend(paras);
                    all_js.extend(js_out);
                    let next = next.trim().to_string();
                    if next.is_empty() || next == url {
                        break;
                    }
                    url = next;
                }
                Ok(Err(e)) => return fail(logs, format!("extract: {e}"), t0),
                Err(e) => return fail(logs, format!("blocking task: {e}"), t0),
            }
        }
        logs.push(LogEntry { level: "info".into(), module: "search".into(), msg: format!("extracted {} paragraphs", all.len()), ms: t0.elapsed().as_millis() as u64 });
        TestResult {
            ok: true,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            raw_status,
            raw_body_truncated,
            extracted: serde_json::to_value(&all).unwrap_or(Value::Null),
            js_output: all_js,
            error: None,
            logs,
        }
    }
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_HTML: &str = r#"
        <html><body>
        <div class="book"><a class="title" href="/book/1">修仙传</a><span class="author">作者甲</span></div>
        <div class="book"><a class="title" href="/book/2">都市传奇</a><span class="author">作者乙</span></div>
        </body></html>"#;

    #[test]
    fn selector_and_regex_cache() {
        let s1 = get_selector("div.book").unwrap();
        let s2 = get_selector("div.book").unwrap();
        assert!(Arc::ptr_eq(&s1, &s2));
        assert!(get_selector("div > >").is_err());
        assert!(get_regex("[a-").is_err());
        assert!(get_regex(r"\d+").is_ok());
    }

    #[test]
    fn validate_source_rules() {
        let raw = serde_json::json!({
            "bookSourceUrl": "https://example.com",
            "bookSourceName": "test",
            "searchUrl": "https://example.com/s?q={{key}}",
            "ruleSearch": {"bookList": "div.book", "name": "a.title@text", "author": ".author@text", "bookUrl": "a.title@href"},
            "ruleToc": {"chapterList": "a.chapter", "chapterName": "@text", "chapterUrl": "@href"},
            "ruleContent": "#content@text"
        });
        assert!(validate_source(&raw).is_ok());
        let bad = serde_json::json!({"ruleSearch": {"bookList": "div > >"}});
        assert!(validate_source(&bad).is_err());
        let bad_js = serde_json::json!({"ruleContent": "js:result.."});
        assert!(validate_source(&bad_js).is_err());
        // Identity is required: a bare string or a nameless object is not a source.
        assert!(validate_source(&serde_json::json!("nope")).is_err());
        assert!(validate_source(&serde_json::json!({"bookSourceUrl": "https://example.com"})).is_err());
        assert!(validate_source(&serde_json::json!({"bookSourceName": "test"})).is_err());
    }

    #[test]
    fn extract_search_list() {
        let rule = serde_json::json!({
            "bookList": "div.book",
            "name": "a.title@text",
            "author": ".author@text",
            "bookUrl": "a.title@href"
        });
        let base = Url::parse("https://example.com/").unwrap();
        let (books, _) = extract_search(SEARCH_HTML, &rule, &base, "").unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].name, "修仙传");
        assert_eq!(books[0].author, "作者甲");
        assert_eq!(books[0].book_url, "https://example.com/book/1");
    }

    #[test]
    fn extract_toc_and_content() {
        let toc_html = r#"<html><body><ul><li><a class="chapter" href="/c/1">第一章 开始</a></li><li><a class="chapter" href="/c/2">第二章 练功</a></li></ul></body></html>"#;
        let rule = serde_json::json!({"chapterList": "a.chapter", "chapterName": "@text", "chapterUrl": "@href"});
        let base = Url::parse("https://example.com/book/1").unwrap();
        let (chs, _) = extract_toc(toc_html, &rule, &base, "").unwrap();
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[0].name, "第一章 开始");
        assert_eq!(chs[0].url, "https://example.com/c/1");

        let content_html = r#"<html><body><div id="content"><p>第一段文字。</p><p>第二段文字。</p><img src="/img/1.png"/></div></body></html>"#;
        let (paras, _) = extract_content(content_html, &serde_json::json!("#content"), &base, "").unwrap();
        assert!(paras.iter().any(|p| p.text.as_deref() == Some("第一段文字。")));
        assert!(paras.iter().any(|p| p.img.as_deref() == Some("https://example.com/img/1.png")));
    }

    #[test]
    fn js_cleaning_works() {
        let out = run_js("result = result.replace(/广告/g, '').trim()", "正文广告内容", "", Duration::from_secs(5)).unwrap();
        assert_eq!(out, "正文内容");
    }

    #[test]
    fn js_expression_return() {
        let out = run_js("result.toUpperCase()", "abc", "", Duration::from_secs(5)).unwrap();
        assert_eq!(out, "ABC");
    }

    #[test]
    fn js_infinite_loop_is_interrupted() {
        let t0 = std::time::Instant::now();
        let r = run_js("while(true){}", "x", "", Duration::from_secs(2));
        assert!(r.is_err(), "infinite loop must be interrupted");
        assert!(t0.elapsed() < Duration::from_secs(10), "interrupt must be prompt, took {:?}", t0.elapsed());
    }

    #[test]
    fn js_memory_bomb_is_limited() {
        // 16MB limit: allocating a huge string must fail, not OOM the process.
        let r = run_js("let a=''; while(true){ a += 'xxxxxxxxxx'; }", "x", "", Duration::from_secs(5));
        assert!(r.is_err());
    }

    #[test]
    fn js_no_network_or_file_access() {
        // QuickJS has no fetch / require / process by default.
        assert!(run_js("fetch('https://example.com')", "x", "", Duration::from_secs(2)).is_err());
        assert!(run_js("require('fs')", "x", "", Duration::from_secs(2)).is_err());
        assert!(run_js("setTimeout(function(){}, 1)", "x", "", Duration::from_secs(2)).is_err());
    }

    #[test]
    fn js_syntax_validation() {
        assert!(validate_js_syntax("result.trim()").is_ok());
        assert!(validate_js_syntax("function broken( {").is_err());
    }

    // --- Advanced rules: request rules, login, pagination, webJs ---

    #[test]
    fn request_rule_get_single_brace() {
        // Single-brace {key}/{page} (the form real Legado sources use).
        let base = Url::parse("https://example.com/").unwrap();
        let spec = parse_request_rule("https://example.com/s?q={key}&page={page}", &base, "修仙", 2, "").unwrap();
        assert_eq!(spec.method, "GET");
        assert_eq!(spec.url, "https://example.com/s?q=%E4%BF%AE%E4%BB%99&page=2");
        assert!(spec.body.is_none());
    }

    #[test]
    fn request_rule_post_with_config() {
        let base = Url::parse("https://example.com/").unwrap();
        let spec = parse_request_rule(
            "/search,{\"method\":\"POST\",\"body\":\"q={{key}}&p={{page}}\",\"headers\":{\"X-Test\":\"yes\"},\"charset\":\"gbk\"}",
            &base,
            "abc",
            3,
            "",
        )
        .unwrap();
        assert_eq!(spec.method, "POST");
        assert_eq!(spec.url, "https://example.com/search");
        assert_eq!(spec.body.as_deref(), Some("q=abc&p=3"));
        assert_eq!(spec.headers, vec![("X-Test".to_string(), "yes".to_string())]);
        assert_eq!(spec.charset.as_deref(), Some("gbk"));
    }

    #[test]
    fn request_rule_js_in_template() {
        let base = Url::parse("https://example.com/").unwrap();
        let spec = parse_request_rule("/s?x={{key + '-v2'}}&p={{page * 2}}", &base, "abc", 2, "").unwrap();
        assert_eq!(spec.url, "https://example.com/s?x=abc-v2&p=4");
    }

    #[test]
    fn request_rule_bad_config() {
        let base = Url::parse("https://example.com/").unwrap();
        assert!(parse_request_rule("/s,{\"method\":\"POST\"", &base, "x", 1, "").is_err());
        assert!(parse_request_rule("/s,{\"method\":\"PUT\"}", &base, "x", 1, "").is_err());
        assert!(parse_request_rule("", &base, "x", 1, "").is_err());
    }

    #[test]
    fn validate_source_single_brace_url() {
        // Regression: the old validator ran Url::parse on the raw template,
        // which rejects `{key}` — standard Legado sources failed to import.
        let raw = serde_json::json!({
            "bookSourceName": "test",
            "bookSourceUrl": "https://example.com",
            "bookSourceName": "test",
            "searchUrl": "https://example.com/s?q={key}&page={page}",
            "ruleSearch": {"bookList": "div.book"}
        });
        assert!(validate_source(&raw).is_ok());
    }

    #[test]
    fn validate_source_advanced_fields() {
        let raw = serde_json::json!({
            "bookSourceUrl": "https://example.com",
            "bookSourceName": "test",
            "searchUrl": "/search,{\"method\":\"POST\",\"body\":\"q={{key}}\"}",
            "loginUrl": "/login,{\"method\":\"POST\",\"body\":\"u=a&p=b\"}",
            "loginCheckJs": "result.includes('ok') ? 'true' : 'false'",
            "header": "{\"User-Agent\":\"test\"}",
            "ruleToc": {"chapterList": "a.ch", "nextTocUrl": "a.next@href"},
            "ruleContent": {"content": "#c", "nextContentUrl": "a.next@href", "webJs": "result.replace('X','')", "sourceRegex": "广告##", "replaceRegex": "XX##"}
        });
        assert!(validate_source(&raw).is_ok(), "advanced source should validate: {:?}", validate_source(&raw).err());
        let bad_js = serde_json::json!({"ruleContent": {"content": "#c", "webJs": "function broken( {"}});
        assert!(validate_source(&bad_js).is_err());
        let bad_header = serde_json::json!({"header": "not json"});
        assert!(validate_source(&bad_header).is_err());
        let bad_next = serde_json::json!({"ruleToc": {"nextTocUrl": "div > >"}});
        assert!(validate_source(&bad_next).is_err());
    }

    #[test]
    fn decode_body_charset() {
        // "中文测试" in GBK.
        let gbk = encoding_rs::GBK.encode("中文测试").0;
        assert_eq!(decode_body(&gbk, Some("gbk"), None), "中文测试");
        assert_eq!(decode_body("hello".as_bytes(), None, Some("text/html; charset=utf-8")), "hello");
        assert_eq!(decode_body("hello".as_bytes(), None, None), "hello");
    }

    #[test]
    fn content_webjs_pipeline() {
        let html = r#"<html><body><div class="ad">广告横幅</div><div id="c"><p>正文XX第一段广告。</p><p>第二段。</p></div><a class="next" href="/c/2">下一页</a></body></html>"#;
        let rule = serde_json::json!({
            "content": "#c",
            "sourceRegex": "(?s)<div class=\"ad\">.*?</div>##",
            "webJs": "result.replace(/XX/g, '')",
            "replaceRegex": "广告##",
            "nextContentUrl": "a.next@href"
        });
        let base = Url::parse("https://example.com/c/1").unwrap();
        let (paras, next, _js) = extract_content_page(html, &rule, &base, "").unwrap();
        let texts: Vec<&str> = paras.iter().filter_map(|p| p.text.as_deref()).collect();
        assert!(texts.iter().any(|t| t.contains("正文第一段")), "texts: {texts:?}");
        assert!(!texts.iter().any(|t| t.contains("广告") || t.contains("XX")), "texts: {texts:?}");
        assert_eq!(next, "https://example.com/c/2");
    }

    #[test]
    fn toc_next_url() {
        let html = r#"<html><body><a class="ch" href="/c/1">第一章</a><a class="next" href="/toc?page=2">下一页</a></body></html>"#;
        let rule = serde_json::json!({"chapterList": "a.ch", "chapterName": "@text", "chapterUrl": "@href", "nextTocUrl": "a.next@href"});
        let base = Url::parse("https://example.com/toc?page=1").unwrap();
        let (chs, next, _) = extract_toc_page(html, &rule, &base, "").unwrap();
        assert_eq!(chs.len(), 1);
        assert_eq!(next, "https://example.com/toc?page=2");
        let html2 = r#"<html><body><a class="ch" href="/c/2">第二章</a></body></html>"#;
        let (chs2, next2, _) = extract_toc_page(html2, &rule, &base, "").unwrap();
        assert_eq!(chs2.len(), 1);
        assert!(next2.is_empty());
    }

    // --- XPath / @json rules ---

    #[test]
    fn extract_search_xpath() {
        let rule = serde_json::json!({
            "bookList": "//div[@class='book']",
            "name": ".//a/text()",
            "author": ".//span[@class='author']/text()",
            "bookUrl": ".//a/@href"
        });
        let base = Url::parse("https://example.com/").unwrap();
        let (books, _) = extract_search(SEARCH_HTML, &rule, &base, "").unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].name, "修仙传");
        assert_eq!(books[0].author, "作者甲");
        assert_eq!(books[0].book_url, "https://example.com/book/1");
    }

    #[test]
    fn extract_search_json() {
        let json = r#"{"data":[{"title":"JSON书一","author":"甲","url":"/book/1"},{"title":"JSON书二","author":"乙","url":"/book/2"}],"total":2}"#;
        let rule = serde_json::json!({
            "bookList": "@json:$.data[*]",
            "name": "@json:$.title",
            "author": "@json:$.author",
            "bookUrl": "$.url"
        });
        let base = Url::parse("https://example.com/").unwrap();
        let (books, _) = extract_search(json, &rule, &base, "").unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[1].name, "JSON书二");
        assert_eq!(books[1].book_url, "https://example.com/book/2");
    }

    #[test]
    fn extract_toc_and_content_json() {
        let base = Url::parse("https://example.com/").unwrap();
        let toc_json = r#"{"chapters":[{"name":"第一章","url":"/c/1"},{"name":"第二章","url":"/c/2"}]}"#;
        let rule = serde_json::json!({"chapterList": "$.chapters[*]", "chapterName": "name", "chapterUrl": "url"});
        let (chs, _) = extract_toc(toc_json, &rule, &base, "").unwrap();
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[0].url, "https://example.com/c/1");

        let content_json = r#"{"content":"第一段。\n第二段。"}"#;
        let (paras, _) = extract_content(content_json, &serde_json::json!({"content": "@json:$.content"}), &base, "").unwrap();
        let texts: Vec<&str> = paras.iter().filter_map(|p| p.text.as_deref()).collect();
        assert_eq!(texts, vec!["第一段。", "第二段。"]);
    }

    #[test]
    fn validate_source_xpath_json() {
        let raw = serde_json::json!({
            "bookSourceUrl": "https://example.com",
            "bookSourceName": "test",
            "ruleSearch": {"bookList": "//div[@class='book']", "name": ".//a/text()", "bookUrl": ".//a/@href"},
            "ruleToc": {"chapterList": "@json:$.chapters[*]", "chapterName": "@json:$.name"},
            "ruleContent": {"content": "@json:$.content", "nextContentUrl": "@json:$.next"}
        });
        assert!(validate_source(&raw).is_ok(), "{:?}", validate_source(&raw).err());
        let bad_xpath = serde_json::json!({"ruleSearch": {"bookList": "//div[foo()]"}});
        assert!(validate_source(&bad_xpath).is_err());
        let bad_json = serde_json::json!({"ruleSearch": {"bookList": "@json:$.data[1:2]"}});
        assert!(validate_source(&bad_json).is_err());
        // Filters are valid now (see jsonpath_filter).
        let ok_json = serde_json::json!({"bookSourceUrl": "https://example.com", "bookSourceName": "test", "ruleSearch": {"bookList": "@json:$.data[?(@.x==1)]"}});
        assert!(validate_source(&ok_json).is_ok());
        // A @json rule on an HTML page is an error, not an empty result.
        let base = Url::parse("https://example.com/").unwrap();
        assert!(extract_search(SEARCH_HTML, &serde_json::json!({"bookList": "@json:$.data[*]"}), &base, "").is_err());
    }

    // --- Fixture HTTP server: POST search, paginated toc/content, login ---

    fn fixture_route(method: &str, path: &str, body: &str, cookie: &str, base: &str) -> (u16, String, String) {
        if method == "POST" && path == "/search" {
            if body.contains("UpdateBook") {
                return (200, String::new(), r#"<html><body><div class="book"><a class="title" href="/toc-update">UpdateBook</a><span class="author">甲</span></div></body></html>"#.into());
            }
            let books = if body.contains("page=1") {
                r#"<div class="book"><a class="title" href="/book/1">书一</a><span class="author">甲</span></div><div class="book"><a class="title" href="/book/2">书二</a><span class="author">乙</span></div>"#
            } else if body.contains("page=2") {
                r#"<div class="book"><a class="title" href="/book/3">书三</a><span class="author">丙</span></div>"#
            } else {
                ""
            };
            return (200, String::new(), format!("<html><body>{books}</body></html>"));
        }
        if method == "GET" && (path == "/toc" || path.starts_with("/toc?")) {
            if path.contains("page=2") {
                return (200, String::new(), r#"<html><body><a class="ch" href="/c/3">第三章</a></body></html>"#.into());
            }
            return (200, String::new(), r#"<html><body><a class="ch" href="/c/1">第一章</a><a class="ch" href="/c/2">第二章</a><a class="next" href="/toc?page=2">下一页</a></body></html>"#.into());
        }
        if method == "GET" && path.starts_with("/chapter") {
            if path.contains("page=2") {
                return (200, String::new(), r#"<html><body><div id="content"><p>第二页正文。</p></div></body></html>"#.into());
            }
            return (200, String::new(), r#"<html><body><div id="content"><p>第一页正文。</p></div><a class="next" href="/chapter?page=2">下一页</a></body></html>"#.into());
        }
        if method == "POST" && path == "/login" {
            return (200, "Set-Cookie: session=abc123; Path=/; HttpOnly\r\n".into(), "welcome ok".into());
        }
        if method == "GET" && path.starts_with("/api/search") {
            return (200, String::new(), r#"{"data":[{"title":"JSON书一","author":"甲","url":"/book/1"},{"title":"JSON书二","author":"乙","url":"/book/2"}]}"#.into());
        }
        if method == "GET" && path == "/api/toc" {
            return (200, String::new(), r#"{"chapters":[{"name":"第一章","url":"/c/1"},{"name":"第二章","url":"/c/2"}]}"#.into());
        }
        if method == "GET" && path == "/api/content" {
            return (200, String::new(), r#"{"content":"第一段。\n第二段。"}"#.into());
        }
        if method == "GET" && path == "/vip" {
            if cookie.contains("session=abc123") {
                return (200, String::new(), r#"<html><body><div class="book"><a class="title" href="/book/9">会员书</a><span class="author">丁</span></div></body></html>"#.into());
            }
            return (401, String::new(), "need login".into());
        }
        if method == "GET" && path == "/explore" {
            return (200, String::new(), r#"<html><body><div class="book"><a class="title" href="/book/7">发现书</a><span class="author">戊</span></div><div class="book"><a class="title" href="/book/8">发现书二</a><span class="author">己</span></div></body></html>"#.into());
        }
        if method == "GET" && path == "/login-form" {
            return (200, String::new(), r#"<html><body><form action="/login-form" method="post"><input name="username" type="text"/><input name="password" type="password"/><button type="submit">登录</button></form></body></html>"#.into());
        }
        if method == "POST" && path == "/login-form" {
            if body.contains("username=alice") && body.contains("password=s3cret") {
                return (200, "Set-Cookie: session=abc123; Path=/; HttpOnly\r\n".into(), "login ok".into());
            }
            return (401, String::new(), "bad credentials".into());
        }
        if method == "GET" && path == "/bookinfo" {
            return (200, String::new(), r#"<html><body><h1 class="title">详情书</h1><span class="author">庚</span></body></html>"#.into());
        }
        if method == "GET" && path == "/toc-update" {
            // A TOC that grows: 4 chapters on one page (distinct URLs, same
            // content page) — for update-check tests.
            return (200, String::new(), r#"<html><body><a class="ch" href="/chapter?n=1">第一章</a><a class="ch" href="/chapter?n=2">第二章</a><a class="ch" href="/chapter?n=3">第三章</a><a class="ch" href="/chapter?n=4">第四章</a></body></html>"#.into());
        }
        if method == "GET" && path == "/subscription" {
            // One valid source + one invalid entry (tests skip behavior).
            let body = format!(
                r#"["nope",{{"bookSourceUrl":"{base}/sub","bookSourceName":"fixture-sub","searchUrl":"/search","ruleSearch":{{"bookList":"div.book","name":"a.title@text","author":".author@text","bookUrl":"a.title@href"}}}}]"#
            );
            return (200, String::new(), body);
        }
        if method == "GET" && path == "/subscription-bad" {
            return (200, String::new(), "this is not json".into());
        }
        (404, String::new(), "not found".into())
    }

    async fn spawn_fixture() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut req: Vec<u8> = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                req.extend_from_slice(&buf[..n]);
                                if req.windows(4).any(|w| w == b"\r\n\r\n") || req.len() > 65536 {
                                    break;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&req).into_owned();
                    let mut lines = text.lines();
                    let first = lines.next().unwrap_or("");
                    let mut parts = first.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let mut clen = 0usize;
                    let mut cookie = String::new();
                    for l in text.lines().skip(1) {
                        if l.is_empty() {
                            break;
                        }
                        let low = l.to_ascii_lowercase();
                        if let Some(v) = low.strip_prefix("content-length:") {
                            clen = v.trim().parse().unwrap_or(0);
                        } else if let Some(v) = low.strip_prefix("cookie:") {
                            cookie = v.trim().to_string();
                        }
                    }
                    let header_end = req.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
                    let mut body = req[header_end..].to_vec();
                    while body.len() < clen {
                        match sock.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => body.extend_from_slice(&buf[..n]),
                            Err(_) => break,
                        }
                    }
                    let body_str = String::from_utf8_lossy(&body).into_owned();
                    let base = format!("http://127.0.0.1:{port}");
                    let (status, extra, resp_body) = fixture_route(&method, &path, &body_str, &cookie, &base);
                    let reason = match status {
                        200 => "OK",
                        401 => "Unauthorized",
                        _ => "Not Found",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        port
    }

    fn fixture_source(port: u16, with_login: bool) -> SourceDef {
        let base = format!("http://127.0.0.1:{port}");
        let mut raw = serde_json::json!({
            "bookSourceUrl": base,
            "bookSourceName": "fixture",
            "searchUrl": "/search,{\"method\":\"POST\",\"body\":\"q={{key}}&page={{page}}\"}",
            "ruleSearch": {"bookList": "div.book", "name": "a.title@text", "author": ".author@text", "bookUrl": "a.title@href"},
            "ruleToc": {"chapterList": "a.ch", "chapterName": "@text", "chapterUrl": "@href", "nextTocUrl": "a.next@href"},
            "ruleContent": {"content": "#content", "nextContentUrl": "a.next@href"}
        });
        if with_login {
            raw["loginUrl"] = serde_json::json!("/login,{\"method\":\"POST\",\"body\":\"user=test&pass=test\"}");
            raw["loginCheckJs"] = serde_json::json!("result.includes('welcome') ? 'true' : 'false'");
        }
        SourceDef::from_raw(&raw)
    }

    #[tokio::test]
    async fn engine_post_search_pagination() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        let source = fixture_source(port, false);
        let res = engine.run_search_pages(&source, "x", 1, 3, true).await;
        assert!(res.ok, "search failed: {:?}", res.error);
        let books: Vec<BookHit> = serde_json::from_value(res.extracted).unwrap();
        assert_eq!(books.len(), 3, "pages 1+2 should yield 3 books, page 3 empty stops");
        assert_eq!(books[2].name, "书三");
    }

    #[tokio::test]
    async fn engine_toc_and_content_pagination() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        let source = fixture_source(port, false);
        let toc = engine.run_toc(&source, &format!("http://127.0.0.1:{port}/toc?page=1"), true).await;
        assert!(toc.ok, "toc failed: {:?}", toc.error);
        let chs: Vec<ChapterHit> = serde_json::from_value(toc.extracted).unwrap();
        assert_eq!(chs.len(), 3);
        assert_eq!(chs[2].name, "第三章");
        let content = engine.run_content(&source, &format!("http://127.0.0.1:{port}/chapter?page=1"), true).await;
        assert!(content.ok, "content failed: {:?}", content.error);
        let paras: Vec<Paragraph> = serde_json::from_value(content.extracted).unwrap();
        let texts: Vec<&str> = paras.iter().filter_map(|p| p.text.as_deref()).collect();
        assert!(texts.iter().any(|t| t.contains("第一页正文")), "texts: {texts:?}");
        assert!(texts.iter().any(|t| t.contains("第二页正文")), "texts: {texts:?}");
    }

    #[tokio::test]
    async fn engine_login_and_cookie_jar() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        // Direct login: POST /login sets a cookie, checkJs sees "welcome".
        let source = fixture_source(port, true);
        let login = engine.run_login(&source, true).await;
        assert!(login.ok, "login failed: {:?}", login.error);
        assert_eq!(login.extracted["logged_in"], serde_json::json!(true));
        // A fresh engine has no login state: fetching /vip gets 401, the
        // engine logs in once and retries — the cookie jar proves it.
        let db2 = crate::db::Database::open_in_memory().await.unwrap();
        let engine2 = Engine::new(Cache::new(db2));
        let mut vip = fixture_source(port, true);
        vip.search_url = "/vip".to_string();
        let base = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let spec = parse_request_rule(&vip.search_url, &base, "", 1, "").unwrap();
        let mut logs = vec![];
        let t0 = std::time::Instant::now();
        let (status, body) = engine2.fetch_spec(&vip, &spec, CacheKind::Search, true, &mut logs, t0, true).await.unwrap();
        assert_eq!(status, 200);
        assert!(body.contains("会员书"), "body: {body}");
        // Cookie isolation within one engine: a different source id on the
        // same domain must NOT inherit the first source's login cookie.
        let mut other = fixture_source(port, false);
        other.id = "other-source".to_string();
        other.search_url = "/vip".to_string();
        let spec3 = parse_request_rule(&other.search_url, &base, "", 1, "").unwrap();
        let mut logs3 = vec![];
        let r = engine2.fetch_spec(&other, &spec3, CacheKind::Search, true, &mut logs3, std::time::Instant::now(), true).await;
        assert!(r.is_err(), "other source must get 401, got {r:?}");
    }

    #[tokio::test]
    async fn engine_explore() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        let mut source = fixture_source(port, false);
        source.explore_url = format!("http://127.0.0.1:{port}/explore");
        source.rule_explore = serde_json::json!({"bookList": "div.book", "name": "a.title@text", "author": ".author@text", "bookUrl": "a.title@href"});
        let res = engine.run_explore(&source, 1, true).await;
        assert!(res.ok, "explore failed: {:?}", res.error);
        let books: Vec<BookHit> = serde_json::from_value(res.extracted).unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].name, "发现书");
        assert_eq!(books[1].book_url, format!("http://127.0.0.1:{port}/book/8"));
    }

    #[tokio::test]
    async fn engine_login_ui_form() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        let mut source = fixture_source(port, false);
        source.login_url = format!("http://127.0.0.1:{port}/login-form");
        source.login_ui = r#"document.querySelector('input[name="username"]').value = 'alice'; document.querySelector('input[name="password"]').value = 's3cret'; document.querySelector('form').submit();"#.to_string();
        source.login_check_js = "result.includes('login ok') ? 'true' : 'false'".to_string();
        let login = engine.run_login(&source, true).await;
        assert!(login.ok, "loginUi login failed: {:?}", login.error);
        assert_eq!(login.extracted["logged_in"], serde_json::json!(true));
        // The form login's cookie must unlock /vip on a later fetch.
        let base = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let spec = parse_request_rule("/vip", &base, "", 1, "").unwrap();
        let mut logs = vec![];
        let (status, body) = engine.fetch_spec(&source, &spec, CacheKind::Search, true, &mut logs, std::time::Instant::now(), true).await.unwrap();
        assert_eq!(status, 200);
        assert!(body.contains("会员书"), "body: {body}");
    }

    /// Test helper: a cached book with `n` chapters (rows + chapter files).
    async fn make_cached_book(
        db: &crate::db::Database,
        data_dir: &std::path::Path,
        title: &str,
        book_url: &str,
        source_id: &str,
        n: usize,
    ) -> String {
        let book_id = crate::scanner::book_id_for_url(book_url);
        let now = chrono::Utc::now().timestamp();
        db.upsert_book(&crate::db::BookRow {
            id: book_id.clone(), title: title.into(), author: "甲".into(), path: None,
            source_url: Some(book_url.to_string()), source: "cached".into(),
            mtime: now, size: 0, index_version: 1, status: "ok".into(),
            chapter_count: n as i64, created_at: now, updated_at: now,
            source_id: source_id.to_string(), has_update: false, last_update_check: 0, cover: String::new(),
        })
        .await
        .unwrap();
        let titles: Vec<String> = (0..n).map(|i| format!("第{}章", i + 1)).collect();
        db.replace_chapters(&book_id, &titles).await.unwrap();
        let dir = data_dir.join("chapters").join(&book_id);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..n {
            std::fs::write(dir.join(format!("{i}.txt")), "旧正文。").unwrap();
        }
        book_id
    }

    #[tokio::test]
    async fn subscription_refresh_imports_and_tags() {
        use crate::scheduler::Scheduler;
        use crate::search::SearchIndex;
        use std::sync::Arc;
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Arc::new(Engine::new(Cache::new(db.clone())));
        let search = Arc::new(SearchIndex::open_in_ram().unwrap());
        let tmp = std::env::temp_dir().join(format!("shuku-sub-test-{}", uuid::Uuid::new_v4()));
        let sched = Arc::new(Scheduler::new(db.clone(), search, engine, tmp.join("books"), tmp.join("data")));
        let url = format!("http://127.0.0.1:{port}/subscription");
        let msg = crate::scheduler::process_refresh_subscription(&sched, &url).await.unwrap();
        assert!(msg.contains("1 imported, 1 skipped"), "msg: {msg}");
        let id = format!("http://127.0.0.1:{port}/sub");
        assert!(db.get_source_raw(&id).await.unwrap().is_some());
        let (sub_url, _) = db.get_source_subscription(&id).await.unwrap().unwrap();
        assert_eq!(sub_url, url);
        // Second run updates the same source instead of duplicating it.
        let msg2 = crate::scheduler::process_refresh_subscription(&sched, &url).await.unwrap();
        assert!(msg2.contains("1 imported"), "msg: {msg2}");
        assert_eq!(db.list_sources().await.unwrap().len(), 1);
        // Non-JSON feed -> error.
        let bad = format!("http://127.0.0.1:{port}/subscription-bad");
        assert!(crate::scheduler::process_refresh_subscription(&sched, &bad).await.is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn health_check_marks_ok_and_fail() {
        use crate::scheduler::Scheduler;
        use crate::search::SearchIndex;
        use std::sync::Arc;
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        // Fixture source has no checkKeyWord; give it an exploreUrl so the
        // probe falls back to explore (fixture /explore returns 2 books).
        let source = fixture_source(port, false);
        let mut raw = source.raw.clone();
        raw["exploreUrl"] = serde_json::json!("/explore");
        raw["ruleExplore"] = serde_json::json!({"bookList": "div.book", "name": "a.title@text", "author": ".author@text", "bookUrl": "a.title@href"});
        db.upsert_source(&raw).await.unwrap();
        let engine = Arc::new(Engine::new(Cache::new(db.clone())));
        let search = Arc::new(SearchIndex::open_in_ram().unwrap());
        let tmp = std::env::temp_dir().join(format!("shuku-health-test-{}", uuid::Uuid::new_v4()));
        let sched = Arc::new(Scheduler::new(db.clone(), search, engine, tmp.join("books"), tmp.join("data")));
        let msg = crate::scheduler::process_health_check(&sched, &source.id).await.unwrap();
        assert!(msg.contains("healthy"), "msg: {msg}");
        let (health, error, checked) = db.source_health(&source.id).await.unwrap().unwrap();
        assert_eq!(health, "ok");
        assert!(error.is_empty());
        assert!(checked > 0);

        // Dead source (closed port) with a checkKeyWord -> fail + error text.
        let dead_raw = serde_json::json!({
            "bookSourceUrl": "http://127.0.0.1:1",
            "bookSourceName": "dead",
            "searchUrl": "/search",
            "ruleSearch": {"bookList": "div.book", "name": "a.title@text", "author": ".author@text", "bookUrl": "a.title@href", "checkKeyWord": "x"},
        });
        let dead_id = db.upsert_source(&dead_raw).await.unwrap();
        let msg2 = crate::scheduler::process_health_check(&sched, &dead_id).await.unwrap();
        assert!(msg2.contains("unhealthy"), "msg: {msg2}");
        let (health2, error2, _) = db.source_health(&dead_id).await.unwrap().unwrap();
        assert_eq!(health2, "fail");
        assert!(!error2.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn change_source_replaces_book() {
        use crate::scheduler::Scheduler;
        use crate::search::SearchIndex;
        use std::sync::Arc;
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let source = fixture_source(port, false);
        db.upsert_source(&source.raw).await.unwrap();
        let engine = Arc::new(Engine::new(Cache::new(db.clone())));
        let search = Arc::new(SearchIndex::open_in_ram().unwrap());
        let tmp = std::env::temp_dir().join(format!("shuku-change-test-{}", uuid::Uuid::new_v4()));
        let data_dir = tmp.join("data");
        let sched = Arc::new(Scheduler::new(db.clone(), search, engine, tmp.join("books"), data_dir.clone()));
        // Explicit book_url: old book has 5 chapters (one more than the new
        // TOC), so the stale 5th file must be removed.
        let old_url = format!("http://127.0.0.1:{port}/toc");
        let book_id = make_cached_book(&db, &data_dir, "换源书", &old_url, &source.id, 5).await;
        let new_url = format!("http://127.0.0.1:{port}/toc-update");
        let msg = crate::scheduler::process_change_source(&sched, &book_id, &source.id, Some(new_url.clone()))
            .await
            .unwrap();
        assert!(msg.contains("4 chapters"), "msg: {msg}");
        let book = db.get_book(&book_id).await.unwrap().unwrap();
        assert_eq!(book.chapter_count, 4);
        assert_eq!(book.source_url.as_deref(), Some(new_url.as_str()));
        assert!(!book.has_update);
        let dir = data_dir.join("chapters").join(&book_id);
        assert!(!dir.join("4.txt").exists(), "stale 5th chapter file must be removed");
        assert!(std::fs::read_to_string(dir.join("3.txt")).unwrap().contains("正文"));
        let (chs, total) = db.list_chapters(&book_id, 1, 100).await.unwrap();
        assert_eq!(total, 4);
        assert_eq!(chs[3].title, "第四章");

        // By search: a book titled UpdateBook resolves to /toc-update.
        let book_id2 = make_cached_book(&db, &data_dir, "UpdateBook", &old_url, &source.id, 2).await;
        crate::scheduler::process_change_source(&sched, &book_id2, &source.id, None)
            .await
            .unwrap();
        let book2 = db.get_book(&book_id2).await.unwrap().unwrap();
        assert_eq!(book2.chapter_count, 4);
        assert!(book2.source_url.as_deref().unwrap().ends_with("/toc-update"));

        // Unknown source -> error (retryable by the task system).
        assert!(crate::scheduler::process_change_source(&sched, &book_id, "no-such-source", None)
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn update_check_appends_new_chapters() {
        use crate::scheduler::Scheduler;
        use crate::search::SearchIndex;
        use std::sync::Arc;
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let source = fixture_source(port, false);
        db.upsert_source(&source.raw).await.unwrap();
        let engine = Arc::new(Engine::new(Cache::new(db.clone())));
        let search = Arc::new(SearchIndex::open_in_ram().unwrap());
        let tmp = std::env::temp_dir().join(format!("shuku-update-test-{}", uuid::Uuid::new_v4()));
        let data_dir = tmp.join("data");
        std::fs::create_dir_all(data_dir.join("chapters")).unwrap();
        let sched = Arc::new(Scheduler::new(db.clone(), search, engine, tmp.join("books"), data_dir.clone()));
        // A cached book with 2 stored chapters; the fixture TOC has 4.
        let book_id = crate::scanner::book_id_for_url(&format!("http://127.0.0.1:{port}/toc-update"));
        let now = chrono::Utc::now().timestamp();
        db.upsert_book(&crate::db::BookRow {
            id: book_id.clone(), title: "更新书".into(), author: "甲".into(), path: None,
            source_url: Some(format!("http://127.0.0.1:{port}/toc-update")),
            source: "cached".into(), mtime: now, size: 0, index_version: 1,
            status: "ok".into(), chapter_count: 2, created_at: now, updated_at: now,
            source_id: source.id.clone(), has_update: false, last_update_check: 0, cover: String::new(),
        }).await.unwrap();
        db.replace_chapters(&book_id, &["第一章".to_string(), "第二章".to_string()]).await.unwrap();
        let dir = data_dir.join("chapters").join(&book_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0.txt"), "第一章正文。").unwrap();
        std::fs::write(dir.join("1.txt"), "第二章正文。").unwrap();

        let msg = crate::scheduler::process_check_update(&sched, &book_id).await.unwrap();
        assert!(msg.contains("+2 chapters"), "msg: {msg}");
        let book = db.get_book(&book_id).await.unwrap().unwrap();
        assert_eq!(book.chapter_count, 4);
        assert!(book.has_update, "new chapters must flag has_update");
        assert!(dir.join("2.txt").exists() && dir.join("3.txt").exists());
        assert!(std::fs::read_to_string(dir.join("2.txt")).unwrap().contains("正文"));
        let (chs, total) = db.list_chapters(&book_id, 1, 100).await.unwrap();
        assert_eq!(total, 4);
        assert_eq!(chs[3].title, "第四章");

        // Second check: TOC (4) <= stored (4) -> no update, flag untouched.
        db.set_has_update(&book_id, false).await.unwrap();
        let msg2 = crate::scheduler::process_check_update(&sched, &book_id).await.unwrap();
        assert!(msg2.contains("no update"), "msg: {msg2}");
        assert!(!db.get_book(&book_id).await.unwrap().unwrap().has_update);

        // enqueue_due_updates: due once (last_update_check=0), then not again.
        let n = crate::scheduler::enqueue_due_updates(&sched, 3600, 50).await.unwrap();
        assert_eq!(n, 1, "book with last_update_check=0 must be due");
        let n2 = crate::scheduler::enqueue_due_updates(&sched, 3600, 50).await.unwrap();
        assert_eq!(n2, 0, "marking checked must prevent a duplicate enqueue");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn engine_search_book_info_fallback() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        let mut source = fixture_source(port, false);
        // The search URL is a detail page: ruleSearch matches nothing, so
        // ruleBookInfo must salvage one book from the page itself.
        source.search_url = format!("http://127.0.0.1:{port}/bookinfo");
        source.rule_search = serde_json::json!({"bookList": "div.nothing", "name": "a.title@text"});
        source.rule_book_info = serde_json::json!({"name": "h1.title@text", "author": ".author@text"});
        let res = engine.run_search_pages(&source, "x", 1, 1, true).await;
        assert!(res.ok, "search failed: {:?}", res.error);
        let books: Vec<BookHit> = serde_json::from_value(res.extracted).unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].name, "详情书");
        assert_eq!(books[0].author, "庚");
        assert_eq!(books[0].book_url, format!("http://127.0.0.1:{port}/bookinfo"));
    }

    #[tokio::test]
    async fn engine_json_source() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        let base = format!("http://127.0.0.1:{port}");
        let raw = serde_json::json!({
            "bookSourceUrl": base,
            "bookSourceName": "json-fixture",
            "searchUrl": "/api/search?q={{key}}",
            "ruleSearch": {"bookList": "@json:$.data[*]", "name": "@json:$.title", "author": "@json:$.author", "bookUrl": "@json:$.url"},
            "ruleToc": {"chapterList": "@json:$.chapters[*]", "chapterName": "@json:$.name", "chapterUrl": "@json:$.url"},
            "ruleContent": {"content": "@json:$.content"}
        });
        let source = SourceDef::from_raw(&raw);
        let res = engine.run_search_pages(&source, "x", 1, 1, true).await;
        assert!(res.ok, "json search failed: {:?}", res.error);
        let books: Vec<BookHit> = serde_json::from_value(res.extracted).unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].name, "JSON书一");
        let toc = engine.run_toc(&source, &format!("http://127.0.0.1:{port}/api/toc"), true).await;
        assert!(toc.ok, "json toc failed: {:?}", toc.error);
        let chs: Vec<ChapterHit> = serde_json::from_value(toc.extracted).unwrap();
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[1].name, "第二章");
        let content = engine.run_content(&source, &format!("http://127.0.0.1:{port}/api/content"), true).await;
        assert!(content.ok, "json content failed: {:?}", content.error);
        let paras: Vec<Paragraph> = serde_json::from_value(content.extracted).unwrap();
        assert_eq!(paras.len(), 2);
        assert_eq!(paras[0].text.as_deref(), Some("第一段。"));
    }

    #[tokio::test]
    async fn engine_xpath_source() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        let base = format!("http://127.0.0.1:{port}");
        let raw = serde_json::json!({
            "bookSourceUrl": base,
            "bookSourceName": "xpath-fixture",
            "searchUrl": "/search,{\"method\":\"POST\",\"body\":\"q={{key}}&page={{page}}\"}",
            "ruleSearch": {"bookList": "//div[@class='book']", "name": ".//a[@class='title']/text()", "author": ".//span[@class='author']/text()", "bookUrl": ".//a[@class='title']/@href"},
            "ruleToc": {"chapterList": "//a[@class='ch']", "chapterName": "@text", "chapterUrl": "@href", "nextTocUrl": "//a[@class='next']/@href"},
            "ruleContent": {"content": "//div[@id='content']", "nextContentUrl": "//a[@class='next']/@href"}
        });
        let source = SourceDef::from_raw(&raw);
        let res = engine.run_search_pages(&source, "x", 1, 3, true).await;
        assert!(res.ok, "xpath search failed: {:?}", res.error);
        let books: Vec<BookHit> = serde_json::from_value(res.extracted).unwrap();
        assert_eq!(books.len(), 3);
        assert_eq!(books[0].name, "书一");
        let toc = engine.run_toc(&source, &format!("http://127.0.0.1:{port}/toc?page=1"), true).await;
        assert!(toc.ok, "xpath toc failed: {:?}", toc.error);
        let chs: Vec<ChapterHit> = serde_json::from_value(toc.extracted).unwrap();
        assert_eq!(chs.len(), 3);
        let content = engine.run_content(&source, &format!("http://127.0.0.1:{port}/chapter?page=1"), true).await;
        assert!(content.ok, "xpath content failed: {:?}", content.error);
        let paras: Vec<Paragraph> = serde_json::from_value(content.extracted).unwrap();
        let texts: Vec<&str> = paras.iter().filter_map(|p| p.text.as_deref()).collect();
        assert!(texts.iter().any(|t| t.contains("第一页正文")), "texts: {texts:?}");
        assert!(texts.iter().any(|t| t.contains("第二页正文")), "texts: {texts:?}");
    }

    #[tokio::test]
    async fn engine_picker_fetch() {
        let port = spawn_fixture().await;
        let db = crate::db::Database::open_in_memory().await.unwrap();
        let engine = Engine::new(Cache::new(db));
        let tpl = format!("http://127.0.0.1:{port}/search,")
            + r#"{"method":"POST","body":"q={{key}}&page={{page}}"}"#;
        let (status, body) = engine.fetch_picker(&tpl, "x", None).await.unwrap();
        assert_eq!(status, 200);
        assert!(body.contains("书一"), "body: {body}");
        // Bad templates are errors, not empty pages.
        assert!(engine.fetch_picker("not a url", "x", None).await.is_err());
        assert!(engine.fetch_picker("ftp://example.com/x", "x", None).await.is_err());
        assert!(engine.fetch_picker("", "x", None).await.is_err());
    }

    // --- Remaining rules: @js:, js: lists, -/+ order, templates, toc JS ---

    #[test]
    fn js_helpers_md5_base64() {
        assert_eq!(run_js("java.md5Encode(input)", "abc", "", Duration::from_secs(2)).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(run_js("java.base64Encode(input)", "hello 世界", "", Duration::from_secs(2)).unwrap(), "aGVsbG8g5LiW55WM");
        assert_eq!(run_js("java.base64Decode(input)", "aGVsbG8g5LiW55WM", "", Duration::from_secs(2)).unwrap(), "hello 世界");
        assert_eq!(run_js("java.encodeURIComponent(input)", "a b&c", "", Duration::from_secs(2)).unwrap(), "a%20b%26c");
        assert_eq!(run_js("regex_replace(input, '\\\\d+', '#')", "a1b22", "", Duration::from_secs(2)).unwrap(), "a#b#");
        assert_eq!(run_js("strip_ws(input)", "  a   b\n c ", "", Duration::from_secs(2)).unwrap(), "a b c");
        // java.ajax / get / post are intentionally absent (no network from JS).
        assert!(run_js("java.ajax('https://example.com')", "x", "", Duration::from_secs(2)).is_err());
        assert!(run_js("typeof java.ajax", "x", "", Duration::from_secs(2)).unwrap() == "undefined");
    }

    #[test]
    fn js_aliases_and_base_url() {
        let base = "https://example.com/book/1";
        assert_eq!(run_js_ext("input", "abc", "", base, "", Duration::from_secs(2)).unwrap(), "abc");
        assert_eq!(run_js_ext("src", "abc", "", base, "", Duration::from_secs(2)).unwrap(), "abc");
        assert_eq!(run_js_ext("new URL('/x', baseUrl).href", "abc", "", base, "", Duration::from_secs(2)).unwrap(), "https://example.com/x");
    }

    #[test]
    fn at_js_postfix_and_js_wrapper() {
        let rule = serde_json::json!({
            "bookList": "div.book",
            "name": "a.title@text",
            "author": ".author@text@js:input.replace(/^作者/, '')",
            "bookUrl": "<js>result</js>"
        });
        let base = Url::parse("https://example.com/").unwrap();
        let (books, _) = extract_search(SEARCH_HTML, &rule, &base, "").unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].author, "甲");
        // bookUrl as a <js> rule runs on the node text (the whole div text).
        assert!(!books[0].book_url.is_empty());
        // Validation accepts @js: postfixes and <js> wrappers, rejects bad JS in them.
        let raw = serde_json::json!({"bookSourceUrl": "https://example.com", "bookSourceName": "test", "ruleSearch": {"bookList": "div.book", "author": ".author@text@js:input.trim()"}});
        assert!(validate_source(&raw).is_ok());
        let bad = serde_json::json!({"ruleSearch": {"bookList": "div.book", "author": ".author@text@js:function broken( {"}});
        assert!(validate_source(&bad).is_err());
    }

    #[test]
    fn js_list_rule_search_html() {
        let rule = serde_json::json!({
            "bookList": "js:[{name: '书一', author: '甲', bookUrl: '/b/1', kind: '玄幻', lastChapter: '第10章'}, {name: '书二', author: '乙', bookUrl: '/b/2'}]",
            "name": "name",
            "author": "author",
            "bookUrl": "bookUrl",
            "kind": "kind",
            "lastChapter": "lastChapter"
        });
        let base = Url::parse("https://example.com/").unwrap();
        let (books, _) = extract_search("<html><body>ignored</body></html>", &rule, &base, "").unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].name, "书一");
        assert_eq!(books[0].kind, "玄幻");
        assert_eq!(books[0].last_chapter, "第10章");
        assert_eq!(books[0].book_url, "https://example.com/b/1");
    }

    #[test]
    fn list_reverse_and_dedupe() {
        let rule = serde_json::json!({
            "bookList": "-div.book",
            "name": "a.title@text",
            "bookUrl": "a.title@href"
        });
        let base = Url::parse("https://example.com/").unwrap();
        let (books, _) = extract_search(SEARCH_HTML, &rule, &base, "").unwrap();
        assert_eq!(books[0].name, "都市传奇");
        assert_eq!(books[1].name, "修仙传");
        // Same bookUrl twice -> one book.
        let dup = r#"<html><body>
        <div class="book"><a class="title" href="/book/1">书一</a></div>
        <div class="book"><a class="title" href="/book/1">书一重发</a></div></body></html>"#;
        let rule2 = serde_json::json!({"bookList": "div.book", "name": "a.title@text", "bookUrl": "a.title@href"});
        let (books2, _) = extract_search(dup, &rule2, &base, "").unwrap();
        assert_eq!(books2.len(), 1);
        assert_eq!(books2[0].name, "书一");
    }

    #[test]
    fn book_url_template_json() {
        let json: Value = serde_json::json!({"data": [{"id": 7, "title": "书一", "author": "甲"}]});
        let rule = serde_json::json!({
            "bookList": "$.data[*]",
            "name": "title",
            "author": "author",
            "bookUrl": "https://example.com/book/{{id}}"
        });
        let base = Url::parse("https://example.com/").unwrap();
        let (books, _) = super::extract_search_json(&json, &rule, &base, "").unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].book_url, "https://example.com/book/7");
    }

    #[test]
    fn extract_book_info_detail_page() {
        let html = r#"<html><body><h1 class="t">修仙传</h1><span class="a">作者甲</span><p class="i">简介文字</p></body></html>"#;
        let rule = serde_json::json!({"name": "h1.t@text", "author": ".a@text", "intro": ".i@text"});
        let base = Url::parse("https://example.com/book/9").unwrap();
        let b = extract_book_info(html, &rule, &base, "").unwrap().expect("book");
        assert_eq!(b.name, "修仙传");
        assert_eq!(b.author, "作者甲");
        assert_eq!(b.book_url, "https://example.com/book/9");
        // No explicit name rule -> not a detail page, no fallback.
        let no_name = serde_json::json!({"author": ".a@text"});
        assert!(extract_book_info(html, &no_name, &base, "").unwrap().is_none());
    }

    #[test]
    fn pre_update_and_format_js() {
        let html = r#"<html><body><script>var x = 1;</script><ul>
        <li><a href="/c/1">第一章</a></li><li><a href="/c/2">第二章</a></li></ul></body></html>"#;
        let rule = serde_json::json!({
            "preUpdateJs": "js:input.replace(/<script[\\s\\S]*?<\\/script>/gi, '')",
            "chapterList": "li",
            "chapterName": "a@text",
            "chapterUrl": "a@href",
            "formatJs": "`${index}. ${title}`"
        });
        let base = Url::parse("https://example.com/book/1").unwrap();
        let (chs, _, _) = extract_toc_page(html, &rule, &base, "").unwrap();
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[0].name, "1. 第一章");
        assert_eq!(chs[1].name, "2. 第二章");
    }

    #[test]
    fn toc_volume_and_flags() {
        let html = r#"<html><body><ul>
        <li><span class="vol">第一卷</span></li>
        <li><a href="/c/1">第一章</a><span class="vip">VIP</span></li>
        <li><a href="/c/2">第二章</a></li></ul></body></html>"#;
        let rule = serde_json::json!({
            "chapterList": "li",
            "chapterName": "@text",
            "chapterUrl": "a@href",
            "isVolume": ".vol@text",
            "isVip": ".vip@text"
        });
        let base = Url::parse("https://example.com/book/1").unwrap();
        let (chs, _, _) = extract_toc_page(html, &rule, &base, "").unwrap();
        assert_eq!(chs.len(), 3);
        assert_eq!(chs[0].name, "第一卷");
        assert!(chs[0].is_volume);
        assert_eq!(chs[0].url, "volume://1");
        assert!(chs[1].is_vip);
        assert_eq!(chs[1].url, "https://example.com/c/1");
        // Non-volume chapter without a URL falls back to the toc page URL.
        let rule2 = serde_json::json!({"chapterList": "li", "chapterName": "@text", "chapterUrl": "a@href"});
        let (chs2, _, _) = extract_toc_page(html, &rule2, &base, "").unwrap();
        assert_eq!(chs2[0].url, "https://example.com/book/1");
    }

    #[test]
    fn check_keyword_is_plain_string() {
        // checkKeyWord is the debug interface's default keyword, not a
        // selector — a JS-looking value must not fail validation.
        let raw = serde_json::json!({"bookSourceUrl": "https://example.com", "bookSourceName": "test", "ruleSearch": {"checkKeyWord": "key.length > 1", "bookList": "div.book"}});
        assert!(validate_source(&raw).is_ok());
        let raw2 = serde_json::json!({"bookSourceUrl": "https://example.com", "bookSourceName": "test", "ruleSearch": {"checkKeyWord": "修仙", "bookList": "div.book"}});
        assert!(validate_source(&raw2).is_ok());
    }
}
