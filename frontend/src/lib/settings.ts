// 阅读设置：localStorage 持久化（key: shuku.reader.settings）
// 2026-11-11 改名 legado-server -> shuku：旧键 legado.* 仍读一次并迁移。
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

const KEY = "shuku.reader.settings";
const LEGACY_KEY = "legado.reader.settings";

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
    const raw = localStorage.getItem(KEY) ?? localStorage.getItem(LEGACY_KEY);
    if (!raw) return { ...DEFAULT_SETTINGS };
    const s = { ...DEFAULT_SETTINGS, ...(JSON.parse(raw) as Partial<ReaderSettings>) };
    if (localStorage.getItem(KEY) === null) {
      try {
        localStorage.setItem(KEY, JSON.stringify(s));
        localStorage.removeItem(LEGACY_KEY);
      } catch {
        /* 忽略 */
      }
    }
    return s;
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
  return `shuku.highlights.${bookId}.${chapterIdx}`;
}

function legacyHighlightKey(bookId: string, chapterIdx: number) {
  return `legado.highlights.${bookId}.${chapterIdx}`;
}

export function loadHighlights(bookId: string, chapterIdx: number): Highlight[] {
  try {
    const raw = localStorage.getItem(highlightKey(bookId, chapterIdx))
      ?? localStorage.getItem(legacyHighlightKey(bookId, chapterIdx));
    const list = JSON.parse(raw ?? "[]") as Highlight[];
    if (raw !== null && localStorage.getItem(highlightKey(bookId, chapterIdx)) === null) {
      try {
        localStorage.setItem(highlightKey(bookId, chapterIdx), JSON.stringify(list));
        localStorage.removeItem(legacyHighlightKey(bookId, chapterIdx));
      } catch {
        /* 忽略 */
      }
    }
    return list;
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
