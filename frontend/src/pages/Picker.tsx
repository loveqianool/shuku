import { createEffect, createSignal, For, onCleanup, Show, type Component } from "solid-js";
import { api, type Source } from "../lib/api";
import { PICKER_SCRIPT } from "../lib/pickerScript";

type Mode = "search" | "toc" | "content";
type Format = "css" | "xpath";

const FIELDS: Record<Mode, { key: string; label: string; list?: boolean }[]> = {
  search: [
    { key: "bookList", label: "列表 bookList", list: true },
    { key: "name", label: "书名 name" },
    { key: "author", label: "作者 author" },
    { key: "bookUrl", label: "链接 bookUrl" },
    { key: "coverUrl", label: "封面 coverUrl" },
    { key: "intro", label: "简介 intro" },
  ],
  toc: [
    { key: "chapterList", label: "列表 chapterList", list: true },
    { key: "chapterName", label: "章节名 chapterName" },
    { key: "chapterUrl", label: "章节链接 chapterUrl" },
    { key: "nextTocUrl", label: "下一页 nextTocUrl" },
  ],
  content: [
    { key: "content", label: "正文 content" },
    { key: "nextContentUrl", label: "下一页 nextContentUrl" },
  ],
};

const RULE_KEY: Record<Mode, "ruleSearch" | "ruleToc" | "ruleContent"> = {
  search: "ruleSearch",
  toc: "ruleToc",
  content: "ruleContent",
};

function asRuleObject(v: unknown): Record<string, string> {
  if (v && typeof v === "object" && !Array.isArray(v)) {
    const out: Record<string, string> = {};
    for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
      if (typeof val === "string") out[k] = val;
    }
    return out;
  }
  if (typeof v === "string" && v) return { content: v };
  return {};
}

function originOf(template: string): string {
  try {
    const probe = template.replace(/\{\{?key\}?\}|\{\{?page\}?\}/g, "x").split(",{")[0];
    return new URL(probe).origin + "/";
  } catch {
    return "/";
  }
}

function injectPicker(html: string, baseUrl: string): string {
  // Server-rendered snapshot: drop the page's own scripts so nothing
  // navigates or re-renders under the picker, then add <base> (relative
  // resources) and the picker script.
  let out = html.replace(/<script\b[^>]*>[\s\S]*?<\/script\s*>/gi, "");
  if (/<head[^>]*>/i.test(out)) {
    out = out.replace(/<head[^>]*>/i, (m) => `${m}<base href="${baseUrl}">`);
  } else {
    out = `<base href="${baseUrl}">` + out;
  }
  const tag = `<script>${PICKER_SCRIPT}</script>`;
  if (/<\/body\s*>/i.test(out)) out = out.replace(/<\/body\s*>/i, `${tag}</body>`);
  else out += tag;
  return out;
}

function looksLikeJson(body: string): boolean {
  const t = body.trimStart();
  return t.startsWith("{") || t.startsWith("[");
}

