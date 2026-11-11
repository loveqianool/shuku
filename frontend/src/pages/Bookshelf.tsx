import { createSignal, createEffect, For, Show, type Component } from "solid-js";
import { createVirtualizer } from "@tanstack/solid-virtual";
import { api, type Book, type Source } from "../lib/api";

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
  const [notice, setNotice] = createSignal("");
  const [sources, setSources] = createSignal<Source[]>([]);
  const [changingId, setChangingId] = createSignal<string | null>(null);
  const [changeSourceId, setChangeSourceId] = createSignal("");
  let parentRef: HTMLDivElement | undefined;

  const load = async (p: number, append: boolean) => {
    setLoading(true);
    setError("");
    try {
      const res = await api.listBooks({ q: q(), author: author(), tag: tag(), sort: sort(), page: p, page_size: PAGE_SIZE });
      setBooks(append ? [...books(), ...res.items] : res.items);
      setTotal(res.total);
      setPage(p);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  // 筛选变化时回到第 1 页
  createEffect(() => {
    void q(); void author(); void tag(); void sort();
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
        <button
          onClick={async () => {
            setNotice("");
            try {
              const res = await api.checkUpdatesAll();
              setNotice(`已排队检查 ${res.queued ?? 0} 本书的更新（后台进行，稍后刷新查看）`);
            } catch (e) {
              setError(e instanceof Error ? e.message : String(e));
            }
          }}
        >
          检查更新
        </button>
      </div>
      <p class="muted">共 {total()} 本 · 虚拟滚动（长列表不渲染全部 DOM）</p>
      <Show when={notice()}><p class="muted">{notice()}</p></Show>
      <Show when={error()}><p class="error">{error()}</p></Show>
      <div class="vlist" ref={parentRef} onScroll={onScroll}>
        <div style={{ height: `${virtualizer.getTotalSize()}px`, position: "relative" }}>
          <For each={virtualizer.getVirtualItems()}>
            {(item) => {
              const b = () => books()[item.index];
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
                  <Show when={b().source === "cached"}>
                    <button
                      style={{ "margin-left": "8px" }}
                      onClick={async (e) => {
                        e.stopPropagation();
                        setNotice("");
                        try {
                          await api.checkUpdate(b().id);
                          setNotice(`已排队检查《${b().title}》的更新`);
                        } catch (err) {
                          setError(err instanceof Error ? err.message : String(err));
                        }
                      }}
                    >
                      检查
                    </button>
                    <button
                      style={{ "margin-left": "4px" }}
                      onClick={async (e) => {
                        e.stopPropagation();
                        if (sources().length === 0) {
                          try {
                            setSources(await api.listSources());
                          } catch (err) {
                            setError(err instanceof Error ? err.message : String(err));
                            return;
                          }
                        }
                        const enabled = sources().filter((s) => s.enabled !== false);
                        setChangeSourceId(enabled[0]?.bookSourceUrl ?? "");
                        setChangingId(b().id);
                      }}
                    >
                      换源
                    </button>
                  </Show>
                  <Show when={changingId() === b().id}>
                    <span onClick={(e) => e.stopPropagation()}>
                      <select value={changeSourceId()} onChange={(e) => setChangeSourceId(e.currentTarget.value)}>
                        <For each={sources().filter((s) => s.enabled !== false)}>
                          {(s) => <option value={s.bookSourceUrl}>{s.bookSourceName}</option>}
                        </For>
                      </select>
                      <button
                        onClick={async () => {
                          if (!changeSourceId()) return;
                          try {
                            await api.changeSource(b().id, { source_id: changeSourceId() });
                            setNotice(`已排队为《${b().title}》换源（按书名在新源搜索，后台进行）`);
                          } catch (err) {
                            setError(err instanceof Error ? err.message : String(err));
                          } finally {
                            setChangingId(null);
                          }
                        }}
                      >
                        确认
                      </button>
                      <button onClick={() => setChangingId(null)}>取消</button>
                    </span>
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
        <p class="muted">书架为空：后端扫描书籍目录后这里会出现书籍（或检查后端是否运行）。</p>
      </Show>
    </div>
  );
};

export default Bookshelf;
