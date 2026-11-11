import { createSignal, For, Show, type Component } from "solid-js";
import { api, type Source, type TestKind, type TestResult } from "../lib/api";

const SourceTest: Component<{ sourceId: string }> = (props) => {
  const [source, setSource] = createSignal<Source | null>(null);
  const [kind, setKind] = createSignal<TestKind>("search");
  const [keyword, setKeyword] = createSignal("");
  const [bookUrl, setBookUrl] = createSignal("");
  const [chapterUrl, setChapterUrl] = createSignal("");
  const [forceRefresh, setForceRefresh] = createSignal(false);
  const [page, setPage] = createSignal(1);
  const [maxPages, setMaxPages] = createSignal(1);
  const [result, setResult] = createSignal<TestResult | null>(null);
  const [running, setRunning] = createSignal(false);
  const [error, setError] = createSignal("");

  api.getSource(props.sourceId).then((s) => {
    setSource(s);
    // checkKeyWord is the source's default search keyword (plain string,
    // not a selector): prefill the keyword box when it is empty.
    const kw = (s.ruleSearch as Record<string, unknown> | null | undefined)?.checkKeyWord;
    if (typeof kw === "string" && kw.trim() !== "" && keyword() === "") setKeyword(kw);
  }).catch((e) => setError(String(e)));

  const run = async () => {
    setRunning(true);
    setError("");
    setResult(null);
    try {
      const r = await api.testSource(props.sourceId, {
        kind: kind(),
        keyword: keyword() || undefined,
        book_url: bookUrl() || undefined,
        chapter_url: chapterUrl() || undefined,
        force_refresh: forceRefresh(),
        page: kind() === "search" || kind() === "explore" ? page() : undefined,
        max_pages: kind() === "search" ? maxPages() : undefined,
      });
      setResult(r);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setRunning(false);
    }
  };

  const r = () => result();

  return (
    <div>
      <a href="#/sources">‹ 返回书源列表</a>
      <h2>书源测试{source() ? ` · ${source()!.bookSourceName}` : ""}</h2>
      <p class="muted">测试请求优先级高于后台爬虫；force_refresh 绕过三级缓存。</p>
      <Show when={error()}><p class="error">{error()}</p></Show>

      <div class="toolbar">
        <select value={kind()} onChange={(e) => setKind(e.currentTarget.value as TestKind)}>
          <option value="search">搜索 search</option>
          <option value="explore">发现 explore</option>
          <option value="toc">目录 toc</option>
          <option value="content">正文 content</option>
          <option value="login">登录 login</option>
        </select>
        <Show when={kind() === "search" || kind() === "explore"}>
          <input type="number" min="1" placeholder="起始页" value={page()} onInput={(e) => setPage(Number(e.currentTarget.value) || 1)} style={{ width: "5em" }} />
        </Show>
        <Show when={kind() === "search"}>
          <input type="number" min="1" max="10" placeholder="页数" value={maxPages()} onInput={(e) => setMaxPages(Number(e.currentTarget.value) || 1)} style={{ width: "5em" }} />
          <input placeholder="keyword（搜索用）" value={keyword()} onInput={(e) => setKeyword(e.currentTarget.value)} />
        </Show>
        <Show when={kind() === "toc"}>
          <input placeholder="book_url（目录用）" value={bookUrl()} onInput={(e) => setBookUrl(e.currentTarget.value)} />
        </Show>
        <Show when={kind() === "content"}>
          <input placeholder="chapter_url（正文用）" value={chapterUrl()} onInput={(e) => setChapterUrl(e.currentTarget.value)} />
        </Show>
        <label><input type="checkbox" checked={forceRefresh()} onChange={(e) => setForceRefresh(e.currentTarget.checked)} /> 强制刷新（绕过缓存）</label>
        <button class="primary" onClick={run} disabled={running()}>{running() ? "测试中…" : "运行测试"}</button>
      </div>

      <Show when={r()}>
        <div class="card">
          <p>
            结果：<span class={r()!.ok ? "ok" : "error"}>{r()!.ok ? "成功" : "失败"}</span>
            {" · "}耗时 {r()!.elapsed_ms} ms
            <Show when={r()!.raw_status != null}>{" · "}HTTP {r()!.raw_status}</Show>
          </p>
          <Show when={r()!.error}><p class="error">错误：{r()!.error}</p></Show>
        </div>

        <Show when={r()!.logs?.length}>
          <h3>结构化日志</h3>
          <table class="logs">
            <thead><tr><th>级别</th><th>模块</th><th>消息</th><th>ms</th></tr></thead>
            <tbody>
              <For each={r()!.logs}>
                {(l) => <tr><td>{l.level}</td><td>{l.module}</td><td>{l.msg}</td><td>{l.ms ?? ""}</td></tr>}
              </For>
            </tbody>
          </table>
        </Show>

        <h3>提取结果 extracted</h3>
        <pre>{JSON.stringify(r()!.extracted, null, 2) ?? "(空)"}</pre>

        <Show when={r()!.js_output != null}>
          <h3>JS 中间输出 js_output</h3>
          <pre>{JSON.stringify(r()!.js_output, null, 2)}</pre>
        </Show>

        <Show when={r()!.raw_body_truncated}>
          <h3>原始响应（截断） raw_body_truncated</h3>
          <pre>{r()!.raw_body_truncated}</pre>
        </Show>
      </Show>
    </div>
  );
};

export default SourceTest;
