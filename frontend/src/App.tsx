import { createSignal, onCleanup, Show, type Component } from "solid-js";
import Bookshelf from "./pages/Bookshelf";
import SearchPage from "./pages/SearchPage";
import Reader from "./pages/Reader";
import Sources from "./pages/Sources";
import Duplicates from "./pages/Duplicates";
import Stats from "./pages/Stats";
import SourceTest from "./pages/SourceTest";
import Picker from "./pages/Picker";
import Tasks from "./pages/Tasks";

export interface Route {
  name: "books" | "search" | "read" | "sources" | "test" | "picker" | "tasks" | "duplicates" | "stats";
  bookId?: string;
  chapterIdx?: number;
  sourceId?: string;
}

function parseHash(): Route {
  const h = location.hash.replace(/^#\/?/, "");
  const parts = h.split("/").filter(Boolean);
  if (parts[0] === "search") return { name: "search" };
  if (parts[0] === "read" && parts[1])
    return { name: "read", bookId: decodeURIComponent(parts[1]), chapterIdx: parts[2] ? Number(parts[2]) : 0 };
  if (parts[0] === "sources" && parts[2] === "test")
    return { name: "test", sourceId: decodeURIComponent(parts[1]) };
  if (parts[0] === "sources" && parts[2] === "pick")
    return { name: "picker", sourceId: decodeURIComponent(parts[1]) };
  if (parts[0] === "picker") return { name: "picker" };
  if (parts[0] === "sources") return { name: "sources" };
  if (parts[0] === "duplicates") return { name: "duplicates" };
  if (parts[0] === "stats") return { name: "stats" };
  if (parts[0] === "tasks") return { name: "tasks" };
  return { name: "books" };
}

const App: Component = () => {
  const [route, setRoute] = createSignal<Route>(parseHash());
  const [dark, setDark] = createSignal(
    (localStorage.getItem("legado.ui.theme") ?? "dark") === "dark",
  );
  const onHash = () => setRoute(parseHash());
  window.addEventListener("hashchange", onHash);
  onCleanup(() => window.removeEventListener("hashchange", onHash));

  const applyTheme = (d: boolean) => {
    setDark(d);
    document.documentElement.dataset.theme = d ? "dark" : "light";
    localStorage.setItem("legado.ui.theme", d ? "dark" : "light");
  };
  document.documentElement.dataset.theme = dark() ? "dark" : "light";

  const link = (href: string, name: string) => (
    <a href={href} class={route().name === name ? "active" : ""}>
      {href === "#/" && name === "books" ? "书架" : name === "search" ? "检索" : name === "sources" ? "书源" : name === "tasks" ? "任务" : name}
    </a>
  );

  return (
    <div class="app">
      <nav class="nav">
        <span class="brand">📚 legado</span>
        <a href="#/" class={route().name === "books" ? "active" : ""}>书架</a>
        <a href="#/search" class={route().name === "search" ? "active" : ""}>检索</a>
        <a href="#/sources" class={route().name === "sources" || route().name === "test" ? "active" : ""}>书源</a>
        <a href="#/picker" class={route().name === "picker" ? "active" : ""}>规则拾取</a>
        <a href="#/duplicates" class={route().name === "duplicates" ? "active" : ""}>去重</a>
        <a href="#/stats" class={route().name === "stats" ? "active" : ""}>统计</a>
        <a href="#/tasks" class={route().name === "tasks" ? "active" : ""}>任务</a>
        <button class="theme-toggle" onClick={() => applyTheme(!dark())}>
          {dark() ? "☀️ 浅色" : "🌙 深色"}
        </button>
      </nav>
      <Show when={!navigator.onLine}>
        <div class="offline-note">离线状态：仅可查看已缓存的静态资源与最近阅读章节。</div>
      </Show>
      <Show when={route().name === "books"}><Bookshelf /></Show>
      <Show when={route().name === "search"}><SearchPage /></Show>
      <Show when={route().name === "read" && route().bookId}>
        <Reader bookId={route().bookId!} initialIdx={route().chapterIdx ?? 0} />
      </Show>
      <Show when={route().name === "sources"}><Sources /></Show>
      <Show when={route().name === "test" && route().sourceId}>
        <SourceTest sourceId={route().sourceId!} />
      </Show>
      <Show when={route().name === "picker"}>
        <Picker sourceId={route().sourceId} />
      </Show>
      <Show when={route().name === "tasks"}><Tasks /></Show>
      <Show when={route().name === "duplicates"}><Duplicates /></Show>
      <Show when={route().name === "stats"}><Stats /></Show>
    </div>
  );
};

export default App;
