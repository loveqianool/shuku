import { createSignal, onMount, For, Show } from "solid-js";
import { api, type DuplicateGroup } from "../lib/api";

/** Duplicate books: same title + author. Keep one copy, delete the rest. */
const Duplicates = () => {
  const [groups, setGroups] = createSignal<DuplicateGroup[]>([]);
  const [keep, setKeep] = createSignal<Record<string, string>>({});
  const [error, setError] = createSignal("");
  const [loading, setLoading] = createSignal(false);

  // 组键：标题+作者拼起来当 key。不能用 `::` 拼接——标题里若有 `::`
  // 会撞键（《A::B》/C 与《A》/B::C 拼出来一样）；JSON 数组序列化无歧义。
  const groupKey = (g: DuplicateGroup) => JSON.stringify([g.title, g.author]);

  const load = async () => {
    setLoading(true);
    try {
      const gs = await api.listDuplicates();
      setGroups(gs);
      // Default-keep the first book of each group (most chapters, then
      // largest size — the backend already sorts that way).
      setKeep(Object.fromEntries(gs.map((g) => [groupKey(g), g.books[0]?.id ?? ""])));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };
  onMount(load);

  const pruneGroup = async (g: DuplicateGroup) => {
    const keepId = keep()[groupKey(g)];
    const doomed = g.books.filter((b) => b.id !== keepId);
    if (doomed.length === 0) return;
    if (!confirm(`删除《${g.title}》的其余 ${doomed.length} 个副本？保留的副本不受影响。`)) return;
    try {
      for (const b of doomed) await api.deleteBook(b.id);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div>
      <h2>书库去重</h2>
      <p class="muted">按书名 + 作者分组，疑似重复的书列在这里。选一个保留，删掉其余副本（书签和进度跟着副本一起删）。</p>
      <Show when={error()}><p class="error">{error()}</p></Show>
      <Show when={loading()}><p class="muted">加载中…</p></Show>
      <Show when={!loading() && groups().length === 0}>
        <p class="muted">没有重复的书。</p>
      </Show>
      <For each={groups()}>
        {(g) => (
          <div class="card">
            <strong>{g.title}</strong> <span class="muted">{g.author || "未知作者"} · {g.books.length} 个副本</span>
            <For each={g.books}>
              {(b) => (
                <label class="row">
                  <input
                    type="radio"
                    name={groupKey(g)}
                    checked={keep()[groupKey(g)] === b.id}
                    onChange={() => setKeep({ ...keep(), [groupKey(g)]: b.id })}
                  />
                  <span>{b.source === "cached" ? "缓存" : "本地"} · {b.chapter_count} 章 · {((b.size ?? 0) / 1024).toFixed(0)} KB</span>
                  <a href={`#/read/${encodeURIComponent(b.id)}`}>打开</a>
                </label>
              )}
            </For>
            <button class="primary" onClick={() => pruneGroup(g)}>删除其余副本</button>
          </div>
        )}
      </For>
    </div>
  );
};

export default Duplicates;
