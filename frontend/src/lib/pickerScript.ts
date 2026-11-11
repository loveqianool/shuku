// Injected into the fetched page inside the picker iframe (srcdoc).
// Runs in the page: hover-highlight, click-to-pick, selector generation
// (CSS / XPath), list scoping, client-side preview evaluation. Talks to
// the parent via postMessage. Plain JS inside a template literal: no
// backticks, no ${...}, no literal </script> anywhere in the script.
export const PICKER_SCRIPT = `(function(){
  var FORMAT = 'css';
  var armed = null;
  var listCtx = null;
  var hlEls = [];
  function post(msg){ msg.type = 'legado-picker'; try { parent.postMessage(msg, '*'); } catch(e){} }
  function clearHl(){ for (var i=0;i<hlEls.length;i++){ hlEls[i].classList.remove('legado-pick-hover','legado-pick-sel','legado-pick-item'); } hlEls = []; }
  function mark(el, cls){ if(!el || !el.classList) return; el.classList.add(cls); hlEls.push(el); }
  function qsa(sel, root){ try { return Array.prototype.slice.call((root||document).querySelectorAll(sel)); } catch(e){ return []; } }
  function qsaCount(sel){ return qsa(sel, document).length; }
  function xpathNodes(expr, ctx){
    var out = [];
    try {
      var res = document.evaluate(expr, ctx||document, null, XPathResult.ORDERED_NODE_ITERATOR_TYPE, null);
      var n; while ((n = res.iterateNext())) out.push(n);
    } catch(e){}
    return out;
  }
  function clsOf(el){
    if (!el.className || typeof el.className !== 'string') return [];
    return el.className.trim().split(/\\s+/).filter(function(c){ return c && c.indexOf('legado-pick') !== 0; });
  }
  function seg(el){
    var s = el.tagName.toLowerCase();
    var cls = clsOf(el);
    if (cls.length) s += '.' + cls.join('.');
    return s;
  }
  function escId(id){ return (window.CSS && CSS.escape) ? CSS.escape(id) : id.replace(/([^a-zA-Z0-9_-])/g, '\\\\$1'); }
  function cssSelector(el){
    if (el.id && qsaCount('#' + escId(el.id)) === 1) return '#' + escId(el.id);
    var parts = [];
    var cur = el;
    while (cur && cur.nodeType === 1 && cur !== document.documentElement) {
      var s = seg(cur);
      parts.unshift(s);
      var cand = parts.join(' > ');
      if (qsaCount(cand) === 1) return cand;
      var parent = cur.parentElement;
      if (parent) {
        var same = 0, idx = 0;
        var kids = parent.children;
        for (var i=0;i<kids.length;i++){
          if (kids[i].tagName === cur.tagName) { same++; if (kids[i] === cur) idx = same; }
        }
        if (same > 1) {
          parts[0] = s + ':nth-child(' + idx + ')';
          cand = parts.join(' > ');
          if (qsaCount(cand) === 1) return cand;
          parts[0] = s;
        }
      }
      cur = parent;
    }
    return parts.join(' > ');
  }
  function xpathFor(el){
    if (el.id) return "//*[@id='" + el.id + "']";
    var parts = [];
    var cur = el;
    while (cur && cur.nodeType === 1) {
      var idx = 1, sib = cur.previousElementSibling;
      while (sib) { if (sib.tagName === cur.tagName) idx++; sib = sib.previousElementSibling; }
      parts.unshift(cur.tagName.toLowerCase() + '[' + idx + ']');
      cur = cur.parentElement;
    }
    return '/' + parts.join('/');
  }
  function xpathList(el){
    var cls = clsOf(el);
    var tag = el.tagName.toLowerCase();
    if (cls.length) return '//' + tag + "[contains(concat(' ', normalize-space(@class), ' '), ' " + cls[0] + " ')]";
    return '//' + tag;
  }
  function listSelector(el){
    var parent = el.parentElement;
    var s = seg(el);
    if (parent) {
      var cand = cssSelector(parent) + ' > ' + s;
      var n = qsaCount(cand);
      if (n >= 1) return { css: cand, xpath: xpathList(el), count: n };
    }
    return { css: cssSelector(el), xpath: xpathFor(el), count: 1 };
  }
  function relSelector(from, to){
    var parts = [];
    var cur = to;
    while (cur && cur !== from) { parts.unshift(seg(cur)); cur = cur.parentElement; }
    if (!cur) return cssSelector(to);
    return parts.join(' > ');
  }
  function relXpath(from, to){
    var parts = [];
    var cur = to;
    while (cur && cur !== from) {
      var idx = 1, sib = cur.previousElementSibling;
      while (sib) { if (sib.tagName === cur.tagName) idx++; sib = sib.previousElementSibling; }
      parts.unshift(cur.tagName.toLowerCase() + '[' + idx + ']');
      cur = cur.parentElement;
    }
    if (!cur) return xpathFor(to);
    return './/' + parts.join('/');
  }
  function suffixOf(field){
    if (field === 'name' || field === 'author' || field === 'chapterName' || field === 'intro') return '@text';
    if (field === 'bookUrl' || field === 'chapterUrl' || field === 'nextTocUrl' || field === 'nextContentUrl') return '@href';
    return '';
  }
  function textOf(el){ return (el.textContent || '').trim().replace(/\\s+/g, ' '); }
  function hrefOf(el){
    var a = el.tagName === 'A' ? el : (el.closest ? el.closest('a') : null);
    return a ? (a.getAttribute('href') || '') : '';
  }
  document.addEventListener('mouseover', function(e){
    if (!armed) return;
    var t = e.target;
    if (!t || t === document.documentElement || t === document.body) return;
    clearHl(); mark(t, 'legado-pick-hover');
  }, true);
  document.addEventListener('click', function(e){
    if (!armed) return;
    e.preventDefault(); e.stopPropagation();
    var t = e.target;
    if (!t || t === document.documentElement || t === document.body) return;
    clearHl();
    var field = armed.field, mode = armed.mode;
    var isList = field === 'bookList' || field === 'chapterList';
    if (isList) {
      var ls = listSelector(t);
      var rule = FORMAT === 'xpath' ? ls.xpath : ls.css;
      listCtx = { css: ls.css, xpath: ls.xpath };
      var items = qsa(ls.css, document);
      for (var i=0;i<items.length;i++) mark(items[i], 'legado-pick-item');
      post({ kind:'pick', mode:mode, field:field, rule:rule, count:ls.count, value:textOf(t).slice(0,80) });
      return;
    }
    var scope = null;
    if (listCtx) {
      var all = FORMAT === 'xpath' ? xpathNodes(listCtx.xpath, document) : qsa(listCtx.css, document);
      if (all.length) scope = all[0];
    }
    var inside = !!(scope && scope.contains(t));
    var suf = suffixOf(field);
    var base, value;
    if (suf === '@href') {
      var link = t.tagName === 'A' ? t : (t.closest ? t.closest('a') : null);
      var target = link || t;
      base = inside ? relSelector(scope, target) : cssSelector(target);
      if (FORMAT === 'xpath') base = inside ? relXpath(scope, target) : xpathFor(target);
      value = hrefOf(target);
    } else if (field === 'content') {
      base = inside ? relSelector(scope, t) : cssSelector(t);
      if (FORMAT === 'xpath') base = inside ? relXpath(scope, t) : xpathFor(t);
      value = textOf(t).slice(0, 120);
    } else {
      base = inside ? relSelector(scope, t) : cssSelector(t);
      if (FORMAT === 'xpath') base = inside ? relXpath(scope, t) : xpathFor(t);
      value = textOf(t).slice(0, 80);
    }
    mark(t, 'legado-pick-sel');
    post({ kind:'pick', mode:mode, field:field, rule: base + suf, value:value, scoped: inside });
  }, true);
  function evalRule(rule, ctx){
    if (!rule) return [];
    var isXpath = rule.indexOf('@xpath:') === 0 || rule.charAt(0) === '/' || rule.indexOf('./') === 0;
    if (isXpath) {
      var expr = rule.replace(/^@xpath:/, '');
      return xpathNodes(expr, ctx);
    }
    var at = rule.lastIndexOf('@');
    var sel = at > 0 ? rule.slice(0, at) : rule;
    var attr = at > 0 ? rule.slice(at + 1) : null;
    var parts = sel.split('&&').map(function(s){ return s.trim(); }).filter(Boolean);
    var cur = [ctx];
    parts.forEach(function(p){
      var next = [];
      cur.forEach(function(c){ next = next.concat(qsa(p, c)); });
      cur = next;
    });
    if (!attr) return cur;
    return cur.map(function(n){
      if (attr === 'text') return { _text: textOf(n) };
      if (attr === 'html') return { _text: n.innerHTML };
      return { _text: n.getAttribute(attr) || '' };
    });
  }
  function fieldVal(item, rule){
    var vals = evalRule(rule, item);
    if (!vals.length) return '';
    var v = vals[0];
    if (v && v._text !== undefined) return v._text;
    if (v && (v.nodeType === 2 || v.nodeType === 3)) return v.nodeValue || '';
    if (v && v.textContent !== undefined) return textOf(v);
    return '';
  }
  function doPreview(rules, mode){
    try {
      if (mode === 'content') {
        var cvals = evalRule(rules.content || '', document);
        var text = '';
        if (cvals.length) text = cvals[0]._text !== undefined ? cvals[0]._text : textOf(cvals[0]);
        post({ kind:'preview', mode:mode, rows:[{ text: String(text).slice(0, 300) }] });
        return;
      }
      var listKey = mode === 'search' ? 'bookList' : 'chapterList';
      var items = evalRule(rules[listKey] || '', document).slice(0, 3);
      var rows = items.map(function(item){
        var row = {};
        Object.keys(rules).forEach(function(k){
          if (k === listKey) return;
          row[k] = fieldVal(item, rules[k]);
        });
        return row;
      });
      post({ kind:'preview', mode:mode, rows:rows });
    } catch(e){
      post({ kind:'preview', mode:mode, error: String(e) });
    }
  }
  window.addEventListener('message', function(e){
    var d = e.data || {};
    if (d.type !== 'legado-picker-cmd') return;
    if (d.cmd === 'arm') { armed = d.armed; if (d.format) FORMAT = d.format; }
    if (d.cmd === 'disarm') { armed = null; clearHl(); }
    if (d.cmd === 'clear') { armed = null; listCtx = null; clearHl(); }
    if (d.cmd === 'preview') { doPreview(d.rules || {}, d.mode); }
  });
  var st = document.createElement('style');
  st.textContent = '.legado-pick-hover{outline:2px solid #f59e0b !important;outline-offset:1px;cursor:pointer} .legado-pick-sel{outline:2px solid #22c55e !important} .legado-pick-item{outline:2px dashed #3b82f6 !important}';
  document.head.appendChild(st);
  post({ kind:'ready' });
})();`;
