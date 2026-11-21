import { createSignal, For, Show, onMount, type Component } from "solid-js";
import { api, type SemanticHit } from "../lib/api";

// 语义搜索：按情节描述找段落。查询向量在浏览器内嵌
// （transformers.js，bge-small-zh-v1.5 q8，与预计算的段落向量
// 同模型/同 pooling/同 instruction 前缀），再把向量发给后端
// 比对预计算的段落向量；后端只做余弦相似度，不跑模型。
// 模型首次使用时从 CDN 下载约 25MB。

const QUERY_INSTRUCTION = "为这个句子生成表示以用于检索相关文章：";
const MODEL = "Xenova/bge-small-zh-v1.5";

type Extractor = (texts: string[], opts?: Record<string, unknown>) => Promise<{ tolist: () => number[][] }>;
type TransformersModule = {
  pipeline: (task: string, model: string, opts?: Record<string, unknown>) => Promise<Extractor>;
  env: { allowLocalModels: boolean };
};

let transformersPromise: Promise<TransformersModule> | null = null;
function loadTransformers(): Promise<TransformersModule> {
  if (!transformersPromise) {
    transformersPromise = (async () => {
      // URL 存变量：TypeScript 不对变量做模块解析；@vite-ignore 让 Vite
      // 不处理这个运行时动态导入（模型库走 CDN，不进打包产物）。
      const TRANSFORMERS_CDN = "https://cdn.jsdelivr.net/npm/@huggingface/transformers@3";
      const mod = (await import(/* @vite-ignore */ TRANSFORMERS_CDN)) as unknown as TransformersModule;
      mod.env.allowLocalModels = false;
      return mod;
    })();
  }
  return transformersPromise;
}

const SemanticSearch: Component = () => {
  const [q, setQ] = createSignal("");
  const [hits, setHits] = createSignal<SemanticHit[]>([]);
  const [status, setStatus] = createSignal("");
  const [error, setError] = createSignal("");
  const [searched, setSearched] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [available, setAvailable] = createSignal<boolean | null>(null);
  const [chunkCount, setChunkCount] = createSignal(0);
  const [bookCount, setBookCount] = createSignal(0);
  let extractor: Extractor | null = null;

  onMount(() => {
    api.semanticStatus()
      .then((s) => {
        setAvailable(s.available);
        if (s.available) {
          setChunkCount(s.chunk_count ?? 0);
          setBookCount(s.book_count ?? 0);
        }
      })
      .catch(() => setAvailable(false));
  });

  const run = async () => {
    const query = q().trim();
    if (!query || loading()) return;
    setLoading(true);
    setError("");
    setSearched(true);
    setHits([]);
    try {
      if (!extractor) {
        setStatus("正在加载模型（首次约 25MB）…");
        const mod = await loadTransformers();
        extractor = await mod.pipeline("feature-extraction", MODEL, { dtype: "q8" });
      }
      setStatus("正在理解描述并比对…");
      const res = await extractor([QUERY_INSTRUCTION + query], { pooling: "mean", normalize: true });
      const vector = res.tolist()[0];
      const r = await api.semanticSearch(vector, 10);
      setHits(r.items);
      setStatus(`找到 ${r.items.length} 条最相关的段落（共比对 ${chunkCount()} 段）。`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setStatus("");
    } finally {
      setLoading(false);
    }
  };

  return (
    <div>
      <h2>语义搜索</h2>
      <p class="muted">按情节描述找段落：记得发生了什么、但不记得原词时用。模型在浏览器内运行，首次加载约 25MB。</p>
      <Show when={available() === false}>
        <p class="error">语义搜索未启用：服务端没有 semantic.json（预计算的段落向量）。</p>
      </Show>
      <Show when={available()}>
        <p class="muted">已覆盖 {bookCount()} 本书、{chunkCount()} 个段落。</p>
      </Show>
      <div class="toolbar">
        <input
          style={{ flex: "1" }}
          placeholder="例如：雨夜里主角被人追杀，躲进破庙"
          value={q()}
          onInput={(e) => setQ(e.currentTarget.value)}
          onKeyDown={(e) => { if (e.key === "Enter") run(); }}
          disabled={available() === false}
        />
        <button class="primary" onClick={run} disabled={loading() || available() === false}>
          {loading() ? "搜索中…" : "搜索"}
        </button>
      </div>
      <Show when={error()}><p class="error">{error()}</p></Show>
      <Show when={status()}><p class="muted">{status()}</p></Show>
      <Show when={searched() && !loading()}>
        <For each={hits()}>
          {(h) => (
            <div class="card" onClick={() => (location.hash = `#/read/${encodeURIComponent(h.book_id)}/${h.chapter_idx}`)} style={{ cursor: "pointer" }}>
              <h3>{h.book_title} <span class="muted">/ {h.chapter_title}</span></h3>
              <p class="muted">第 {h.chapter_idx + 1} 章 · 相似度 {(h.score * 100).toFixed(1)}%</p>
              <p>{h.snippet}</p>
            </div>
          )}
        </For>
        <Show when={hits().length === 0}><p class="muted">没有命中，换个描述试试（越具体越好：人物、地点、发生了什么）。</p></Show>
      </Show>
    </div>
  );
};

export default SemanticSearch;
