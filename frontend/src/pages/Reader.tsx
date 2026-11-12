import { createSignal, createEffect, For, Show, onCleanup, onMount, type Component } from "solid-js";
import { readerChrome as showChrome, setReaderChrome as setShowChrome } from "../lib/readerChrome";
import { api, type Book, type Chapter, type ChapterMeta, type ChapterIssue, type SearchHit, type BookMatch } from "../lib/api";
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
  const [searchDone, setSearchDone] = createSignal(false);
  // 本书内搜索：逐个列出全部出现处（另一家阅读器的交互），点一个跳去那一处。
  const [inBookMatches, setInBookMatches] = createSignal<BookMatch[]>([]);
  const [inBookMatchTotal, setInBookMatchTotal] = createSignal(0);
  const [inBookMatchPage, setInBookMatchPage] = createSignal(1);
  // 当前选中的是第几处（在全部结果里的序号，0 起）；-1 表示未选。
  const [currentMatch, setCurrentMatch] = createSignal(-1);
  // 当前选中处在本章里的第几个出现处（0 起），用来在正文里单独标亮并滚动定位。
  const [currentOccurrence, setCurrentOccurrence] = createSignal(-1);
  // 本书内搜索高亮：点结果跳章后，在正文里标亮该词全部出现处。
  const [searchTerm, setSearchTerm] = createSignal("");
  // 仿另一家阅读器的交互：顶栏和底栏平时显示，点一下正文收起来，
  // 再点一下浮出来；搜尋与设置面板由底栏按钮唤出，平时收起。
  const [showSearch, setShowSearch] = createSignal(false);
  const [showSettings, setShowSettings] = createSignal(false);
  // 本章内滚动进度（0..1），驱动右边的竖向滑杆与全书进度。
  const [scrollFrac, setScrollFrac] = createSignal(0);
  // 竖向全书滑杆拖动中的值（null 表示没在拖），松手才跳章，免得拖动时反复加载。
  const [bookDrag, setBookDrag] = createSignal<number | null>(null);
  // 顶栏两行叠放：App 导航（阅读页 fixed 浮层）+ 阅读器顶栏（fixed）。
  // 导航高度随屏宽换行而变，量实高再把阅读器顶栏顶到导航下方，
  // 免得两栏在视口顶部重叠；窗口变宽/窄时重测。
  const [navH, setNavH] = createSignal(0);
  const measureBars = () => {
    const nav = document.querySelector(".nav") as HTMLElement | null;
    setNavH(nav ? nav.offsetHeight : 0);
  };
  createEffect(() => {
    if (showChrome()) requestAnimationFrame(measureBars);
  });
  onMount(() => {
    window.addEventListener("resize", measureBars);
    onCleanup(() => window.removeEventListener("resize", measureBars));
  });
  // 竖向全书滑杆拖动提示：只显示全书百分比（不按章节），停 1.2 秒后淡出消失。
  const [bookTip, setBookTip] = createSignal<{ pct: number } | null>(null);
  const [tipFading, setTipFading] = createSignal(false);
  let tipTimer: ReturnType<typeof setTimeout> | undefined;
  let tipClearTimer: ReturnType<typeof setTimeout> | undefined;
  const showBookTip = (pct: number) => {
    if (toc().length === 0) return;
    setBookTip({ pct });
    setTipFading(false);
    clearTimeout(tipTimer);
    clearTimeout(tipClearTimer);
    tipTimer = setTimeout(() => setTipFading(true), 1200);
    tipClearTimer = setTimeout(() => setBookTip(null), 1500);
  };
  onCleanup(() => { clearTimeout(tipTimer); clearTimeout(tipClearTimer); });
  // 进阅读界面时顶栏/底栏默认显示（之前若收起过，打开新书时恢复）。
  onMount(() => setShowChrome(true));

  // 当前选中处定位：章节渲染好之后，把本章第 currentOccurrence 个
  // mark.search-hit 单独标亮（search-hit-current）并滚到它。只在当前
  // 章节就是选中处所在的章节时定位，避免换章途中用旧的序号去滚新章。
  createEffect(() => {
    const k = currentOccurrence();
    const term = searchTerm();
    void chapter();
    if (k < 0 || !term) return;
    const m = inBookMatches()[currentMatch()];
    if (!m || m.chapter_idx !== idx()) return;
    requestAnimationFrame(() => {
      const marks = contentRef?.querySelectorAll("mark.search-hit");
      if (!marks || k >= marks.length) return;
      marks.forEach((el) => el.classList.remove("search-hit-current"));
      const el = marks[k];
      el.classList.add("search-hit-current");
      el.scrollIntoView({ block: "center" });
    });
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
    const seq = ++scriptSeq;
    if (script === "orig") {
      setConverter(null);
      return;
    }
    try {
      if (script === "trad") {
        const m = await import("opencc-js/cn2t");
        if (seq !== scriptSeq) return;
        setConverter({ fn: m.Converter({ from: "cn", to: "t" }) });
      } else {
        const m = await import("opencc-js/t2cn");
        if (seq !== scriptSeq) return;
        setConverter({ fn: m.Converter({ from: "t", to: "cn" }) });
      }
    } catch {
      if (seq === scriptSeq) setConverter(null);
    }
  };
  // 只跟 script 变化：settings 是整包 signal，读它会让改字号/模式也
  // 触发重复 import；再加序号守卫，快速来回切换时旧的 import 回来
  // 不覆盖新的 converter。
  let scriptSeq = 0;
  let lastScript: ReaderSettings["script"] | null = null;
  createEffect(() => {
    const script = settings().script;
    if (script === lastScript) return;
    lastScript = script;
    applyScript(script);
  });

  // 朗读：浏览器 speechSynthesis，优先中文语音。切章/退出时停。
  // getVoices() 首次常为空（语音异步加载）：进页面就预热并监听
  // voiceschanged 缓存中文语音，否则第一次朗读总落到默认语音。
  const [speaking, setSpeaking] = createSignal(false);
  const [ttsRate, setTtsRate] = createSignal(1);
  const [zhVoice, setZhVoice] = createSignal<SpeechSynthesisVoice | null>(null);
  const pickZhVoice = () => {
    const v = window.speechSynthesis?.getVoices().find((x) => x.lang?.startsWith("zh")) ?? null;
    if (v) setZhVoice(v);
    return v;
  };
  onMount(() => {
    pickZhVoice();
    const synth = window.speechSynthesis;
    if (!synth) return;
    synth.addEventListener("voiceschanged", pickZhVoice);
    onCleanup(() => synth.removeEventListener("voiceschanged", pickZhVoice));
  });
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
    const zh = zhVoice() ?? synth.getVoices().find((v) => v.lang?.startsWith("zh"));
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
    // 输入框 min/max 只拦 UI，typed 值、脏的 localStorage 仍可能越界
    // 或 NaN；这里按字段夹紧，NaN 回默认值。
    if (typeof next.fontSize === "number")
      next.fontSize = Number.isFinite(next.fontSize) ? Math.min(32, Math.max(12, next.fontSize)) : 18;
    if (typeof next.lineHeight === "number")
      next.lineHeight = Number.isFinite(next.lineHeight) ? Math.min(3, Math.max(1.2, next.lineHeight)) : 1.9;
    if (typeof next.indent === "number")
      next.indent = Number.isFinite(next.indent) ? Math.min(4, Math.max(0, next.indent)) : 2;
    setSettings(next);
    saveSettings(next);
  };

  const loadToc = async (seq?: number) => {
    const all: ChapterMeta[] = [];
    let page = 1;
    for (;;) {
      const res = await api.listChapters(props.bookId, page, 200);
      all.push(...res.items);
      if (all.length >= res.total || res.items.length === 0) break;
      page += 1;
    }
    if (seq !== undefined && seq !== loadSeq) return;
    setToc(all);
  };

  // 序号守卫：换书/跳章时先到的响应不覆盖后到的（Bookshelf 同款 loadSeq）。
  // 每次 loadChapter / 换书 effect 都自增，await 回来后对不上就丢弃。
  let loadSeq = 0;

  // frac?: 0..1，换章后滚到章内对应位置（竖向全书滑杆按百分比定位时用）。
  // offsetPx?: 恢复进度时用的像素偏移（读到的 offset 原样滚回去）。
  // 两者都不传则滚到章首。滚动要等章节渲染好（scrollHeight 就绪）再做。
  const loadChapter = async (i: number, frac?: number, offsetPx?: number) => {
    const seq = ++loadSeq;
    setLoading(true);
    setError("");
    stopSpeaking();
    try {
      const ch = await api.getChapter(props.bookId, i);
      if (seq !== loadSeq) return;
      setChapter(ch);
      setIdx(i);
      setHighlights(loadHighlights(props.bookId, i));
      setSelText("");
      location.hash = `#/read/${encodeURIComponent(props.bookId)}/${i}`;
      if (frac === undefined && offsetPx === undefined) {
        if (contentRef) contentRef.scrollTop = 0;
        window.scrollTo(0, 0);
        setScrollFrac(0);
        // 进度：切章即保存
        await api.saveProgress(props.bookId, { chapter_idx: i, offset: 0 }).catch(() => {});
      } else {
        // 等渲染好再滚到章内位置，并按实际 offset 保存进度。
        requestAnimationFrame(() => {
          if (seq !== loadSeq) return;
          if (offsetPx !== undefined) {
            // 按像素恢复：夹到最大滚动范围，免得换了字号/屏宽后越界。
            if (settings().mode === "page") {
              const el = contentRef;
              if (el) el.scrollTop = Math.min(offsetPx, Math.max(0, el.scrollHeight - el.clientHeight));
            } else {
              window.scrollTo(0, Math.min(offsetPx, Math.max(0, document.documentElement.scrollHeight - window.innerHeight)));
            }
            setScrollFrac(getScrollFrac());
            const offset = settings().mode === "page" ? (contentRef?.scrollTop ?? 0) : window.scrollY;
            api.saveProgress(props.bookId, { chapter_idx: i, offset }).catch(() => {});
          } else {
            scrollToFrac(frac!);
            setScrollFrac(frac!);
            const offset = settings().mode === "page" ? (contentRef?.scrollTop ?? 0) : window.scrollY;
            api.saveProgress(props.bookId, { chapter_idx: i, offset }).catch(() => {});
          }
        });
      }
    } catch (e) {
      if (seq !== loadSeq) return;
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (seq === loadSeq) setLoading(false);
    }
  };

  createEffect(() => {
    const seq = ++loadSeq;
    const bid = props.bookId;
    // 换书：清掉上本书的搜索/目录/设置等状态，免得旧内容闪一下。
    // （直接复位 signal，不调 clearSearch：它定义在后面，effect 首次
    // 同步执行时会碰到 TDZ。）
    setInBookQ("");
    setInBookMatches([]);
    setInBookMatchTotal(0);
    setInBookMatchPage(1);
    setInBookHits([]);
    setInBookTotal(0);
    setSearchTerm("");
    setCurrentMatch(-1);
    setCurrentOccurrence(-1);
    setSearchDone(false);
    setShowToc(false);
    setShowSearch(false);
    setShowSettings(false);
    setChapterIssues(null);
    setSameAuthor([]);
    setScrollFrac(0);
    setChapter(null);
    setBook(null);
    setToc([]);
    api.getBook(bid).then((b) => {
      if (seq !== loadSeq) return;
      setBook(b);
      // 同作者归组：后端 author 过滤是子串匹配，前端再按全等过滤。
      if (b.author) {
        api.listBooks({ author: b.author, page: 1, page_size: 100 })
          .then((res) => { if (seq === loadSeq) setSameAuthor(res.items.filter((x) => x.author === b.author && x.id !== bid)); })
          .catch(() => {});
      }
    }).catch((e) => { if (seq === loadSeq) setError(String(e)); });
    loadToc(seq).catch((e) => { if (seq === loadSeq) setError(String(e)); });
    // 恢复进度：无明确章节时跳到上次章节，并按像素偏移滚回去
    // （以前只跳章不恢复章内位置，读一半的章节每次都回章首）。
    api.getProgress(bid)
      .then((p) => {
        if (seq !== loadSeq) return;
        if (props.initialIdx === 0) loadChapter(p.chapter_idx, undefined, p.offset);
        else loadChapter(props.initialIdx);
      })
      .catch(() => { if (seq === loadSeq) loadChapter(props.initialIdx); });
  });

  // 滚动进度自动保存（防抖 1s）：仿真翻页模式走容器滚动，
  // 滚动模式走整页滚动，两路共用同一防抖保存。
  const scheduleSaveProgress = (offset: number) => {
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      api.saveProgress(props.bookId, { chapter_idx: idx(), offset }).catch(() => {});
    }, 1000);
  };
  const onScroll = () => {
    if (!contentRef) return;
    setScrollFrac(getScrollFrac());
    scheduleSaveProgress(contentRef.scrollTop);
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

  // 本章滚动进度：仿真翻页模式下正文容器自己滚，滚动模式下整页滚。
  const getScrollFrac = () => {
    if (settings().mode === "page") {
      const el = contentRef;
      if (!el) return 0;
      const max = el.scrollHeight - el.clientHeight;
      return max > 0 ? Math.min(1, Math.max(0, el.scrollTop / max)) : 0;
    }
    const max = document.documentElement.scrollHeight - window.innerHeight;
    return max > 0 ? Math.min(1, Math.max(0, window.scrollY / max)) : 0;
  };
  const scrollToFrac = (f: number) => {
    const v = Math.min(1, Math.max(0, f));
    if (settings().mode === "page") {
      const el = contentRef;
      if (!el) return;
      el.scrollTo({ top: v * (el.scrollHeight - el.clientHeight) });
    } else {
      window.scrollTo({ top: v * (document.documentElement.scrollHeight - window.innerHeight) });
    }
  };
  // 滚动模式下整页滚，用 window 滚动事件同步本章进度并防抖保存
  // （以前只同步滑杆不保存，滚动模式下读再久进度也不更新）。
  const onWinScroll = () => {
    if (settings().mode === "page") return;
    setScrollFrac(getScrollFrac());
    scheduleSaveProgress(window.scrollY);
  };
  window.addEventListener("scroll", onWinScroll, { passive: true });
  onCleanup(() => window.removeEventListener("scroll", onWinScroll));

  // 全书进度（0..100）：章节序号 + 本章内滚动进度，底栏滑杆用。
  const bookPct = () => {
    const n = toc().length;
    if (n === 0) return 0;
    return ((idx() + scrollFrac()) / n) * 100;
  };
  // 全书百分比 → {target 章序号, frac 章内位置 0..1}。
  // pos = pct/100*n 是“章序号 + 章内位置”的连续值，不取整即不按章跳。
  const pctToChapterPos = (pct: number) => {
    const n = toc().length;
    const pos = Math.min(n - 0.0001, Math.max(0, (pct / 100) * n));
    const target = Math.min(n - 1, Math.floor(pos));
    return { target, frac: pos - target };
  };
  // 竖向全书滑杆拖动：按全书百分比连续定位，不按章节跳。
  // 落到目标章的对应位置，而不是章首；同章时只滚动。
  const jumpToBookPct = (pct: number) => {
    const n = toc().length;
    if (n === 0) return;
    const { target, frac } = pctToChapterPos(pct);
    if (target === idx()) {
      scrollToFrac(frac);
      return;
    }
    loadChapter(target, frac);
  };
  // 底栏「搜索」：浮出顶栏与搜索面板并聚焦输入框。
  let searchInputRef: HTMLInputElement | undefined;
  const openSearch = () => {
    setShowChrome(true);
    setShowSearch(true);
    requestAnimationFrame(() => searchInputRef?.focus());
  };
  // 点正文切换顶栏/底栏显隐（有选词时不切换，免得干扰批注）。
  const toggleChrome = () => {
    const sel = window.getSelection();
    if (sel && sel.toString().trim()) return;
    setShowChrome(!showChrome());
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

  // 点章节搜索结果（拼音回退时用）：跳到那章，标亮该词全部出现处，滚到第一处。
  const jumpToSearchHit = async (h: SearchHit) => {
    const term = inBookQ().trim();
    setSearchTerm(term);
    setCurrentMatch(-1);
    setCurrentOccurrence(-1);
    setInBookHits([]);
    setShowSearch(false);
    await loadChapter(h.chapter_idx);
    requestAnimationFrame(() => {
      contentRef?.querySelector("mark.search-hit")?.scrollIntoView({ block: "center" });
    });
  };

  // 点搜索结果：跳到那一处（不是只跳章），标亮该词全部出现处，
  // 再把选中的那一处单独标亮并滚到它。
  const jumpToMatch = async (i: number) => {
    const m = inBookMatches()[i];
    if (!m) return;
    setSearchTerm(inBookQ().trim());
    setCurrentMatch(i);
    setShowSearch(false);
    // 该处在本章里的第几个出现处（用来单独标亮）。
    const k = inBookMatches().slice(0, i).filter((x) => x.chapter_idx === m.chapter_idx).length;
    if (m.chapter_idx !== idx()) {
      setCurrentOccurrence(-1);
      await loadChapter(m.chapter_idx);
    }
    setCurrentOccurrence(k);
  };

  // 上一处 / 下一处：超出已加载的结果时先加载下一页（可能要连加载多页，
  // 旧代码只加载一页，目标在更后面时 jumpToMatch 会拿到 undefined）。
  const stepMatch = async (d: number) => {
    const total = inBookMatchTotal();
    if (total === 0) return;
    let n = currentMatch() + d;
    if (n < 0) n = 0;
    if (n >= total) n = total - 1;
    while (n >= inBookMatches().length && inBookMatches().length < total) {
      const before = inBookMatches().length;
      await loadMoreMatches();
      if (inBookMatches().length === before) break;
    }
    await jumpToMatch(n);
  };

  const loadMoreMatches = async () => {
    const q = inBookQ().trim();
    if (!q || inBookMatches().length >= inBookMatchTotal()) return;
    const next = inBookMatchPage() + 1;
    try {
      const res = await api.listMatches(props.bookId, q, next, 500);
      setInBookMatches([...inBookMatches(), ...res.items]);
      setInBookMatchPage(next);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const clearSearch = () => {
    setInBookQ("");
    setInBookMatches([]);
    setInBookMatchTotal(0);
    setInBookMatchPage(1);
    setInBookHits([]);
    setInBookTotal(0);
    setSearchTerm("");
    setCurrentMatch(-1);
    setCurrentOccurrence(-1);
    setSearchDone(false);
  };

  const searchInBook = async () => {
    const q = inBookQ().trim();
    if (!q) return;
    setSearchTerm("");
    setCurrentMatch(-1);
    setCurrentOccurrence(-1);
    setInBookMatches([]);
    setInBookMatchTotal(0);
    setInBookMatchPage(1);
    setInBookHits([]);
    setInBookTotal(0);
    setSearchDone(false);
    setInBookSearching(true);
    try {
      const res = await api.listMatches(props.bookId, q, 1, 500);
      setInBookMatches(res.items);
      setInBookMatchTotal(res.total);
      setInBookMatchPage(1);
      if (res.total === 0) {
        // 没有字面出现处（例如打拼音）：回退到按章节分组的搜索。
        const ch = await api.search(q, "all", 1, 20, props.bookId);
        setInBookHits(ch.items);
        setInBookTotal(ch.total);
      }
      setSearchDone(true);
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
      <Show when={showChrome()}>
        <div class="reader-bar" style={{ top: `${navH()}px` }}>
          <a href="#/">‹ 书库</a>
          <strong>{book()?.title ?? "…"}</strong>
          <span class="muted">第 {idx() + 1} 章 / 共 {toc().length} 章</span>
        </div>
      </Show>

      <Show when={showSearch()}>
        <div class="card">
          <div class="row">
            <input
              ref={searchInputRef}
              placeholder="本书内搜索"
              value={inBookQ()}
              onInput={(e) => setInBookQ(e.currentTarget.value)}
              onKeyDown={(e) => { if (e.key === "Enter") searchInBook(); }}
            />
            <button disabled={inBookSearching()} onClick={searchInBook}>搜</button>
            <button onClick={() => { setShowSearch(false); clearSearch(); }}>✕</button>
          </div>
        </div>
      </Show>

      <Show when={showSearch() && inBookMatches().length > 0}>
        <div class="card">
          <strong>本书内搜索「{inBookQ()}」：共 {inBookMatchTotal()} 处</strong>
          <button onClick={clearSearch}>清除</button>
          <For each={inBookMatches()}>
            {(m, i) => (
              <p
                class="match-row"
                classList={{ current: i() === currentMatch() }}
                onClick={() => jumpToMatch(i())}
              >
                <span class="muted">{i() + 1}. </span>
                <span innerHTML={m.snippet} />{" "}
                <span class="muted">{m.chapter_title}</span>
              </p>
            )}
          </For>
          <Show when={inBookMatches().length < inBookMatchTotal()}>
            <button onClick={loadMoreMatches}>加载更多（还剩 {inBookMatchTotal() - inBookMatches().length} 处）</button>
          </Show>
        </div>
      </Show>

      <Show when={showSearch() && inBookMatches().length === 0 && inBookHits().length > 0}>
        <div class="card">
          <strong>本书内搜索：{inBookTotal()} 条</strong>
          <button onClick={clearSearch}>清除</button>
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

      <Show when={showSearch() && inBookMatches().length === 0 && inBookHits().length === 0 && searchDone() && inBookQ().trim() !== ""}>
        <div class="card">
          <span class="muted">没有找到「{inBookQ()}」</span>
          <button onClick={clearSearch}>清除</button>
        </div>
      </Show>

      <Show when={searchTerm() && showChrome()}>
        <div class="card row">
          <Show when={currentMatch() >= 0} fallback={
            <span>搜索「{searchTerm()}」：本章 {(chapter()?.paragraphs ?? []).reduce((n, p) => n + (p.text ?? "").split(searchTerm()).length - 1, 0)} 处</span>
          }>
            <span>搜索「{searchTerm()}」：第 {currentMatch() + 1} / 共 {inBookMatchTotal()} 处</span>
            <button onClick={() => stepMatch(-1)}>‹ 上一处</button>
            <button onClick={() => stepMatch(1)}>下一处 ›</button>
          </Show>
          <button onClick={() => { setSearchTerm(""); setCurrentMatch(-1); setCurrentOccurrence(-1); }}>清除标亮</button>
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

      <Show when={showSettings()}>
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
        <label>语速 <input type="number" step={0.1} min={0.5} max={2} value={ttsRate()} onChange={(e) => setTtsRate(Number(e.currentTarget.value))} /></label>
      </div>
      </Show>

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
        <div onClick={toggleChrome}>
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
                      <mark class="anno" title={highlights().find((h) => h.text === part.text)?.note ?? ""}>
                        {/* 批注内也要切搜索词，否则词落在批注里就标不出来、定位不到 */}
                        <For each={splitOnTerm(part.text, searchTerm())}>
                          {(sp) => sp.hit ? <mark class="search-hit">{disp(sp.text)}</mark> : <>{disp(sp.text)}</>}
                        </For>
                      </mark>
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

      <Show when={showChrome()}>
        <div class="reader-bottombar">
          <div class="chapter-progress">
            <button disabled={idx() <= 0 || loading()} onClick={() => loadChapter(idx() - 1)}>‹</button>
            <input
              type="range" min="0" max="100" step="0.1"
              value={scrollFrac() * 100}
              onInput={(e) => scrollToFrac(Number(e.currentTarget.value) / 100)}
              aria-label="本章进度"
            />
            <span class="muted">{Math.round(scrollFrac() * 100)}%</span>
            <button disabled={idx() >= toc().length - 1 || loading()} onClick={() => loadChapter(idx() + 1)}>›</button>
          </div>
          <div class="bar-actions">
            <button onClick={() => setShowToc(!showToc())}>📑 目录</button>
            <button onClick={openSearch}>🔍 搜索</button>
            <button onClick={speakChapter}>{speaking() ? "⏹ 停止朗读" : "🔊 朗读"}</button>
            <button onClick={() => setShowSettings(!showSettings())}>⚙️ 设置</button>
          </div>
        </div>
      </Show>

      {/* 竖向 range 在这套 CSS 下 0 在底部、100 在顶部，
          所以 value/事件里用 100 - pct 反一下，往下拖才是往后翻。 */}
      <input
        type="range" min="0" max="100" step="0.1"
        value={bookDrag() ?? 100 - bookPct()}
        onInput={(e) => {
          const v = Number(e.currentTarget.value);
          const pct = 100 - v;
          setBookDrag(v);
          showBookTip(pct);
          // 同章时拖动即实时滚动（像真正的滚动条）；跨章等松手再换章，
          // 避免拖动途中反复发请求。
          if (toc().length > 0) {
            const { target, frac } = pctToChapterPos(pct);
            if (target === idx()) scrollToFrac(frac);
          }
        }}
        onChange={(e) => { const v = Number(e.currentTarget.value); setBookDrag(null); jumpToBookPct(100 - v); showBookTip(100 - v); }}
        class="book-progress"
        aria-label="全书进度"
      />
      <Show when={bookTip()}>
        <div class="book-tip" classList={{ fading: tipFading() }}>
          {Math.round(bookTip()!.pct)}%
        </div>
      </Show>
    </div>
  );
};

export default Reader;
