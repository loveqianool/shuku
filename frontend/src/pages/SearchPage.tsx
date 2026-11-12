import { createSignal, For, Show, onCleanup, type Component } from "solid-js";
import { api, type SearchHit, type SearchScope, type Suggestion } from "../lib/api";

const SearchPage: Component = () => {
  const [q, setQ] = createSignal("");
  const [scope, setScope] = createSignal<SearchScope>("all");
  const [hits, setHits] = createSignal<SearchHit[]>([]);
  const [total, setTotal] = createSignal(0);
  const [page, setPage] = createSignal(1);
  const [loading, setLoading] = createSignal(false);
  const [loadingMore, setLoadingMore] = createSignal(false);
  const [error, setError] = createSignal("");
  const [searched, setSearched] = createSignal(false);
  const [suggestions, setSuggestions] = createSignal<Suggestion[]>([]);
  const [showSuggest, setShowSuggest] = createSignal(false);
  let suggestTimer: ReturnType<typeof setTimeout> | undefined;
  // 联想序号守卫：防抖后先发的请求可能后到，旧结果不覆盖新输入的。
  let suggestSeq = 0;
  onCleanup(() => clearTimeout(suggestTimer));

  const onInput = (v: string) => {
    setQ(v);
    clearTimeout(suggestTimer);
    if (!v.trim()) {
      setSuggestions([]);
      return;
    }
    // 防抖 200ms：书名/作者联想（也覆盖拼音，如 xiuxian）。
    suggestTimer = setTimeout(() => {
      const seq = ++suggestSeq;
      const query = v.trim();
      api.suggest(query)
        .then((r) => {
          if (seq !== suggestSeq) return;
          setSuggestions(r.items);
          setShowSuggest(true);
        })
        .catch(() => {});
    }, 200);
  };

  const pickSuggestion = (s: Suggestion) => {
    setSuggestions([]);
    setShowSuggest(false);
    location.hash = `#/read/${encodeURIComponent(s.id)}`;
  };

  const run = async () => {
    if (!q().trim()) return;
    setLoading(true);
    setError("");
    setSearched(true);
    setShowSuggest(false);
    // 先清旧结果：新搜索加载中旧命中还挂着，会误以为是这次的结果。
    setHits([]);
    setTotal(0);
    setPage(1);
    try {
      const res = await api.search(q().trim(), scope(), 1, 20);
      setHits(res.items);
      setTotal(res.total);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  // 加载更多：下一页追加到结果列表。
  const loadMore = async () => {
    if (loading() || loadingMore() || hits().length >= total()) return;
    setLoadingMore(true);
    try {
      const res = await api.search(q().trim(), scope(), page() + 1, 20);
      setHits([...hits(), ...res.items]);
      setPage(page() + 1);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoadingMore(false);
    }
  };

  return (
    <div>
      <h2>全文检索</h2>
      <p class="muted">检索在后端 tantivy 完成（章节粒度，snippet 高亮），前端只渲染结果。</p>
      <div class="toolbar">
        <div style={{ position: "relative", flex: "1" }}>
          <input
            style={{ width: "100%" }}
            placeholder="输入关键词，回车检索"
            value={q()}
            onInput={(e) => onInput(e.currentTarget.value)}
            onKeyDown={(e) => { if (e.key === "Enter") run(); if (e.key === "Escape") setShowSuggest(false); }}
            onFocus={() => { if (suggestions().length > 0) setShowSuggest(true); }}
            onBlur={() => setTimeout(() => setShowSuggest(false), 150)}
          />
          <Show when={showSuggest() && suggestions().length > 0}>
            <div class="card" style={{ position: "absolute", top: "100%", left: 0, right: 0, "z-index": 10 }}>
              <For each={suggestions()}>
                {(s) => (
                  <p
                    style={{ cursor: "pointer", margin: "4px 0" }}
                    onMouseDown={(e) => { e.preventDefault(); pickSuggestion(s); }}
                  >
                    {s.title} <span class="muted">· {s.author || "未知作者"}</span>
                  </p>
                )}
              </For>
            </div>
          </Show>
        </div>
        <select value={scope()} onChange={(e) => setScope(e.currentTarget.value as SearchScope)}>
          <option value="all">全部</option>
          <option value="local">本地书</option>
          <option value="cached">已缓存网络书</option>
        </select>
        <button class="primary" onClick={run} disabled={loading()}>检索</button>
      </div>
      <Show when={error()}><p class="error">{error()}</p></Show>
      <Show when={loading()}><p class="muted">检索中…</p></Show>
      <Show when={searched() && !loading()}>
        <p class="muted">命中 {total()} 个章节</p>
        <For each={hits()}>
          {(h) => (
            <div class="card" onClick={() => (location.hash = `#/read/${encodeURIComponent(h.book_id)}/${h.chapter_idx}`)} style={{ cursor: "pointer" }}>
              <h3>{h.book_title} <span class="muted">/ {h.chapter_title}</span></h3>
              <p class="muted">{h.author ?? ""} · 第 {h.chapter_idx + 1} 章{(h.match_count ?? 0) > 0 ? <> · {h.match_count} 处</> : null} · score {h.score?.toFixed(2) ?? "-"}</p>
              {/* snippet 来自后端 tantivy 高亮（<em>），直接渲染 */}
              <p innerHTML={h.snippet} />
            </div>
          )}
        </For>
        <Show when={hits().length === 0}><p class="muted">没有命中，换个关键词试试。</p></Show>
        <Show when={hits().length > 0 && hits().length < total()}>
          <button onClick={loadMore} disabled={loadingMore()}>
            {loadingMore() ? "加载中…" : `加载更多（还剩 ${total() - hits().length} 个）`}
          </button>
        </Show>
      </Show>
    </div>
  );
};

export default SearchPage;
