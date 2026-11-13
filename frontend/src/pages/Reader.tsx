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

// 瀑布流阅读器：章节按顺序连续渲染，滚到底（前 1200px）自动载下一章，
// 滚到顶（400px 内）自动补上一章，一直往下读到全书完。不再按章切页，
// 仿真翻页模式已删（与瀑布流冲突）。进度按 {当前章, 章内像素偏移} 存，
// 与旧 Progress 格式兼容（offset 是章内偏移，不是整页滚动值）。
const Reader: Component<{ bookId: string; initialIdx: number }> = (props) => {
  const [book, setBook] = createSignal<Book | null>(null);
  const [toc, setToc] = createSignal<ChapterMeta[]>([]);
  const [showToc, setShowToc] = createSignal(false);
  // 已载入的章节（按 idx 升序连续）。跳章/搜尋跳轉時重置窗口，只留目標章起
  // 的內容，避免一次抓幾十章；自然滾動時逐章追加/前補。
  const [chapters, setChapters] = createSignal<Chapter[]>([]);
  // 当前在视口里的章节（由滚动位置定）：进度、目录高亮、朗读、批注都跟它。
  const [idx, setIdx] = createSignal(props.initialIdx);
  const [settings, setSettings] = createSignal<ReaderSettings>(loadSettings());
  const [error, setError] = createSignal("");
  const [loading, setLoading] = createSignal(false);
  // 逐章追加/前補進行中（防重入；初始載入用 loading）。
  const [loadingMore, setLoadingMore] = createSignal(false);
  // 追加/前補抓章失敗時置真，顯示「點此重試」（抓失敗不應靜默卡住）。
  const [loadMoreError, setLoadMoreError] = createSignal(false);
  // 批注按章存（瀑布流同時渲染多章，單一 highlights signal 不夠用）。
  const [highlightsByChapter, setHighlightsByChapter] = createSignal<Record<number, Highlight[]>>({});
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
  // 当前章内滚动进度（0..1），驱动右边的竖向滑杆与全书进度。
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
  // 进阅读界面时顶栏/底栏默认隐藏（点正文唤出；之前若显示过，打开新书时恢复隐藏）。
  onMount(() => setShowChrome(false));

  let contentRef: HTMLDivElement | undefined;
  let saveTimer: ReturnType<typeof setTimeout> | undefined;

  // 已载入窗口的上下界（chapters() 按 idx 升序连续）。
  const minLoaded = () => (chapters().length ? chapters()[0].chapter_idx : -1);
  const maxLoaded = () => (chapters().length ? chapters()[chapters().length - 1].chapter_idx : -1);
  const allLoaded = () => toc().length > 0 && maxLoaded() >= toc().length - 1;
  // 某章的 <section> 元素（data-chapter-idx 定位，搜尋/跳轉時圈定範圍）。
  const chapterEl = (i: number) =>
    contentRef?.querySelector(`[data-chapter-idx="${i}"]`) as HTMLElement | null;
  const currentChapter = () => chapters().find((c) => c.chapter_idx === idx()) ?? null;

  // 当前选中处定位：目标章渲染好之后，把该章第 currentOccurrence 个
  // mark.search-hit 单独标亮（search-hit-current）并滚到它。只在当前
  // 章节就是选中处所在的章节时定位，避免换章途中用旧的序号去滚新章。
  createEffect(() => {
    const k = currentOccurrence();
    const term = searchTerm();
    void chapters();
    if (k < 0 || !term) return;
    const m = inBookMatches()[currentMatch()];
    if (!m || m.chapter_idx !== idx()) return;
    requestAnimationFrame(() => {
      const sec = chapterEl(m.chapter_idx);
      if (!sec) return;
      const marks = sec.querySelectorAll("mark.search-hit");
      if (k >= marks.length) return;
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
    const controller = new AbortController();
    downloadController = controller;
    try {
      await downloadBook(props.bookId, n, setDownloadProgress, { signal: controller.signal });
    } finally {
      downloadController = undefined;
      setDownloading(false);
      refreshOffline();
    }
  };
  // 取消整本离线：中断进行中的章节请求，worker 循环停下来不再取下一章。
  let downloadController: AbortController | undefined;
  const cancelDownload = () => {
    downloadController?.abort();
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
    // 朗读当前在视口里的那章（瀑布流没有“当前单章”，跟视口走）。
    const text = (currentChapter()?.paragraphs ?? [])
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

  const loadToc = async (bid: string) => {
    const all: ChapterMeta[] = [];
    let page = 1;
    for (;;) {
      const res = await api.listChapters(bid, page, 200);
      all.push(...res.items);
      if (all.length >= res.total || res.items.length === 0) break;
      page += 1;
    }
    // 只认当前这本书：换书后旧的 TOC 响应直接丢弃。不用 loadSeq 守卫，
    // 因为 resetToChapter 也会自增 loadSeq，会把目录误判为过期。
    if (bid !== props.bookId) return;
    setToc(all);
  };

  // 序号守卫：换书/跳章时先到的响应不覆盖后到的（Bookshelf 同款 loadSeq）。
  // 每次载入 / 换书 effect 都自增，await 回来后对不上就丢弃。
  let loadSeq = 0;

  // 抓一章正文（不改状态；seq 对不上或失败返 null）。
  const fetchOne = async (i: number, seq: number): Promise<Chapter | null> => {
    try {
      const ch = await api.getChapter(props.bookId, i);
      return seq === loadSeq ? ch : null;
    } catch (e) {
      if (seq === loadSeq) setError(e instanceof Error ? e.message : String(e));
      return null;
    }
  };
  const loadHighlightsFor = (i: number) => {
    setHighlightsByChapter((prev) => ({ ...prev, [i]: loadHighlights(props.bookId, i) }));
  };

  // 往后追加一章（滚到底触发）。返 false 表示没载入（已到底/在载/序号过期）。
  // 注意：finally 必须无条件清 loadingMore。旧代码用 `if (seq === loadSeq)`
  // 守住，跳章/拖滑杆令 loadSeq 自增时，过期的追加永远清不掉 loadingMore，
  // 之后所有追加都被 `loadingMore()` 挡住，永久卡在「加载下一章中…」。
  const appendOne = async (): Promise<boolean> => {
    const total = toc().length;
    const next = maxLoaded() + 1;
    if (total === 0 || next >= total || loadingMore()) return false;
    setLoadingMore(true);
    setLoadMoreError(false);
    const seq = loadSeq;
    try {
      const ch = await fetchOne(next, seq);
      if (seq !== loadSeq) return false; // 过期：已有更新的操作接手，不是失败
      if (!ch) { setLoadMoreError(true); return false; } // 真抓失败，显示重试
      loadHighlightsFor(next);
      setChapters((prev) => [...prev, ch]);
      return true;
    } finally {
      setLoadingMore(false);
    }
  };
  // 往前补一章（滚到顶触发）。前补会把已读内容往下顶，载完按增加的高度
  // 把滚动位置补回去，视口内容不动。
  const prependOne = async (): Promise<boolean> => {
    const prev = minLoaded() - 1;
    if (prev < 0 || loadingMore()) return false;
    setLoadingMore(true);
    setLoadMoreError(false);
    const seq = loadSeq;
    const beforeH = document.documentElement.scrollHeight;
    const beforeY = window.scrollY;
    try {
      const ch = await fetchOne(prev, seq);
      if (seq !== loadSeq) return false;
      if (!ch) { setLoadMoreError(true); return false; }
      loadHighlightsFor(prev);
      setChapters((prevChs) => [ch, ...prevChs]);
      requestAnimationFrame(() => {
        if (seq !== loadSeq) return;
        const afterH = document.documentElement.scrollHeight;
        window.scrollTo(0, beforeY + (afterH - beforeH));
      });
      return true;
    } finally {
      setLoadingMore(false);
    }
  };

  // 跳转：把载入窗口重置到目标章（只留目标章，免得远跳时一次抓幾十章），
  // 再滚到章内位置。opts.frac：章内 0..1；opts.offsetPx：恢复进度用的
  // 章内像素偏移；opts.scroll=false 时只载入不滚动（搜尋定位由 effect 接手）。
  const resetToChapter = async (
    i: number,
    opts?: { frac?: number; offsetPx?: number; scroll?: boolean },
  ) => {
    const seq = ++loadSeq;
    setLoading(true);
    setError("");
    stopSpeaking();
    try {
      const ch = await fetchOne(i, seq);
      if (seq !== loadSeq || !ch) return;
      setHighlightsByChapter({ [i]: loadHighlights(props.bookId, i) });
      setChapters([ch]);
      setIdx(i);
      setSelText("");
      // 用 replaceState 更新深链（不触发 hashchange → App 路由重算，
      // 免得换书 effect 被重複觸發、loadSeq 自增令在途的追加漏清 loadingMore）。
      history.replaceState(null, "", `#/read/${encodeURIComponent(props.bookId)}/${i}`);
      // 后台先把下一章也载好，往下滚时无缝（不 await，不阻塞定位）。
      const total = toc().length;
      if (i + 1 < total) void appendOne();
      if (opts?.scroll === false) return;
      requestAnimationFrame(() => {
        if (seq !== loadSeq) return;
        const el = chapterEl(i);
        if (!el) return;
        const max = Math.max(0, el.scrollHeight - window.innerHeight);
        const top = el.offsetTop + (opts?.offsetPx !== undefined
          ? Math.min(opts.offsetPx, max)
          : (opts?.frac ?? 0) * max);
        window.scrollTo(0, top);
        setScrollFrac(getScrollFrac());
        api.saveProgress(props.bookId, {
          chapter_idx: i,
          offset: Math.max(0, window.scrollY - el.offsetTop),
        }).catch(() => {});
      });
    } catch (e) {
      if (seq !== loadSeq) return;
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (seq === loadSeq) setLoading(false);
    }
  };

  // 目录/滑杆/搜尋的跳章：已载入就直接滚过去，没载入才重置窗口。
  const jumpToChapter = async (i: number, frac = 0) => {
    const el = chapterEl(i);
    if (el) {
      const max = Math.max(0, el.scrollHeight - window.innerHeight);
      window.scrollTo({ top: el.offsetTop + frac * max });
      setIdx(i);
      return;
    }
    await resetToChapter(i, { frac });
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
    setChapters([]);
    setHighlightsByChapter({});
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
    loadToc(bid).catch((e) => { if (bid === props.bookId) setError(String(e)); });
    // 恢复进度：无明确章节时跳到上次章节，并按章内像素偏移滚回去
    // （以前只跳章不恢复章内位置，读一半的章节每次都回章首）。
    api.getProgress(bid)
      .then((p) => {
        if (seq !== loadSeq) return;
        if (props.initialIdx === 0) resetToChapter(p.chapter_idx, { offsetPx: p.offset });
        else resetToChapter(props.initialIdx);
      })
      .catch(() => { if (seq === loadSeq) resetToChapter(props.initialIdx); });
  });

  // 滚动进度自动保存（防抖 1s）。offset 是章内像素偏移（scrollY - 章节
  // section 顶），与旧 Progress 格式兼容。
  const scheduleSaveProgress = (offset: number) => {
    clearTimeout(saveTimer);
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
    if (document.visibilityState === "visible" && chapters().length > 0) {
      pendingSeconds += 1;
      if (pendingSeconds >= 60) flushReadingTime();
    }
  }, 1000);
  onCleanup(() => { clearInterval(readTimer); flushReadingTime(); stopSpeaking(); });

  // 当前章内滚动进度（0..1）：章比视口矮时整章可见即算读完（frac=1）。
  const getScrollFrac = () => {
    const el = chapterEl(idx());
    if (!el) return 0;
    const h = el.scrollHeight;
    const off = window.scrollY - el.offsetTop;
    if (h <= window.innerHeight) return off >= 0 ? 1 : 0;
    return Math.min(1, Math.max(0, off / (h - window.innerHeight)));
  };
  const scrollToFrac = (f: number) => {
    const el = chapterEl(idx());
    if (!el) return;
    const max = Math.max(0, el.scrollHeight - window.innerHeight);
    window.scrollTo({ top: el.offsetTop + Math.min(1, Math.max(0, f)) * max });
  };
  // 按滚动位置定当前章：视口顶部（120px 容差）所在的最后一章。
  const updateCurrentFromScroll = () => {
    const els = contentRef?.querySelectorAll("[data-chapter-idx]");
    if (!els || els.length === 0) return;
    let current = -1;
    els.forEach((el) => {
      const r = (el as HTMLElement).getBoundingClientRect();
      if (r.top <= 120) current = Number((el as HTMLElement).dataset.chapterIdx);
    });
    if (current === -1) current = Number((els[0] as HTMLElement).dataset.chapterIdx);
    if (current !== -1 && current !== idx()) {
      setIdx(current);
      // 同上：replaceState 不触发 hashchange，滚动换章不惊动路由。
      history.replaceState(null, "", `#/read/${encodeURIComponent(props.bookId)}/${current}`);
    }
  };
  // 整页滚动：同步当前章与章内进度、防抖保存进度，并触发瀑布流载入。
  // 靠近底部 1200px 载下一章；靠近顶部 400px 补上一章。
  const onWinScroll = () => {
    updateCurrentFromScroll();
    setScrollFrac(getScrollFrac());
    const el = chapterEl(idx());
    if (el) scheduleSaveProgress(Math.max(0, window.scrollY - el.offsetTop));
    const doc = document.documentElement;
    if (window.innerHeight + window.scrollY >= doc.scrollHeight - 1200) void appendOne();
    if (window.scrollY <= 400) void prependOne();
  };
  window.addEventListener("scroll", onWinScroll, { passive: true });
  onCleanup(() => window.removeEventListener("scroll", onWinScroll));

  // 全书进度（0..100）：章节序号 + 本章内滚动进度。
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
    if (target === idx() && chapterEl(target)) {
      scrollToFrac(frac);
      return;
    }
    void jumpToChapter(target, frac);
  };
  // 底栏「搜索」：只浮出搜索面板并聚焦输入框，不显示顶栏/底栏。
  // 回到结果列表时滚回上次点过的匹配处（jumpToMatch 会收起面板、卸载
  // 列表 DOM，再开时位置归零；这里用 current 匹配项恢复位置）。
  let searchInputRef: HTMLInputElement | undefined;
  let matchListRef: HTMLDivElement | undefined;
  const openSearch = () => {
    setShowChrome(false);
    setShowSearch(true);
    requestAnimationFrame(() => {
      const hasCurrent = currentMatch() >= 0 && inBookMatches().length > 0;
      searchInputRef?.focus({ preventScroll: hasCurrent });
      if (hasCurrent) {
        matchListRef?.querySelector(".match-row.current")?.scrollIntoView({ block: "nearest" });
      }
    });
  };
  // 点正文切换顶栏/底栏显隐（有选词时不切换，免得干扰批注）。
  const toggleChrome = () => {
    const sel = window.getSelection();
    if (sel && sel.toString().trim()) return;
    setShowChrome(!showChrome());
  };

  const onSelect = () => {
    const sel = window.getSelection();
    const t = sel?.toString().trim() ?? "";
    setSelText(t.length > 1 && t.length < 500 ? t : "");
  };

  const addHighlight = () => {
    const t = selText();
    if (!t) return;
    const cur = idx();
    const next = [...(highlightsByChapter()[cur] ?? []), { text: t, note: noteInput() || undefined, created_at: Date.now() }];
    setHighlightsByChapter((prev) => ({ ...prev, [cur]: next }));
    saveHighlights(props.bookId, cur, next);
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
    if (!chapterEl(h.chapter_idx)) {
      await resetToChapter(h.chapter_idx, { scroll: false });
    }
    requestAnimationFrame(() => {
      chapterEl(h.chapter_idx)?.querySelector("mark.search-hit")?.scrollIntoView({ block: "center" });
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
    if (!chapterEl(m.chapter_idx)) {
      setCurrentOccurrence(-1);
      await resetToChapter(m.chapter_idx, { scroll: false });
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
  const curHighlights = () => highlightsByChapter()[idx()] ?? [];

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
        <div class="card" ref={matchListRef}>
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
            <span>搜索「{searchTerm()}」：本章 {(currentChapter()?.paragraphs ?? []).reduce((n, p) => n + (p.text ?? "").split(searchTerm()).length - 1, 0)} 处</span>
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
              <button class={c.idx === idx() ? "current" : ""} onClick={() => { setShowToc(false); void jumpToChapter(c.idx); }}>
                {c.title}
              </button>
            )}
          </For>
        </div>
      </Show>

      <Show when={showSettings()}>
      {/* 設置浮動面板（fixed 浮層，唔佔文檔流）：基本排版 + 朗讀 + 整本離線
          + 缺章檢查。離線/缺章以前放閱讀器底部，讀到一半搵唔到，收進設置隨時用到。 */}
      <div class="reader-settings">
        <div class="settings-head">
          <strong>设置</strong>
          <button onClick={() => setShowSettings(false)}>✕</button>
        </div>
        <div class="settings-row">
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
          <label>繁简
            <select value={s().script} onChange={(e) => patchSettings({ script: e.currentTarget.value as ReaderSettings["script"] })}>
              <option value="orig">原文</option>
              <option value="trad">繁體</option>
              <option value="simp">简体</option>
            </select>
          </label>
          <label>语速 <input type="number" step={0.1} min={0.5} max={2} value={ttsRate()} onChange={(e) => setTtsRate(Number(e.currentTarget.value))} /></label>
        </div>
        <div class="settings-section">
          <div class="row">
            <strong>朗读</strong>
            <button onClick={speakChapter}>{speaking() ? "⏹ 停止朗读" : "🔊 朗读本章"}</button>
            <span class="muted">读当前在视口里的章节</span>
          </div>
        </div>
        <div class="settings-section">
          <strong>整本离线</strong>
          <div class="row">
            <span class="muted">
              离线：{offlineCount() === null ? "检查中…" : `${offlineCount()} / ${toc().length} 章已缓存`}
            </span>
            <button disabled={downloading() || toc().length === 0} onClick={downloadWholeBook}>
              {downloading() ? `下载中 ${downloadProgress()} / ${toc().length}` : "下载整本离线读"}
            </button>
            <Show when={downloading()}>
              <button onClick={cancelDownload}>取消</button>
            </Show>
            <Show when={(offlineCount() ?? 0) > 0 && !downloading()}>
              <button onClick={clearOffline}>删除离线缓存</button>
            </Show>
          </div>
          <span class="muted">整本离线与全库共享缓存：最多 5000 章、保留 30 天</span>
        </div>
        <div class="settings-section">
          <strong>缺章检查</strong>
          <div class="row">
            <button disabled={checkingChapters()} onClick={checkChapters}>检查缺章</button>
            <Show when={chapterIssues() !== null}>
              <span class="muted">
                {chapterIssues()!.length === 0 ? "章节连续，无缺章" : `${chapterIssues()!.length} 个问题`}
              </span>
            </Show>
          </div>
          <Show when={chapterIssues() !== null && chapterIssues()!.length > 0}>
            <For each={chapterIssues()!}>
              {(iss) => (
                <p class="muted">第 {iss.idx + 1} 章 {iss.title}：{iss.detail}</p>
              )}
            </For>
          </Show>
        </div>
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

      {/* 瀑布流正文：已载章节按顺序连续渲染，整页滚动。点正文开合顶栏/底栏。 */}
      <div
        ref={contentRef}
        class="reader-content"
        data-reader-theme={s().theme}
        style={{
          "font-size": `${s().fontSize}px`,
          "line-height": String(s().lineHeight),
          "text-indent": `${s().indent}em`,
        }}
        onClick={toggleChrome}
        onMouseUp={onSelect}
      >
        <For each={chapters()}>
          {(ch) => (
            <section class="reader-chapter" data-chapter-idx={ch.chapter_idx}>
              <h2>{disp(ch.title)}</h2>
              <For each={ch.paragraphs}>
                {(p) => (
                  <Show when={p.img} fallback={
                    <p>
                      <For each={renderTextWithHighlights(p.text ?? "", highlightsByChapter()[ch.chapter_idx] ?? [])}>
                        {(part) => part.anno ? (
                          <mark class="anno" title={(highlightsByChapter()[ch.chapter_idx] ?? []).find((h) => h.text === part.text)?.note ?? ""}>
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
              {/* 当前章的批注列表（跟在该章末尾，只在当前章有批注时显示）。 */}
              <Show when={ch.chapter_idx === idx() && curHighlights().length > 0}>
                <div class="card">
                  <strong>本章批注（{curHighlights().length}）</strong>
                  <For each={curHighlights()}>
                    {(h, i) => (
                      <p class="muted">
                        <mark class="anno">{h.text.slice(0, 40)}</mark> {h.note ?? ""}
                        <button onClick={() => {
                          const next = curHighlights().filter((_, j) => j !== i());
                          setHighlightsByChapter((prev) => ({ ...prev, [idx()]: next }));
                          saveHighlights(props.bookId, idx(), next);
                        }}>删</button>
                      </p>
                    )}
                  </For>
                </div>
              </Show>
            </section>
          )}
        </For>
      </div>
      <Show when={loadingMore()}>
        <p class="muted">加载下一章中…</p>
      </Show>
      <Show when={loadMoreError() && !loadingMore()}>
        <p class="muted">
          加载下一章失败（网络或服务异常）{" "}
          <button onClick={() => { setLoadMoreError(false); void appendOne(); }}>点此重试</button>
        </p>
      </Show>

      {/* 全书读完：只有全部章节都载入后才出现（离线/缺章已收进设置，
          这里只留完本标记与同作者推荐，免得读到一半误以为书已读完）。 */}
      <Show when={allLoaded()}>
        <p class="muted book-end">— 全书完 —</p>
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
      </Show>

      <Show when={showChrome()}>
        <div class="reader-bottombar">
          <div class="bar-actions">
            <button onClick={() => setShowToc(!showToc())}>📑 目录</button>
            <button onClick={openSearch}>🔍 搜索</button>
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
            if (target === idx() && chapterEl(target)) scrollToFrac(frac);
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
