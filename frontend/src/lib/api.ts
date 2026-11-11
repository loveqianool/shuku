// API 层：与 docs/API.md 契约一一对应。前端只做展示与调用，
// 不做全文检索、不做爬虫 HTML 解析、不做后端排版。

export interface Page<T> {
  items: T[];
  total: number;
  page: number;
  page_size: number;
}

export interface Book {
  id: string;
  title: string;
  author?: string;
  source?: "local" | "cached";
  status?: string;
  chapter_count?: number;
  size?: number;
  has_update?: boolean;
  last_update_check?: number;
  cover?: string;
  updated_at?: string;
  tags?: string[];
  [k: string]: unknown;
}

export interface ChapterMeta {
  idx: number;
  title: string;
}

export interface Paragraph {
  text?: string;
  img?: string;
}

export interface Chapter {
  book_id: string;
  chapter_idx: number;
  title: string;
  paragraphs: Paragraph[];
}

export interface SearchHit {
  book_id: string;
  book_title: string;
  author?: string;
  chapter_idx: number;
  chapter_title: string;
  snippet: string;
  score?: number;
  match_count?: number;
}

export type SearchScope = "local" | "cached" | "all";

export interface Source {
  id?: string;
  bookSourceUrl: string;
  bookSourceName: string;
  bookSourceGroup?: string;
  bookSourceType?: number;
  searchUrl?: string;
  exploreUrl?: string;
  bookUrlPattern?: string;
  ruleSearch?: unknown;
  ruleExplore?: unknown;
  ruleBookInfo?: unknown;
  ruleToc?: unknown;
  ruleContent?: unknown;
  jsLib?: string;
  enabled?: boolean;
  weight?: number;
  lastCheckTime?: number;
  health?: string;
  healthError?: string;
  subscriptionUrl?: string;
  customOrder?: number;
  [k: string]: unknown;
}

export type TestKind = "search" | "explore" | "toc" | "content" | "login";

export interface TestLog {
  level: string;
  module: string;
  msg: string;
  ms?: number;
}

export interface TestResult {
  ok: boolean;
  elapsed_ms: number;
  raw_status?: number;
  raw_body_truncated?: string;
  extracted?: unknown;
  js_output?: unknown;
  error?: string;
  logs: TestLog[];
}

export interface PickerFetchResult {
  ok: boolean;
  status: number;
  body: string;
  elapsed_ms: number;
}

export interface TaskItem {
  id: string;
  type?: "scan" | "crawl";
  status?: string;
  path?: string;
  source_id?: string;
  book_url?: string;
  error?: string;
  created_at?: string;
  updated_at?: string;
  [k: string]: unknown;
}

export interface DuplicateGroup {
  title: string;
  author: string;
  books: Book[];
}

export interface ReadingStats {
  total_books: number;
  reading_books: number;
  finished_books: number;
  total_seconds: number;
  active_days: number;
  finished_by_month: [string, number][];
  recent_finished: [string, string, string, number][];
  top_books_by_time: [string, string, string, number][];
}

export interface ChapterIssue {
  idx: number;
  title: string;
  kind: string;
  detail: string;
}

export interface BookWithIssues {
  id: string;
  title: string;
  author: string;
  issues: number;
}

export interface Suggestion {
  id: string;
  title: string;
  author: string;
}

export interface TaskStats {
  scan_pending?: number;
  crawl_pending?: number;
  [k: string]: unknown;
}

export interface Progress {
  chapter_idx: number;
  offset: number;
}

