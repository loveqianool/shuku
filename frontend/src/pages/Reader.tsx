import { createSignal, createEffect, createMemo, For, Show, onCleanup, type Component } from "solid-js";
import { api, type Book, type Chapter, type ChapterMeta, type ChapterIssue, type SearchHit } from "../lib/api";
import { downloadBook, deleteOfflineBook, offlineChapterCount } from "../lib/offline";
import {
  loadSettings, saveSettings, loadHighlights, saveHighlights,
  type ReaderSettings, type Highlight,
} from "../lib/settings";

function renderTextWithHighlights(text: string, highlights: Highlight[]) {
  // 按批注片段切分，命中片段包 <mark class="anno">
  const parts: { text: string; anno: boolean }[] = [{ text, anno: false }];
  for (const h of highlights) {
    if (!h.text) continue;
    const next: typeof parts = [];
    for (const p of parts) {
      if (p.anno) { next.push(p); continue; }
      const idx = p.text.indexOf(h.text);
      if (idx < 0) { next.push(p); continue; }
      if (idx > 0) next.push({ text: p.text.slice(0, idx), anno: false });
      next.push({ text: h.text, anno: true });
      if (idx + h.text.length < p.text.length)
        next.push({ text: p.text.slice(idx + h.text.length), anno: false });
    }
    parts.splice(0, parts.length, ...next);
  }
  return parts;
}

function splitOnTerm(text: string, term: string) {
  // 按词切分全部出现处（批注只标首处，搜索要标全部）。
  if (!term) return [{ text, hit: false }];
  const parts: { text: string; hit: boolean }[] = [];
  let rest = text;
  for (;;) {
    const i = rest.indexOf(term);
    if (i < 0) { if (rest) parts.push({ text: rest, hit: false }); break; }
    if (i > 0) parts.push({ text: rest.slice(0, i), hit: false });
    parts.push({ text: term, hit: true });
    rest = rest.slice(i + term.length);
  }
  return parts;
}