// Minimal JSONPath subset (mirrors the backend): $.a.b, ['a'], [n], [-n], [*].
function evalJsonPath(root: unknown, path: string): unknown[] {
  let p = path.trim();
  if (p.startsWith("@json:")) p = p.slice(6);
  if (p.startsWith("$")) p = p.slice(1);
  const segs: (string | number | "*")[] = [];
  if (/^[A-Za-z0-9_-]+$/.test(p)) {
    segs.push(p);
  } else {
    const re = /(?:\.([A-Za-z0-9_-]+)|\[\s*(?:'([^']*)'|"([^"]*)"|(-?\d+)|\*)\s*\])/g;
    let m: RegExpExecArray | null;
    while ((m = re.exec(p)) !== null) {
      if (m[1] !== undefined) segs.push(m[1]);
      else if (m[2] !== undefined) segs.push(m[2]);
      else if (m[3] !== undefined) segs.push(m[3]);
      else if (m[4] !== undefined) segs.push(Number(m[4]));
      else segs.push("*");
    }
  }
  let cur: unknown[] = [root];
  for (const s of segs) {
    const next: unknown[] = [];
    for (const v of cur) {
      if (s === "*") {
        if (Array.isArray(v)) next.push(...v);
        else if (v && typeof v === "object") next.push(...Object.values(v as Record<string, unknown>));
      } else if (typeof s === "number") {
        if (Array.isArray(v)) {
          const i = s < 0 ? v.length + s : s;
          if (v[i] !== undefined) next.push(v[i]);
        }
      } else if (v && typeof v === "object" && !Array.isArray(v)) {
        const val = (v as Record<string, unknown>)[s];
        if (val !== undefined) next.push(val);
      }
    }
    cur = next;
  }
  return cur;
}

function jsonToString(v: unknown): string {
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  if (v == null) return "";
  return JSON.stringify(v);
}

function pathToString(segs: (string | number)[]): string {
  let out = "$";
  for (const s of segs) {
    if (typeof s === "number") out += `[${s}]`;
    else if (/^[A-Za-z0-9_-]+$/.test(s)) out += `.${s}`;
    else out += `['${s}']`;
  }
  return out;
}

const JsonNode: Component<{
  label: string;
  value: unknown;
  segs: (string | number)[];
  depth: number;
  onPick: (segs: (string | number)[], value: unknown) => void;
}> = (props) => {
  const [open, setOpen] = createSignal(props.depth < 2);
  const isObj = props.value !== null && typeof props.value === "object";
  const entries: [string, unknown][] = isObj
    ? Array.isArray(props.value)
      ? (props.value as unknown[]).map((v, i) => [String(i), v])
      : Object.entries(props.value as Record<string, unknown>)
    : [];
  const summary = Array.isArray(props.value)
    ? `Array(${props.value.length})`
    : isObj
      ? `Object(${entries.length})`
      : jsonToString(props.value).slice(0, 60);
  return (
    <div class="json-node" style={{ "margin-left": `${props.depth * 14}px` }}>
      <div class="json-row" onClick={() => props.onPick(props.segs, props.value)}>
        <Show when={isObj}>
          <button
            class="json-toggle"
            onClick={(e) => {
              e.stopPropagation();
              setOpen(!open());
            }}
          >
            {open() ? "▾" : "▸"}
          </button>
        </Show>
        <span class="json-key">{props.label}</span>
        <span class="muted">{isObj ? summary : summary}</span>
      </div>
      <Show when={isObj && open()}>
        <For each={entries}>
          {([k, v]) => (
            <JsonNode
              label={Array.isArray(props.value) ? `[${k}]` : k}
              value={v}
              segs={[...props.segs, Array.isArray(props.value) ? Number(k) : k]}
              depth={props.depth + 1}
              onPick={props.onPick}
            />
          )}
        </For>
      </Show>
    </div>
  );
};

const Picker: Component<{ sourceId?: string }> = (props) => {
  const [source, setSource] = createSignal<Source | null>(null);
  const [url, setUrl] = createSignal("");
  const [keyword, setKeyword] = createSignal("");
  const [mode, setMode] = createSignal<Mode>("search");
  const [format, setFormat] = createSignal<Format>("css");
  const [rules, setRules] = createSignal<Record<string, Record<string, string>>>({
    ruleSearch: {},
    ruleToc: {},
    ruleContent: {},
  });
  const [armed, setArmed] = createSignal<{ mode: Mode; field: string } | null>(null);
  const [html, setHtml] = createSignal("");
  const [jsonData, setJsonData] = createSignal<unknown>(null);
  const [fetchInfo, setFetchInfo] = createSignal("");
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal("");
  const [msg, setMsg] = createSignal("");
  const [previewRows, setPreviewRows] = createSignal<Record<string, string>[]>([]);
  const [previewError, setPreviewError] = createSignal("");
  let iframeRef: HTMLIFrameElement | undefined;

  if (props.sourceId) {
    api
      .getSource(props.sourceId)
      .then((s) => {
        setSource(s);
        setUrl(s.searchUrl ?? "");
        setRules({
          ruleSearch: asRuleObject(s.ruleSearch),
          ruleToc: asRuleObject(s.ruleToc),
          ruleContent: asRuleObject(s.ruleContent),
        });
      })
      .catch((e) => setError(String(e)));
  }

  const postCmd = (cmd: Record<string, unknown>) => {
    iframeRef?.contentWindow?.postMessage({ type: "legado-picker-cmd", ...cmd }, "*");
  };

  createEffect(() => {
    const a = armed();
    if (a) postCmd({ cmd: "arm", armed: a, format: format() });
    else postCmd({ cmd: "disarm" });
  });

  const onMessage = (e: MessageEvent) => {
    const d = e.data as Record<string, unknown> | null;
    if (!d || d.type !== "legado-picker") return;
    if (iframeRef && e.source !== iframeRef.contentWindow) return;
    if (d.kind === "ready") {
      const a = armed();
      if (a) postCmd({ cmd: "arm", armed: a, format: format() });
      return;
    }
    if (d.kind === "pick") {
      const m = d.mode as Mode;
      const field = d.field as string;
      const rule = d.rule as string;
      setRules((r) => ({ ...r, [RULE_KEY[m]]: { ...r[RULE_KEY[m]], [field]: rule } }));
      setMsg(
        `${field} = ${rule}（${(d.value as string) || ""}${typeof d.count === "number" ? `，命中 ${d.count} 个` : ""}${
          d.scoped === false ? "，注意：不在列表项内，是绝对路径" : ""
        }）`,
      );
      setArmed(null);
      return;
    }
    if (d.kind === "preview") {
      if (d.error) setPreviewError(String(d.error));
      else {
        setPreviewError("");
        setPreviewRows((d.rows as Record<string, string>[]) ?? []);
      }
      return;
    }
  };
  window.addEventListener("message", onMessage);
  // eslint-disable-next-line solid/reactivity
  onCleanup(() => window.removeEventListener("message", onMessage));

  const fetchPage = async () => {
    const u = url().trim();
    if (!u) {
      setError("先填页面地址（搜索页可带 {key} / {{key}} 模板）");
      return;
    }
    setLoading(true);
    setError("");
    setMsg("");
    setPreviewRows([]);
    setArmed(null);
    try {
      const r = await api.pickerFetch({ url: u, keyword: keyword() || undefined });
      setFetchInfo(`HTTP ${r.status} · ${r.elapsed_ms} ms · ${(r.body.length / 1024).toFixed(0)} KB`);
      if (looksLikeJson(r.body)) {
        setJsonData(JSON.parse(r.body));
        setHtml("");
      } else {
        setJsonData(null);
        setHtml(injectPicker(r.body, originOf(u)));
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  const setRule = (field: string, value: string) => {
    const m = mode();
    setRules((r) => ({ ...r, [RULE_KEY[m]]: { ...r[RULE_KEY[m]], [field]: value } }));
  };

  const armField = (field: string) => {
    const a = armed();
    if (a && a.mode === mode() && a.field === field) setArmed(null);
    else setArmed({ mode: mode(), field });
  };

  const runPreview = () => {
    setPreviewError("");
    const m = mode();
    const rs = rules()[RULE_KEY[m]];
    if (jsonData() !== null) {
      const listKey = m === "search" ? "bookList" : m === "toc" ? "chapterList" : "";
      if (m === "content") {
        const vals = evalJsonPath(jsonData(), rs.content || "");
        setPreviewRows([{ text: vals.map(jsonToString).join("\n").slice(0, 300) }]);
        return;
      }
      const items = evalJsonPath(jsonData(), rs[listKey] || "").slice(0, 3);
      setPreviewRows(
        items.map((item) => {
          const row: Record<string, string> = {};
          for (const [k, rule] of Object.entries(rs)) {
            if (k === listKey) continue;
            row[k] = jsonToString(evalJsonPath(item, rule)[0] ?? "");
          }
          return row;
        }),
      );
      return;
    }
    postCmd({ cmd: "preview", rules: rs, mode: m });
  };

  const onJsonPick = (segs: (string | number)[], value: unknown) => {
    const a = armed();
    if (!a) {
      setMsg("先点右侧字段旁的「选」，再点 JSON 节点");
      return;
    }
    const m = a.mode;
    const rk = RULE_KEY[m];
    const listKey = m === "search" ? "bookList" : m === "toc" ? "chapterList" : "";
    const isList = a.field === listKey;
    if (isList) {
      if (!Array.isArray(value)) {
        setMsg("列表字段要点一个数组节点");
        return;
      }
      setRule(a.field, pathToString(segs) + "[*]");
    } else {
      // Inside a picked list item, paths are relative to the item.
      const listRule = rules()[rk][listKey] ?? "";
      const prefix = listRule.replace(/^\@json:/, "").replace(/\[\*\]$/, "");
      const abs = pathToString(segs);
      if (prefix && abs.startsWith(prefix)) {
        const rest = abs.slice(prefix.length).replace(/^\[\d+\]/, "");
        if (rest) setRule(a.field, "@json:$" + rest);
        else setRule(a.field, "@json:" + abs);
      } else {
        setRule(a.field, "@json:" + abs);
      }
    }
    setMsg(`${a.field} 已填入`);
    setArmed(null);
  };

  const copyRules = async () => {
    try {
      await navigator.clipboard.writeText(JSON.stringify(rules(), null, 2));
      setMsg("规则 JSON 已复制");
    } catch {
      setMsg("复制失败，请手动选中下方 JSON");
    }
  };

  const saveToSource = async () => {
    setError("");
    try {
      const r = rules();
      if (source()) {
        const s = source()!;
        await api.updateSource(s.id!, {
          ...s,
          ruleSearch: r.ruleSearch,
          ruleToc: r.ruleToc,
          ruleContent: r.ruleContent,
        });
        setMsg("已保存到书源，去测试面板验证一下");
      } else {
        // createSource returns only {id}; keep the payload we sent.
        const payload: Source = {
          bookSourceUrl: originOf(url()),
          bookSourceName: `拾取-${new URL(originOf(url())).hostname}`,
          searchUrl: url(),
          ruleSearch: r.ruleSearch,
          ruleToc: r.ruleToc,
          ruleContent: r.ruleContent,
        };
        const created = (await api.createSource(payload)) as unknown as { id: string };
        setSource({ ...payload, id: created.id });
        setMsg("已新建书源，去测试面板验证一下");
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const modeRules = () => rules()[RULE_KEY[mode()]];

  return (
    <div>
      <a href="#/sources">‹ 返回书源列表</a>
      <h2>规则拾取{source() ? ` · ${source()!.bookSourceName}` : ""}</h2>
      <p class="muted">
        像 uBlock 元素选择器一样：点右侧字段旁的「选」，再点页面里的元素，规则自动生成。列表字段先点一个条目，之后的名称、作者、链接都在该条目内相对取。
      </p>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <Show when={msg()}>
        <p class="ok">{msg()}</p>
      </Show>

      <div class="toolbar">
        <select value={mode()} onChange={(e) => { setMode(e.currentTarget.value as Mode); setArmed(null); }}>
          <option value="search">搜索页 search</option>
          <option value="toc">目录页 toc</option>
          <option value="content">正文页 content</option>
        </select>
        <select value={format()} onChange={(e) => setFormat(e.currentTarget.value as Format)}>
          <option value="css">生成 CSS</option>
          <option value="xpath">生成 XPath</option>
        </select>
        <input
          placeholder={mode() === "search" ? "搜索地址，可带 {key} 模板，如 /s?q={key}" : "页面地址（目录/正文页的完整 URL）"}
          value={url()}
          onInput={(e) => setUrl(e.currentTarget.value)}
          style={{ flex: "1", "min-width": "16em" }}
        />
        <Show when={mode() === "search"}>
          <input placeholder="keyword（测试词）" value={keyword()} onInput={(e) => setKeyword(e.currentTarget.value)} style={{ width: "8em" }} />
        </Show>
        <button class="primary" onClick={fetchPage} disabled={loading()}>
          {loading() ? "抓取中…" : "抓取页面"}
        </button>
      </div>
      <Show when={fetchInfo()}>
        <p class="muted">{fetchInfo()}</p>
      </Show>

      <div class="picker-grid">
        <div class="picker-preview">
          <Show when={html()}>
            <iframe ref={iframeRef} srcdoc={html()} title="picker" class="picker-frame" />
          </Show>
          <Show when={jsonData() !== null}>
            <div class="card json-tree">
              <p class="muted">JSON 响应：点节点生成 @json 规则；点数组节点生成列表规则。</p>
              <JsonNode label="$" value={jsonData()} segs={[]} depth={0} onPick={onJsonPick} />
            </div>
          </Show>
          <Show when={!html() && jsonData() === null}>
            <p class="muted">先抓取一个页面。服务端代抓（走书源同一套请求、解码、限流），页面以快照形式在 iframe 里打开，脚本已剥离，点元素不会跳转。</p>
          </Show>
        </div>

        <div class="picker-panel">
          <h3>
            {mode() === "search" ? "搜索规则" : mode() === "toc" ? "目录规则" : "正文规则"}
            <span class="muted">（{format() === "css" ? "CSS" : "XPath"}）</span>
          </h3>
          <For each={FIELDS[mode()]}>
            {(f) => (
              <div class={`field-row${armed()?.mode === mode() && armed()?.field === f.key ? " armed" : ""}`}>
                <label>{f.label}</label>
                <input value={modeRules()[f.key] ?? ""} onInput={(e) => setRule(f.key, e.currentTarget.value)} placeholder={f.list ? "先点「选」再点页面里一个条目" : "点「选」再点页面里的元素"} />
                <button onClick={() => armField(f.key)}>{armed()?.field === f.key ? "取消" : "选"}</button>
              </div>
            )}
          </For>
          <div class="toolbar">
            <button onClick={runPreview} disabled={!html() && jsonData() === null}>
              预览提取
            </button>
            <button onClick={copyRules}>复制规则 JSON</button>
            <button class="primary" onClick={saveToSource}>
              {source() ? "保存到书源" : "保存为新书源"}
            </button>
            <Show when={source()}>
              <a href={`#/sources/${encodeURIComponent(source()!.id!)}/test`}>
                <button>去测试</button>
              </a>
            </Show>
          </div>
          <Show when={previewError()}>
            <p class="error">{previewError()}</p>
          </Show>
          <Show when={previewRows().length}>
            <h4>预览（前 3 条，页面内即时算出；以服务端测试面板为准）</h4>
            <For each={previewRows()}>
              {(row) => (
                <pre class="preview-row">
                  {Object.entries(row)
                    .map(([k, v]) => `${k}: ${v}`)
                    .join("\n")}
                </pre>
              )}
            </For>
          </Show>
          <h4>当前规则 JSON</h4>
          <pre class="rules-json">{JSON.stringify(rules(), null, 2)}</pre>
        </div>
      </div>
    </div>
  );
};

export default Picker;
