import { createSignal, For, Show, type Component } from "solid-js";
import { api, type Source } from "../lib/api";
import JsonEditor from "../components/JsonEditor";

const EMPTY_SOURCE: Source = {
  bookSourceUrl: "",
  bookSourceName: "新书源",
  bookSourceGroup: "",
  bookSourceType: 0,
  searchUrl: "",
  enabled: true,
  weight: 0,
};

const Sources: Component = () => {
  const [sources, setSources] = createSignal<Source[]>([]);
  const [error, setError] = createSignal("");
  const [ok, setOk] = createSignal("");
  const [editing, setEditing] = createSignal<Source | null>(null);
  const [jsonText, setJsonText] = createSignal("");
  const [jsonError, setJsonError] = createSignal("");
  const [subUrl, setSubUrl] = createSignal("");

  const refresh = async () => {
    try {
      setSources(await api.listSources());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  refresh();

  const startEdit = (s: Source) => {
    setEditing(s);
    setJsonText(JSON.stringify(s, null, 2));
    setJsonError("");
  };

  const save = async () => {
    let parsed: Source;
    try {
      parsed = JSON.parse(jsonText()) as Source;
    } catch (e) {
      setJsonError("JSON 解析失败：" + (e instanceof Error ? e.message : String(e)));
      return;
    }
    if (!parsed.bookSourceUrl || !parsed.bookSourceName) {
      setJsonError("bookSourceUrl 与 bookSourceName 必填");
      return;
    }
    try {
      const cur = editing();
      if (cur?.id) await api.updateSource(cur.id, parsed);
      else await api.createSource(parsed);
      setOk("已保存");
      setEditing(null);
      await refresh();
    } catch (e) {
      setJsonError(e instanceof Error ? e.message : String(e));
    }
  };

  const remove = async (s: Source) => {
    if (!s.id || !confirm(`删除书源「${s.bookSourceName}」？`)) return;
    try {
      await api.deleteSource(s.id);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const doExport = async () => {
    try {
      const data = await api.exportSources();
      const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
      const a = document.createElement("a");
      a.href = URL.createObjectURL(blob);
      a.download = "legado-sources.json";
      a.click();
      URL.revokeObjectURL(a.href);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const doImport = async (file: File) => {
    try {
      const data = JSON.parse(await file.text()) as Source[] | { sources: Source[] };
      const list = Array.isArray(data) ? data : data.sources;
      if (!Array.isArray(list)) throw new Error("文件格式不对：应为书源数组或 {sources:[...]}");
      await api.importSources(list);
      setOk(`已导入 ${list.length} 个书源`);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div>
      <h2>书源管理</h2>
      <p class="muted">阅读3 JSON 格式。规则合法性（selector / regex）在后端保存时校验。</p>
      <div class="toolbar">
        <button class="primary" onClick={() => startEdit({ ...EMPTY_SOURCE })}>新建书源</button>
        <button onClick={doExport}>导出全部</button>
        <label>
          导入
          <input
            type="file"
            accept="application/json,.json"
            style={{ display: "none" }}
            onChange={(e) => { const f = e.currentTarget.files?.[0]; if (f) doImport(f); }}
          />
        </label>
        <input
          placeholder="订阅 URL（书源 JSON 数组或单个书源）"
          value={subUrl()}
          onInput={(e) => setSubUrl(e.currentTarget.value)}
          style={{ "min-width": "280px" }}
        />
        <button
          onClick={async () => {
            const url = subUrl().trim();
            if (!url) return;
            try {
              await api.subscribe(url);
              setOk("已排队从订阅导入书源（后台进行，稍后刷新查看）");
              setSubUrl("");
            } catch (e) {
              setError(e instanceof Error ? e.message : String(e));
            }
          }}
        >
          订阅
        </button>
      </div>
      <Show when={error()}><p class="error">{error()}</p></Show>
      <Show when={ok()}><p class="ok">{ok()}</p></Show>

      <For each={sources()}>
        {(s) => (
          <div class="card">
            <h3>{s.bookSourceName} <span class="muted">{s.bookSourceGroup ?? ""}</span></h3>
            <p class="muted">{s.bookSourceUrl}</p>
            <div class="row">
              <span class="badge">{s.enabled === false ? "已禁用" : "启用中"}</span>
              <span class="badge">权重 {s.weight ?? 0}</span>
              <Show when={s.health === "ok"}><span class="badge" style={{ "background": "#166534", "color": "#fff" }}>正常</span></Show>
              <Show when={s.health === "fail"}>
                <span class="badge" style={{ "background": "#991b1b", "color": "#fff" }} title={s.healthError ?? ""}>失效</span>
              </Show>
              <Show when={!s.health || s.health === "unknown"}><span class="badge">未检查</span></Show>
              <Show when={s.subscriptionUrl}><span class="badge" title={s.subscriptionUrl as string}>订阅</span></Show>
              <button onClick={() => startEdit(s)}>编辑</button>
              <Show when={s.id}>
                <a href={`#/sources/${encodeURIComponent(s.id!)}/test`}><button>测试</button></a>
                <a href={`#/sources/${encodeURIComponent(s.id!)}/pick`}><button>拾取</button></a>
                <button
                  onClick={async () => {
                    try {
                      await api.healthCheck(s.id!);
                      setOk(`已排队检查「${s.bookSourceName}」（后台进行，稍后刷新查看）`);
                    } catch (e) {
                      setError(e instanceof Error ? e.message : String(e));
                    }
                  }}
                >
                  检查
                </button>
              </Show>
              <button class="danger" onClick={() => remove(s)}>删除</button>
            </div>
          </div>
        )}
      </For>
      <Show when={sources().length === 0}><p class="muted">暂无书源，可新建或导入阅读3 JSON。</p></Show>

      <Show when={editing()}>
        <div class="card">
          <h3>{editing()!.id ? "编辑书源" : "新建书源"}</h3>
          <JsonEditor value={jsonText()} onChange={setJsonText} />
          <Show when={jsonError()}><p class="error">{jsonError()}</p></Show>
          <div class="toolbar">
            <button class="primary" onClick={save}>保存</button>
            <button onClick={() => setEditing(null)}>取消</button>
          </div>
        </div>
      </Show>
    </div>
  );
};

export default Sources;