const Reader: Component<{ bookId: string; initialIdx: number }> = (props) => {
  const [book, setBook] = createSignal<Book | null>(null);
  const [toc, setToc] = createSignal<ChapterMeta[]>([]);
  const [showToc, setShowToc] = createSignal(false);
  const [idx, setIdx] = createSignal(props.initialIdx);
  const [chapter, setChapter] = createSignal<Chapter | null>(null);
  const [settings, setSettings] = createSignal<ReaderSettings>(loadSettings());
  const [error, setError] = createSignal("");
  const [loading, setLoading] = createSignal(false);
  const [highlights, setHighlights] = createSignal<Highlight[]>([]);
  const [selText, setSelText] = createSignal("");
  const [noteInput, setNoteInput] = createSignal("");
  const [inBookQ, setInBookQ] = createSignal("");
  const [inBookHits, setInBookHits] = createSignal<SearchHit[]>([]);
  const [inBookTotal, setInBookTotal] = createSignal(0);
  const [inBookSearching, setInBookSearching] = createSignal(false);
  // 本书内搜索高亮：点结果跳章后，在正文里标亮该词全部出现处。
  const [searchTerm, setSearchTerm] = createSignal("");
  const searchMatchCount = createMemo(() => {
    const term = searchTerm();
    if (!term) return 0;
    return (chapter()?.paragraphs ?? []).reduce((n, p) => n + (p.text ?? "").split(term).length - 1, 0);
  });
  const [sameAuthor, setSameAuthor] = createSignal<Book[]>([]);
  // 整本离线状态（实际查 Cache Storage）。
  const [offlineCount, setOfflineCount] = createSignal<number | null>(null);
  const [downloading, setDownloading] = createSignal(false);
  const [downloadProgress, setDownloadProgress] = createSignal(0);
  const refreshOffline = () => {
    const n = toc().length;
    if (n > 0) offlineChapterCount(props.bookId, n).then(setOfflineCount);
  };
  createEffect(() => {
    if (toc().length > 0) refreshOffline();
  });
  const downloadWholeBook = async () => {
    const n = toc().length;
    if (n === 0 || downloading()) return;
    setDownloading(true);
    setDownloadProgress(0);
    try {
      await downloadBook(props.bookId, n, setDownloadProgress);
    } finally {
      setDownloading(false);
      refreshOffline();
    }
  };
  const clearOffline = async () => {
    await deleteOfflineBook(props.bookId, toc().length);
    refreshOffline();
  };

  // 缺章/乱章检查（章节序号不连续、空章节）。
  const [chapterIssues, setChapterIssues] = createSignal<ChapterIssue[] | null>(null);
  const [checkingChapters, setCheckingChapters] = createSignal(false);
  const checkChapters = async () => {
    setCheckingChapters(true);
    try {
      const res = await api.checkChapterIssues(props.bookId);
      setChapterIssues(res.issues);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setCheckingChapters(false);
    }
  };
  // 繁简转换：词典按需加载（opencc-js/cn2t 约 1MB，t2cn 约 100KB，
  // 只在用户第一次切换时下载）。转换后按片段逐段显示，批注按原文
  // 分段不受影响（词组转换极少跨片段边界）。
  const [converter, setConverter] = createSignal<{ fn: (s: string) => string } | null>(null);
  const disp = (t: string) => {
    const c = converter();
    return c ? c.fn(t) : t;
  };
  const applyScript = async (script: ReaderSettings["script"]) => {
    if (script === "orig") {
      setConverter(null);
      return;
    }
    try {
      if (script === "trad") {
        const m = await import("opencc-js/cn2t");
        setConverter({ fn: m.Converter({ from: "cn", to: "t" }) });
      } else {
        const m = await import("opencc-js/t2cn");
        setConverter({ fn: m.Converter({ from: "t", to: "cn" }) });
      }
    } catch {
      setConverter(null);
    }
  };
  createEffect(() => {
    applyScript(settings().script);
  });

  // 朗读：浏览器 speechSynthesis，优先中文语音。切章/退出时停。
  const [speaking, setSpeaking] = createSignal(false);
  const [ttsRate, setTtsRate] = createSignal(1);
  const stopSpeaking = () => {
    window.speechSynthesis?.cancel();
    setSpeaking(false);
  };
  const speakChapter = () => {
    const synth = window.speechSynthesis;
    if (!synth) return;
    if (speaking()) {
      stopSpeaking();
      return;
    }
    const text = (chapter()?.paragraphs ?? [])
      .map((p) => disp(p.text ?? ""))
      .filter(Boolean)
      .join("\n");
    if (!text) return;
    const u = new SpeechSynthesisUtterance(text);
    const zh = synth.getVoices().find((v) => v.lang?.startsWith("zh"));
    if (zh) u.voice = zh;
    u.rate = ttsRate();
    u.onend = () => setSpeaking(false);
    u.onerror = () => setSpeaking(false);
    synth.speak(u);
    setSpeaking(true);
  };
  let contentRef: HTMLDivElement | undefined;
  let saveTimer: ReturnType<typeof setTimeout> | undefined;

  const patchSettings = (p: Partial<ReaderSettings>) => {
    const next = { ...settings(), ...p };
    setSettings(next);
    saveSettings(next);
  };

  const loadToc = async () => {
    const all: ChapterMeta[] = [];
    let page = 1;
    for (;;) {
      const res = await api.listChapters(props.bookId, page, 200);
      all.push(...res.items);
      if (all.length >= res.total || res.items.length === 0) break;
      page += 1;
    }
    setToc(all);
  };

  const loadChapter = async (i: number) => {
    setLoading(true);
    setError("");
    stopSpeaking();
    try {
      const ch = await api.getChapter(props.bookId, i);
      setChapter(ch);
      setIdx(i);
      setHighlights(loadHighlights(props.bookId, i));
      setSelText("");
      location.hash = `#/read/${encodeURIComponent(props.bookId)}/${i}`;
      if (contentRef) contentRef.scrollTop = 0;
      // 进度：切章即保存
      await api.saveProgress(props.bookId, { chapter_idx: i, offset: 0 }).catch(() => {});
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  createEffect(() => {
    api.getBook(props.bookId).then((b) => {
      setBook(b);
      // 同作者归组：后端 author 过滤是子串匹配，前端再按全等过滤。
      if (b.author) {
        api.listBooks({ author: b.author, page: 1, page_size: 100 })
          .then((res) => setSameAuthor(res.items.filter((x) => x.author === b.author && x.id !== props.bookId)))
          .catch(() => {});
      }
    }).catch((e) => setError(String(e)));
    loadToc().catch((e) => setError(String(e)));
    // 恢复进度：无明确章节时跳到上次章节
    api.getProgress(props.bookId)
      .then((p) => {
        if (props.initialIdx === 0 && p.chapter_idx > 0) loadChapter(p.chapter_idx);
        else loadChapter(props.initialIdx);
      })
      .catch(() => loadChapter(props.initialIdx));
  });

  // 滚动进度自动保存（防抖 1s）
  const onScroll = () => {
    if (!contentRef) return;
    clearTimeout(saveTimer);
    const offset = contentRef.scrollTop;
    saveTimer = setTimeout(() => {
      api.saveProgress(props.bookId, { chapter_idx: idx(), offset }).catch(() => {});
    }, 1000);
  };
  onCleanup(() => clearTimeout(saveTimer));

  // 阅读时长：页面可见且章节已加载时每秒累计，满 60 秒上报一次，
  // 退出阅读器时把剩余秒数一起上报。
  let pendingSeconds = 0;
  const flushReadingTime = () => {
    if (pendingSeconds <= 0) return;
    const s = pendingSeconds;
    pendingSeconds = 0;
    api.addReadingTime(props.bookId, s).catch(() => {});
  };
  const readTimer = setInterval(() => {
    if (document.visibilityState === "visible" && chapter()) {
      pendingSeconds += 1;
      if (pendingSeconds >= 60) flushReadingTime();
    }
  }, 1000);
  onCleanup(() => { clearInterval(readTimer); flushReadingTime(); stopSpeaking(); });

  // 仿真翻页：按一屏滚动
  const pageStep = (dir: 1 | -1) => {
    if (!contentRef) return;
    contentRef.scrollBy({ top: dir * contentRef.clientHeight * 0.92, behavior: "smooth" });
  };

  const onKey = (e: KeyboardEvent) => {
    if (settings().mode !== "page") return;
    if (e.key === "ArrowRight" || e.key === "ArrowDown" || e.key === " ") { e.preventDefault(); pageStep(1); }
    if (e.key === "ArrowLeft" || e.key === "ArrowUp") { e.preventDefault(); pageStep(-1); }
  };
  window.addEventListener("keydown", onKey);
  onCleanup(() => window.removeEventListener("keydown", onKey));

  const onSelect = () => {
    const sel = window.getSelection();
    const t = sel?.toString().trim() ?? "";
    setSelText(t.length > 1 && t.length < 500 ? t : "");
  };

  const addHighlight = () => {
    const t = selText();
    if (!t) return;
    const next = [...highlights(), { text: t, note: noteInput() || undefined, created_at: Date.now() }];
    setHighlights(next);
    saveHighlights(props.bookId, idx(), next);
    setSelText("");
    setNoteInput("");
    window.getSelection()?.removeAllRanges();
  };

  // 点搜索结果：跳到那章，标亮该词全部出现处，滚到第一处。
  const jumpToSearchHit = async (h: SearchHit) => {
    const term = inBookQ().trim();
    setSearchTerm(term);
    setInBookHits([]);
    await loadChapter(h.chapter_idx);
    requestAnimationFrame(() => {
      contentRef?.querySelector("mark.search-hit")?.scrollIntoView({ block: "center" });
    });
  };

  const searchInBook = async () => {
    const q = inBookQ().trim();
    if (!q) return;
    setSearchTerm("");
    setInBookSearching(true);
    try {
      const res = await api.search(q, "all", 1, 20, props.bookId);
      setInBookHits(res.items);
      setInBookTotal(res.total);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setInBookSearching(false);
    }
  };

  const s = () => settings();
  const ch = () => chapter();

  return (
    <div class="reader">
      <div class="reader-bar">
        <a href="#/">‹ 书架</a>
        <strong>{book()?.title ?? "…"}</strong>
        <button onClick={() => setShowToc(!showToc())}>目录</button>
        <span class="muted">第 {idx() + 1} 章 / 共 {toc().length} 章</span>
        <input
          placeholder="本书内搜索"
          value={inBookQ()}
          onInput={(e) => setInBookQ(e.currentTarget.value)}
          onKeyDown={(e) => { if (e.key === "Enter") searchInBook(); }}
        />
        <button disabled={inBookSearching()} onClick={searchInBook}>搜</button>
      </div>

      <Show when={inBookHits().length > 0 || (inBookTotal() === 0 && inBookQ().trim() !== "" && !inBookSearching())}>
        <div class="card">
          <strong>本书内搜索：{inBookTotal()} 条</strong>
          <button onClick={() => { setInBookHits([]); setInBookTotal(0); setInBookQ(""); setSearchTerm(""); }}>清除</button>
          <For each={inBookHits()}>
            {(h) => (
              <p>
                <a href="#" onClick={(e) => { e.preventDefault(); jumpToSearchHit(h); }}>
                  {h.chapter_title}
                </a>{" "}
                {(h.match_count ?? 0) > 0 ? <span class="muted">· {h.match_count} 处 </span> : null}
                <span innerHTML={h.snippet} />
              </p>
            )}
          </For>
        </div>
      </Show>

      <Show when={searchTerm()}>
        <div class="card row">
          <span>搜索「{searchTerm()}」：本章 {searchMatchCount()} 处</span>
          <button onClick={() => setSearchTerm("")}>清除标亮</button>
        </div>
      </Show>

      <Show when={showToc()}>
        <div class="toc">
          <For each={toc()}>
            {(c) => (
              <button class={c.idx === idx() ? "current" : ""} onClick={() => { setShowToc(false); loadChapter(c.idx); }}>
                {c.title}
              </button>
            )}
          </For>
        </div>
      </Show>

      <div class="reader-settings">
        <label>字号 <input type="number" min={12} max={32} value={s().fontSize} onChange={(e) => patchSettings({ fontSize: Number(e.currentTarget.value) })} /></label>
        <label>行距 <input type="number" step={0.1} min={1.2} max={3} value={s().lineHeight} onChange={(e) => patchSettings({ lineHeight: Number(e.currentTarget.value) })} /></label>
        <label>缩进 <input type="number" step={0.5} min={0} max={4} value={s().indent} onChange={(e) => patchSettings({ indent: Number(e.currentTarget.value) })} /> em</label>
        <label>主题
          <select value={s().theme} onChange={(e) => patchSettings({ theme: e.currentTarget.value as ReaderSettings["theme"] })}>
            <option value="dark">深色</option>
            <option value="light">浅色</option>
            <option value="sepia">米色</option>
          </select>
        </label>
        <label>翻页
          <select value={s().mode} onChange={(e) => patchSettings({ mode: e.currentTarget.value as ReaderSettings["mode"] })}>
            <option value="scroll">滚动</option>
            <option value="page">仿真翻页（按屏）</option>
          </select>
        </label>
        <label>繁简
          <select value={s().script} onChange={(e) => patchSettings({ script: e.currentTarget.value as ReaderSettings["script"] })}>
            <option value="orig">原文</option>
            <option value="trad">繁體</option>
            <option value="simp">简体</option>
          </select>
        </label>
      </div>

      <Show when={error()}><p class="error">{error()}</p></Show>
      <Show when={loading()}><p class="muted">加载章节中…</p></Show>

      <Show when={selText()}>
        <div class="card row">
          <span class="muted">已选 {selText().length} 字</span>
          <input placeholder="批注（可选）" value={noteInput()} onInput={(e) => setNoteInput(e.currentTarget.value)} />
          <button class="primary" onClick={addHighlight}>添加批注高亮</button>
          <button onClick={() => { setSelText(""); window.getSelection()?.removeAllRanges(); }}>取消</button>
        </div>
      </Show>

      <Show when={ch()}>
        <h2>{disp(ch()!.title)}</h2>
        <div
          ref={contentRef}
          class="reader-content"
          classList={{ "page-mode": s().mode === "page" }}
          data-reader-theme={s().theme}
          style={{
            "font-size": `${s().fontSize}px`,
            "line-height": String(s().lineHeight),
            "text-indent": `${s().indent}em`,
          }}
          onScroll={onScroll}
          onMouseUp={onSelect}
        >
          <For each={ch()!.paragraphs}>
            {(p) => (
              <Show when={p.img} fallback={
                <p>
                  <For each={renderTextWithHighlights(p.text ?? "", highlights())}>
                    {(part) => part.anno ? (
                      <mark class="anno" title={highlights().find((h) => h.text === part.text)?.note ?? ""}>{disp(part.text)}</mark>
                    ) : (
                      <For each={splitOnTerm(part.text, searchTerm())}>
                        {(sp) => sp.hit ? <mark class="search-hit">{disp(sp.text)}</mark> : <>{disp(sp.text)}</>}
                      </For>
                    )}
                  </For>
                </p>
              }>
                {/* 图片懒加载 */}
                <img src={p.img} alt="" loading="lazy" />
              </Show>
            )}
          </For>
        </div>
        <Show when={highlights().length > 0}>
          <div class="card">
            <strong>本章批注（{highlights().length}）</strong>
            <For each={highlights()}>
              {(h, i) => (
                <p class="muted">
                  <mark class="anno">{h.text.slice(0, 40)}</mark> {h.note ?? ""}
                  <button onClick={() => {
                    const next = highlights().filter((_, j) => j !== i());
                    setHighlights(next);
                    saveHighlights(props.bookId, idx(), next);
                  }}>删</button>
                </p>
              )}
            </For>
          </div>
        </Show>
      </Show>

      <Show when={sameAuthor().length > 0}>
        <div class="card">
          <strong>同作者的其他书（{sameAuthor().length}）</strong>
          <For each={sameAuthor()}>
            {(b) => (
              <p>
                <a href={`#/read/${encodeURIComponent(b.id)}`}>{b.title}</a>
                <span class="muted"> · {b.chapter_count} 章</span>
              </p>
            )}
          </For>
        </div>
      </Show>

      <div class="card row">
        <span class="muted">
          离线：{offlineCount() === null ? "检查中…" : `${offlineCount()} / ${toc().length} 章已缓存`}
        </span>
        <button disabled={downloading() || toc().length === 0} onClick={downloadWholeBook}>
          {downloading() ? `下载中 ${downloadProgress()} / ${toc().length}` : "下载整本离线读"}
        </button>
        <Show when={(offlineCount() ?? 0) > 0 && !downloading()}>
          <button onClick={clearOffline}>删除离线缓存</button>
        </Show>
      </div>

      <div class="card row">
        <button disabled={checkingChapters()} onClick={checkChapters}>检查缺章</button>
        <Show when={chapterIssues() !== null}>
          <span class="muted">
            {chapterIssues()!.length === 0 ? "章节连续，无缺章" : `${chapterIssues()!.length} 个问题`}
          </span>
        </Show>
      </div>
      <Show when={chapterIssues() !== null && chapterIssues()!.length > 0}>
        <div class="card">
          <For each={chapterIssues()!}>
            {(iss) => (
              <p>第 {iss.idx + 1} 章 {iss.title}：{iss.detail}</p>
            )}
          </For>
        </div>
      </Show>

      <div class="toolbar">
        <button disabled={idx() <= 0 || loading()} onClick={() => loadChapter(idx() - 1)}>上一章</button>
        <Show when={s().mode === "page"}>
          <button onClick={() => pageStep(-1)}>‹ 上一屏</button>
          <button onClick={() => pageStep(1)}>下一屏 ›</button>
        </Show>
        <button disabled={idx() >= toc().length - 1 || loading()} onClick={() => loadChapter(idx() + 1)}>下一章</button>
        <button onClick={speakChapter}>{speaking() ? "⏹ 停止朗读" : "🔊 朗读本章"}</button>
        <label>语速 <input type="number" step={0.1} min={0.5} max={2} value={ttsRate()} onChange={(e) => setTtsRate(Number(e.currentTarget.value))} /></label>
      </div>
    </div>
  );
};

export default Reader;
