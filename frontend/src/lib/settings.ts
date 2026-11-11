// 阅读设置：localStorage 持久化（key: legado.reader.settings）
export type ReaderMode = "scroll" | "page";
export type ReaderTheme = "light" | "dark" | "sepia";
export type ReaderScript = "orig" | "trad" | "simp";

export interface ReaderSettings {
  fontSize: number; // px
  lineHeight: number;
  indent: number; // em 首行缩进
  theme: ReaderTheme;
  mode: ReaderMode;
  script: ReaderScript;
}

const KEY = "legado.reader.settings";

export const DEFAULT_SETTINGS: ReaderSettings = {
  fontSize: 18,
  lineHeight: 1.9,
  indent: 2,
  theme: "dark",
  mode: "scroll",
  script: "orig",
};

export function loadSettings(): ReaderSettings {
  try {
    const raw = localStorage.getItem(KEY);
    if (!raw) return { ...DEFAULT_SETTINGS };
    return { ...DEFAULT_SETTINGS, ...(JSON.parse(raw) as Partial<ReaderSettings>) };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}

export function saveSettings(s: ReaderSettings) {
  try {
    localStorage.setItem(KEY, JSON.stringify(s));
  } catch {
    /* 存储满/禁用时静默失败 */
  }
}

// 批注高亮：按书+章节存选中的原文片段与备注（localStorage）
export interface Highlight {
  text: string;
  note?: string;
  created_at: number;
}

export function highlightKey(bookId: string, chapterIdx: number) {
  return `legado.highlights.${bookId}.${chapterIdx}`;
}

export function loadHighlights(bookId: string, chapterIdx: number): Highlight[] {
  try {
    return JSON.parse(localStorage.getItem(highlightKey(bookId, chapterIdx)) ?? "[]") as Highlight[];
  } catch {
    return [];
  }
}

export function saveHighlights(bookId: string, chapterIdx: number, list: Highlight[]) {
  try {
    localStorage.setItem(highlightKey(bookId, chapterIdx), JSON.stringify(list));
  } catch {
    /* 忽略 */
  }
}
