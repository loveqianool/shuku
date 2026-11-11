import { createSignal, onMount, For, Show } from "solid-js";
import { api, type ReadingStats } from "../lib/api";

const fmtDuration = (seconds: number) => {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (h > 0) return `${h} 小时 ${m} 分`;
  if (m > 0) return `${m} 分钟`;
  return `${seconds} 秒`;
};

const fmtDate = (ts: number) => new Date(ts * 1000).toLocaleDateString();

const Stats = () => {
  const [stats, setStats] = createSignal<ReadingStats | null>(null);
  const [error, setError] = createSignal("");
  onMount(() => {
    api.readingStats().then(setStats).catch((e) => setError(e instanceof Error ? e.message : String(e)));
  });

  return (
    <div>
      <h2>阅读统计</h2>
      <Show when={error()}><p class="error">{error()}</p></Show>
      <Show when={stats()}>
        {(s) => (
          <>
            <p class="muted">
              藏书 {s().total_books} 本 · 在读 {s().reading_books} 本 · 读完 {s().finished_books} 本 ·
              累计阅读 {fmtDuration(s().total_seconds)} · 活跃 {s().active_days} 天
            </p>
            <p class="muted">读完以读到最后一章为准；时长只统计阅读器打开且页面可见的时间。</p>

            <Show when={s().finished_by_month.length > 0}>
              <div class="card">
                <strong>每月读完</strong>
                <For each={s().finished_by_month}>
                  {([month, count]) => (
                    <p>{month}：{count} 本</p>
                  )}
                </For>
              </div>
            </Show>

            <Show when={s().recent_finished.length > 0}>
              <div class="card">
                <strong>最近读完</strong>
                <For each={s().recent_finished}>
                  {([id, title, author, ts]) => (
                    <p>
                      <a href={`#/read/${encodeURIComponent(id)}`}>{title}</a>
                      <span class="muted"> · {author || "未知作者"} · {fmtDate(ts)}</span>
                    </p>
                  )}
                </For>
              </div>
            </Show>

            <Show when={s().top_books_by_time.length > 0}>
              <div class="card">
                <strong>花时间最多的书</strong>
                <For each={s().top_books_by_time}>
                  {([id, title, author, seconds]) => (
                    <p>
                      <a href={`#/read/${encodeURIComponent(id)}`}>{title}</a>
                      <span class="muted"> · {author || "未知作者"} · {fmtDuration(seconds)}</span>
                    </p>
                  )}
                </For>
              </div>
            </Show>
          </>
        )}
      </Show>
    </div>
  );
};

export default Stats;