export interface Bookmark {
  id?: string;
  chapter_idx: number;
  offset: number;
  note?: string;
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: { "Content-Type": "application/json", ...(init?.headers ?? {}) },
  });
  if (!res.ok) {
    let msg = `HTTP ${res.status}`;
    try {
      const body = await res.json();
      msg = (body as { error?: string }).error ?? (body as { message?: string }).message ?? msg;
    } catch {
      /* 保留状态码 */
    }
    throw new Error(msg);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

const qs = (o: Record<string, string | number | undefined>) =>
  "?" +
  Object.entries(o)
    .filter(([, v]) => v !== undefined && v !== "")
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`)
    .join("&");

export const api = {
  health: () => req<{ ok: boolean; version?: string }>("/api/health"),

  listBooks: (p: { q?: string; author?: string; tag?: string; sort?: string; page?: number; page_size?: number }) =>
    req<Page<Book>>("/api/books" + qs(p)),
  getBook: (id: string) => req<Book>(`/api/books/${encodeURIComponent(id)}`),
  listChapters: (id: string, page = 1, page_size = 200) =>
    req<Page<ChapterMeta>>(`/api/books/${encodeURIComponent(id)}/chapters${qs({ page, page_size })}`),
  getChapter: (id: string, idx: number) =>
    req<Chapter>(`/api/books/${encodeURIComponent(id)}/chapters/${idx}`),

  search: (q: string, scope: SearchScope = "all", page = 1, page_size = 20, bookId?: string) =>
    req<Page<SearchHit>>("/api/search" + qs({ q, scope, page, page_size, book_id: bookId })),
  suggest: (q: string) =>
    req<{ items: Suggestion[] }>("/api/suggest" + qs({ q })),

  listSources: () => req<Source[]>("/api/sources"),
  getSource: (id: string) => req<Source>(`/api/sources/${encodeURIComponent(id)}`),
  createSource: (s: Source) => req<Source>("/api/sources", { method: "POST", body: JSON.stringify(s) }),
  updateSource: (id: string, s: Source) =>
    req<Source>(`/api/sources/${encodeURIComponent(id)}`, { method: "PUT", body: JSON.stringify(s) }),
  deleteSource: (id: string) => req<void>(`/api/sources/${encodeURIComponent(id)}`, { method: "DELETE" }),
  importSources: (sources: Source[]) =>
    req<{ imported?: number }>("/api/sources/import", { method: "POST", body: JSON.stringify({ sources }) }),
  exportSources: async (): Promise<Source[]> => {
    const res = await fetch("/api/sources/export");
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return (await res.json()) as Source[];
  },
  testSource: (id: string, body: { kind: TestKind; keyword?: string; book_url?: string; chapter_url?: string; force_refresh?: boolean; page?: number; max_pages?: number }) =>
    req<TestResult>(`/api/sources/${encodeURIComponent(id)}/test`, { method: "POST", body: JSON.stringify(body) }),
  healthCheck: (id: string) =>
    req<TaskItem>(`/api/sources/${encodeURIComponent(id)}/health-check`, { method: "POST" }),
  subscribe: (url: string) =>
    req<TaskItem>("/api/sources/subscribe", { method: "POST", body: JSON.stringify({ url }) }),
  pickerFetch: (body: { url: string; keyword?: string; headers?: Record<string, string> }) =>
    req<PickerFetchResult>("/api/picker/fetch", { method: "POST", body: JSON.stringify(body) }),

  getProgress: (id: string) => req<Progress>(`/api/books/${encodeURIComponent(id)}/progress`),
  saveProgress: (id: string, p: Progress) =>
    req<Progress>(`/api/books/${encodeURIComponent(id)}/progress`, { method: "PUT", body: JSON.stringify(p) }),
  listBookmarks: (id: string) => req<Bookmark[]>(`/api/books/${encodeURIComponent(id)}/bookmarks`),
  addBookmark: (id: string, b: Bookmark) =>
    req<Bookmark>(`/api/books/${encodeURIComponent(id)}/bookmarks`, { method: "POST", body: JSON.stringify(b) }),
  deleteBookmark: (bookId: string, bmId: string) =>
    req<void>(`/api/books/${encodeURIComponent(bookId)}/bookmarks/${encodeURIComponent(bmId)}`, { method: "DELETE" }),
  listTags: (id: string) => req<string[]>(`/api/books/${encodeURIComponent(id)}/tags`),
  addTag: (id: string, tag: string) =>
    req<void>(`/api/books/${encodeURIComponent(id)}/tags`, { method: "POST", body: JSON.stringify({ tag }) }),
  deleteTag: (bookId: string, tag: string) =>
    req<void>(`/api/books/${encodeURIComponent(bookId)}/tags/${encodeURIComponent(tag)}`, { method: "DELETE" }),
  allTags: () => req<string[]>("/api/tags"),

  listTasks: (type?: "scan" | "crawl", status?: string) =>
    req<Page<TaskItem>>("/api/tasks" + qs({ type, status })),
  taskStats: () => req<TaskStats>("/api/tasks/stats"),
  triggerScan: (path?: string) => req<TaskItem>("/api/tasks/scan", { method: "POST", body: JSON.stringify({ path }) }),
  enqueueCrawl: (source_id: string, book_url: string) =>
    req<TaskItem>("/api/tasks/crawl", { method: "POST", body: JSON.stringify({ source_id, book_url }) }),
  checkUpdate: (id: string) =>
    req<TaskItem>(`/api/books/${encodeURIComponent(id)}/check-update`, { method: "POST" }),
  checkUpdatesAll: () =>
    req<{ queued?: number }>("/api/tasks/check-updates", { method: "POST" }),
  changeSource: (id: string, body: { source_id: string; book_url?: string }) =>
    req<TaskItem>(`/api/books/${encodeURIComponent(id)}/change-source`, { method: "POST", body: JSON.stringify(body) }),
  listDuplicates: () => req<DuplicateGroup[]>("/api/books/duplicates"),
  deleteBook: (id: string) =>
    req<{ deleted: boolean }>(`/api/books/${encodeURIComponent(id)}`, { method: "DELETE" }),
  addReadingTime: (id: string, seconds: number) =>
    req<{ ok: boolean }>(`/api/books/${encodeURIComponent(id)}/reading-time`, { method: "POST", body: JSON.stringify({ seconds }) }),
  readingStats: () => req<ReadingStats>("/api/stats"),
  checkChapterIssues: (id: string) =>
    req<{ issues: ChapterIssue[] }>(`/api/books/${encodeURIComponent(id)}/chapter-issues`),
  listBooksWithChapterIssues: () =>
    req<{ books: BookWithIssues[] }>("/api/books/chapter-issues"),
  triggerChapterCheck: () =>
    req<TaskItem>("/api/tasks/chapter-check", { method: "POST" }),
  retryTask: (id: string) => req<void>(`/api/tasks/${encodeURIComponent(id)}/retry`, { method: "POST" }),
  pauseTask: (id: string) => req<void>(`/api/tasks/${encodeURIComponent(id)}/pause`, { method: "POST" }),
  resumeTask: (id: string) => req<void>(`/api/tasks/${encodeURIComponent(id)}/resume`, { method: "POST" }),
  rebuildIndex: (pause_crawl: boolean) =>
    req<{ ok?: boolean }>(`/api/tasks/rebuild-index`, { method: "POST", body: JSON.stringify({ pause_crawl }) }),
};
