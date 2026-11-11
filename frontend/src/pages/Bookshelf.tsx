import { createSignal, createEffect, onMount, For, Show, type Component } from "solid-js";
import { createVirtualizer } from "@tanstack/solid-virtual";
import { api, type Book, type RecentBook } from "../lib/api";

const PAGE_SIZE = 50;

const Bookshelf: Component = () => {
  const [q, setQ] = createSignal("");
  const [author, setAuthor] = createSignal("");
  const [tag, setTag] = createSignal("");
  const [sort, setSort] = createSignal("updated");
  const [books, setBooks] = createSignal<Book[]>([]);
  const [total, setTotal] = createSignal(0);
  const [page, setPage] = createSignal(1);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal("");
  const [recent, setRecent] = createSignal<RecentBook[]>([]);
  const [recentOpen, setRecentOpen] = createSignal(true);
  let parentRef: HTMLDivElement | undefined;

  // 每次筛选输入都会触发 load，连续输入会并发多个请求；用序号丢弃过期响应，
  // 否则先发的慢响应会覆盖后发的筛选结果。
  let loadSeq = 0;
  const load = async (p: number, append: boolean) => {
    const my = ++loadSeq;
    setLoading(true);
    setError("");
    try {
      const res = await api.listBooks({ q: q(), author: author(), tag: tag(), sort: sort(), page: p, page_size: PAGE_SIZE });
      if (my !== loadSeq) return;
      setBooks(append ? [...books(), ...res.items] : res.items);
      setTotal(res.total);
      setPage(p);
    } catch (e) {
      if (my !== loadSeq) return;
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (my === loadSeq) setLoading(false);
    }
  };

  // 最近阅读：最近打开的 100 本书（打开书时阅读器会写进度），点开续读。
  onMount(async () => {
    try {
      const res = await api.recentBooks();
      setRecent(res.items);
    } catch {
      // 没有最近阅读不影响书库列表。
    }
  });

  // 筛选变化时回到第 1 页并滚回顶部（否则虚拟列表还停在旧的滚动位置，
  // 配合同步滞后的测量缓存会渲染到越界的旧索引）。
  createEffect(() => {
    void q(); void author(); void tag(); void sort();
    parentRef?.scrollTo({ top: 0 });
    load(1, false);
  });

  const virtualizer = createVirtualizer({
    get count() {
      return books().length;
    },
    getScrollElement: () => parentRef ?? null,
    estimateSize: () => 76,
    overscan: 8,
  });

  // 可见书目直接由 books() 派生：虚拟滚动内部的 getVirtualItems() 是
  // 一个 store，经 reconcile 按 index 复用旧项，列表换了它也可能不更新，
  // 造成“共 N 本”已变、列表还显示旧书；越界项直接滤掉，顺带防崩。
  type VItem = ReturnType<typeof virtualizer.getVirtualItems>[number];
  const visibleBooks = (): { item: VItem; book: Book }[] => {
    const list = books();
    const out: { item: VItem; book: Book }[] = [];
    for (const item of virtualizer.getVirtualItems()) {
      const book = list[item.index];
      if (book) out.push({ item, book });
    }
    return out;
  };

  // books() 变化时强制虚拟滚动重测，避免容器高度还停在旧列表的尺寸。
  createEffect(() => {
    void books();
    virtualizer.measure();
  });

  const onScroll = () => {
    const el = parentRef;
    if (!el || loading()) return;
    if (books().length >= total()) return;
    if (el.scrollHeight - el.scrollTop - el.clientHeight < 400) load(page() + 1, true);
  };

  return (
    <div>
      <div class="toolbar">
        <input placeholder="书名关键词" value={q()} onInput={(e) => setQ(e.currentTarget.value)} />
        <input placeholder="作者" value={author()} onInput={(e) => setAuthor(e.currentTarget.value)} />
        <input placeholder="标签" value={tag()} onInput={(e) => setTag(e.currentTarget.value)} />
        <select value={sort()} onChange={(e) => setSort(e.currentTarget.value)}>
          <option value="updated">最近更新</option>
          <option value="title">书名</option>
          <option value="author">作者</option>
        </select>
      </div>
      <Show when={recent().length > 0}>
        <div class="card">
          <h3 style={{ cursor: "pointer", margin: "0 0 8px" }} onClick={() => setRecentOpen(!recentOpen())}>
            最近阅读 {recentOpen() ? "▾" : "▸"}
          </h3>
          <Show when={recentOpen()}>
            <For each={recent()}>
              {(r) => (
                <div
                  class="recent-row"
                  onClick={() => (location.hash = `#/read/${encodeURIComponent(r.id)}/${r.last_chapter_idx}`)}
                >
                  <strong>{r.title}</strong>
                  <span class="muted"> {r.author ?? ""}</span>
                  <span class="muted"> · 第 {r.last_chapter_idx + 1} 章</span>
                </div>
              )}
            </For>
          </Show>
        </div>
      </Show>
      <p class="muted">共 {total()} 本 · 虚拟滚动（长列表不渲染全部 DOM）</p>
      <Show when={error()}><p class="error">{error()}</p></Show>
      <div class="vlist" ref={parentRef} onScroll={onScroll}>
        <div style={{ height: `${virtualizer.getTotalSize()}px`, position: "relative" }}>
          <For each={visibleBooks()}>
            {(x) => {
              const item = x.item;
              const book = x.book;
              const b = () => book;
              return (
                <div
                  class="vlist-item"
                  style={{ position: "absolute", top: "0", left: "0", width: "100%", transform: `translateY(${item.start}px)`, height: `${item.size}px`, display: "flex", gap: "8px", "align-items": "center" }}
                  onClick={() => (location.hash = `#/read/${encodeURIComponent(b().id)}`)}
                >
                  <Show when={b().cover}>
                    <img src={`/api/books/${encodeURIComponent(b().id)}/cover`} alt="" style={{ height: "56px", width: "40px", "object-fit": "cover", "flex-shrink": "0" }} />
                  </Show>
                  <div style={{ "min-width": "0" }}>
                  <strong>{b().title}</strong>
                  <span class="muted"> {b().author ?? ""}</span>
                  <Show when={b().has_update}>
                    <span class="badge" style={{ "background": "#b45309", "color": "#fff" }}> 有更新</span>
                  </Show>
                  <div>
                    <span class="badge">{b().source ?? "local"}</span>
                    <Show when={b().chapter_count != null}><span class="badge"> {b().chapter_count} 章</span></Show>
                    <For each={b().tags ?? []}>{(t) => <span class="badge"> {t}</span>}</For>
                  </div>
                  </div>
                </div>
              );
            }}
          </For>
        </div>
      </div>
      <Show when={loading()}><p class="muted">加载中…</p></Show>
      <Show when={!loading() && books().length === 0 && !error()}>
        <p class="muted">书库为空：后端扫描书籍目录后这里会出现书籍（或检查后端是否运行）。</p>
      </Show>
    </div>
  );
};

export default Bookshelf;
