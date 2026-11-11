import { createSignal, For, Show, type Component } from "solid-js";
import { api, type BookWithIssues, type TaskItem, type TaskStats } from "../lib/api";

const Tasks: Component = () => {
  const [type, setType] = createSignal<"scan" | "crawl" | undefined>(undefined);
  const [tasks, setTasks] = createSignal<TaskItem[]>([]);
  const [stats, setStats] = createSignal<TaskStats | null>(null);
  const [issueBooks, setIssueBooks] = createSignal<BookWithIssues[]>([]);
  const [error, setError] = createSignal("");
  const [ok, setOk] = createSignal("");
  const [scanPath, setScanPath] = createSignal("");
  const [crawlSource, setCrawlSource] = createSignal("");
  const [crawlUrl, setCrawlUrl] = createSignal("");
  const [pauseCrawl, setPauseCrawl] = createSignal(true);

  const refresh = async () => {
    try {
      const [t, s] = await Promise.all([api.listTasks(type(), undefined), api.taskStats()]);
      setTasks(t.items);
      setStats(s);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    try {
      const r = await api.listBooksWithChapterIssues();
      setIssueBooks(r.books);
    } catch {
      /* 缺章列表失败不影响任务页 */
    }
  };
  refresh();

  const act = async (fn: () => Promise<unknown>, msg: string) => {
    setError(""); setOk("");
    try { await fn(); setOk(msg); await refresh(); }
    catch (e) { setError(e instanceof Error ? e.message : String(e)); }
  };

  return (
    <div>
      <h2>任务队列</h2>
      <p class="muted">扫描队列与爬虫队列相互隔离；重建索引期间可暂停爬虫。</p>
      <Show when={stats()}>
        <div class="card"><pre>{JSON.stringify(stats(), null, 2)}</pre></div>
      </Show>

      <div class="toolbar">
        <input placeholder="扫描路径（留空用后端默认书籍目录）" value={scanPath()} onInput={(e) => setScanPath(e.currentTarget.value)} />
        <button class="primary" onClick={() => act(() => api.triggerScan(scanPath() || undefined), "已触发增量扫描")}>触发扫描</button>
      </div>
      <div class="toolbar">
        <input placeholder="source_id" value={crawlSource()} onInput={(e) => setCrawlSource(e.currentTarget.value)} />
        <input placeholder="book_url" value={crawlUrl()} onInput={(e) => setCrawlUrl(e.currentTarget.value)} />
        <button onClick={() => act(() => api.enqueueCrawl(crawlSource(), crawlUrl()), "已入队爬取")}>入队爬取</button>
      </div>
      <div class="toolbar">
        <label><input type="checkbox" checked={pauseCrawl()} onChange={(e) => setPauseCrawl(e.currentTarget.checked)} /> 重建索引时暂停爬虫</label>
        <button onClick={() => act(() => api.rebuildIndex(pauseCrawl()), "已触发重建索引")}>重建索引</button>
        <button onClick={() => act(() => api.triggerChapterCheck(), "已入队全库缺章检查")}>全库缺章检查</button>
        <button onClick={refresh}>刷新</button>
      </div>
      <Show when={issueBooks().length > 0}>
        <div class="card">
          <strong>有缺章/乱章的书（{issueBooks().length}）</strong>
          <For each={issueBooks()}>
            {(b) => (
              <p>
                <a href={`#/read/${encodeURIComponent(b.id)}`}>{b.title}</a>
                <span class="muted"> · {b.author || "未知作者"} · {b.issues} 个问题</span>
              </p>
            )}
          </For>
        </div>
      </Show>
      <Show when={error()}><p class="error">{error()}</p></Show>
      <Show when={ok()}><p class="ok">{ok()}</p></Show>

      <div class="toolbar">
        <select value={type() ?? ""} onChange={(e) => { const v = e.currentTarget.value; setType(v === "" ? undefined : (v as "scan" | "crawl")); refresh(); }}>
          <option value="">全部队列</option>
          <option value="scan">扫描队列</option>
          <option value="crawl">爬虫队列</option>
        </select>
      </div>

      <For each={tasks()}>
        {(t) => (
          <div class="card">
            <p><strong>{t.id}</strong> <span class="badge">{t.type ?? ""}</span> <span class="badge">{t.status ?? ""}</span></p>
            <p class="muted">{t.path ?? t.book_url ?? ""}</p>
            <Show when={t.error}><p class="error">{t.error}</p></Show>
            <div class="row">
              <button onClick={() => act(() => api.retryTask(t.id), "已重试")}>重试</button>
              <button onClick={() => act(() => api.pauseTask(t.id), "已暂停")}>暂停</button>
              <button onClick={() => act(() => api.resumeTask(t.id), "已恢复")}>恢复</button>
            </div>
          </div>
        )}
      </For>
      <Show when={tasks().length === 0}><p class="muted">当前队列为空。</p></Show>
    </div>
  );
};

export default Tasks;
