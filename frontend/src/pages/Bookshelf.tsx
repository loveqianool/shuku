import { createSignal, createEffect, onCleanup, onMount, For, Show, type Component } from "solid-js";
import { createVirtualizer } from "@tanstack/solid-virtual";
import { api, type Book, type RecentBook } from "../lib/api";

const PAGE_SIZE = 50;

// 最近阅读的一行：左滑移除。Pointer Events 统一触摸/鼠标；
// touch-action: pan-y（样式）让浏览器只接管竖向滚动，横向滑动交给这里。
// 滑过 80px 松手即移除（先滑出动画，再调接口删进度并从列表去掉）；
// 没滑够则回弹。一次横向滑动不会触发点开（suppressClick 挡住随后的 click）。
function RecentRow(props: { book: RecentBook; onRemove: (id: string) => void }) {
  const [offsetX, setOffsetX] = createSignal(0);
  const [sliding, setSliding] = createSignal(false);
  let rowEl: HTMLDivElement | undefined;
  let startX = 0;
  let startY = 0;
  let tracking = false;
  let isSwipe = false;
  let suppressClick = false;

  const onPointerDown = (e: PointerEvent) => {
    if (e.pointerType === "mouse" && e.button !== 0) return;
    tracking = true;
    isSwipe = false;
    suppressClick = false;
    startX = e.clientX;
    startY = e.clientY;
    setSliding(false);
  };
  const onPointerMove = (e: PointerEvent) => {
    if (!tracking) return;
    const dx = e.clientX - startX;
    const dy = e.clientY - startY;
    if (!isSwipe) {
      if (Math.abs(dy) > 12 && Math.abs(dy) > Math.abs(dx)) {
        // 竖向滚动：交给浏览器，停止跟手。
        tracking = false;
        return;
      }
      if (Math.abs(dx) > 12) {
        isSwipe = true;
        suppressClick = true;
        try {
          rowEl?.setPointerCapture(e.pointerId);
        } catch {
          /* 指针已释放 */
        }
      }
    }
    if (isSwipe) {
      // 只允许向左，夹在 [-160, 0]。
      setOffsetX(Math.min(0, Math.max(-160, dx)));
    }
  };
  const finish = (remove: boolean) => {
    tracking = false;
    setSliding(true);
    if (remove) {
      setOffsetX(-(rowEl?.offsetWidth ?? 320));
      window.setTimeout(() => props.onRemove(props.book.id), 200);
    } else {
      setOffsetX(0);
    }
  };
  const onPointerUp = () => {
    if (!tracking) return;
    finish(isSwipe && offsetX() < -80);
  };
  const onPointerCancel = () => {
    if (!tracking) return;
    finish(false);
  };
  const onClick = () => {
    if (suppressClick) {
      suppressClick = false;
      return;
    }
    location.hash = `#/read/${encodeURIComponent(props.book.id)}/${props.book.last_chapter_idx}`;
  };

  return (
    <div class="recent-swipe">
      <div class="recent-swipe-bg">移除</div>
      <div
        ref={rowEl}
        class="recent-row recent-swipe-row"
        classList={{ sliding: sliding() }}
        style={{ transform: `translateX(${offsetX()}px)` }}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerCancel}
        onClick={onClick}
      >
        <strong>{props.book.title}</strong>
        <span class="muted"> {props.book.author ?? ""}</span>
        <span class="muted"> · 第 {props.book.last_chapter_idx + 1} 章</span>
      </div>
    </div>
  );
}

const Bookshelf: Component = () => {
  const [q, setQ] = createSignal("");
  const [author, setAuthor] = createSignal("");
  const [tag, setTag] = createSignal("");
  const [sort, setSort] = createSignal("updated");
  // 防抖后的筛选值（250ms）：每敲一字就发请求太浪费，输入框即时回显，
  // 真正拉列表只跟防抖后的值。
  const [dq, setDq] = createSignal("");
  const [dauthor, setDauthor] = createSignal("");
  const [dtag, setDtag] = createSignal("");
  createEffect(() => {
    const qq = q(); const aa = author(); const tt = tag();
    const t = setTimeout(() => { setDq(qq); setDauthor(aa); setDtag(tt); }, 250);
    onCleanup(() => clearTimeout(t));
  });
  const [books, setBooks] = createSignal<Book[]>([]);
  const [total, setTotal] = createSignal(0);
  const [page, setPage] = createSignal(1);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal("");
  const [recent, setRecent] = createSignal<RecentBook[]>([]);
  const [recentOpen, setRecentOpen] = createSignal(true);
  let parentRef: HTMLDivElement | undefined;

  // 左滑移除：先从列表去掉（即时反馈），再调接口删进度；接口失败则
  // 把该行加回去（避免列表与服务端不一致）。
  const removeRecent = async (id: string) => {
    const prev = recent();
    setRecent(prev.filter((r) => r.id !== id));
    try {
      await api.deleteProgress(id);
    } catch {
      setRecent(prev);
    }
  };

  // 每次筛选输入都会触发 load，连续输入会并发多个请求；用序号丢弃过期响应，
  // 否则先发的慢响应会覆盖后发的筛选结果。
  let loadSeq = 0;
  const load = async (p: number, append: boolean) => {
    const my = ++loadSeq;
    setLoading(true);
    setError("");
    try {
      const res = await api.listBooks({ q: dq(), author: dauthor(), tag: dtag(), sort: sort(), page: p, page_size: PAGE_SIZE });
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
    void dq(); void dauthor(); void dtag(); void sort();
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
      {/* 书名搜索时自动隐藏最近阅读（给搜索结果让地方）；清空搜索后恢复
          （recentOpen 折叠状态保留）。只看书名输入，不看作者/标签。 */}
      <Show when={recent().length > 0 && q().trim() === ""}>
        <div class="card">
          <h3 style={{ cursor: "pointer", margin: "0 0 8px" }} onClick={() => setRecentOpen(!recentOpen())}>
            最近阅读 {recentOpen() ? "▾" : "▸"}
          </h3>
          <Show when={recentOpen()}>
            <For each={recent()}>
              {(r) => <RecentRow book={r} onRemove={removeRecent} />}
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
              return (
                <div
                  class="vlist-item"
                  style={{ position: "absolute", top: "0", left: "0", width: "100%", transform: `translateY(${item.start}px)`, height: `${item.size}px`, display: "flex", gap: "8px", "align-items": "center" }}
                  onClick={() => (location.hash = `#/read/${encodeURIComponent(book.id)}`)}
                >
                  <Show when={book.cover_mime}>
                    <img src={`/api/books/${encodeURIComponent(book.id)}/cover`} alt="" style={{ height: "56px", width: "40px", "object-fit": "cover", "flex-shrink": "0" }} />
                  </Show>
                  <div style={{ "min-width": "0" }}>
                  <strong>{book.title}</strong>
                  <span class="muted"> {book.author ?? ""}</span>
                  <Show when={book.has_update}>
                    <span class="badge" style={{ "background": "#b45309", "color": "#fff" }}> 有更新</span>
                  </Show>
                  <div>
                    <span class="badge">{book.source ?? "local"}</span>
                    <Show when={book.chapter_count != null}><span class="badge"> {book.chapter_count} 章</span></Show>
                    <For each={book.tags ?? []}>{(t) => <span class="badge"> {t}</span>}</For>
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
