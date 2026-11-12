// 整本离线：把一本书的全部章节（及封面/目录）预取进 Service Worker
// 的缓存（workbox `recent-chapters` / `book-images` / `recent-book-meta`），
// 离线时整本可读。缓存是共享的（上限 5000 章、30 天），所以状态以
// Cache Storage 里实际命中的章节数为准，不用本地标记。

const CHAPTER_CACHE = "recent-chapters";
const IMAGE_CACHE = "book-images";
const META_CACHE = "recent-book-meta";

const chapterUrl = (bookId: string, idx: number) => `/api/books/${encodeURIComponent(bookId)}/chapters/${idx}`;

/** 已缓存的章节数（实际查 Cache Storage）。无 SW/无缓存时返回 0。 */
export async function offlineChapterCount(bookId: string, total: number): Promise<number> {
  try {
    const cache = await caches.open(CHAPTER_CACHE);
    const keys = await cache.keys();
    const prefix = `/api/books/${encodeURIComponent(bookId)}/chapters/`;
    let n = 0;
    for (const req of keys) {
      const path = new URL(req.url).pathname;
      if (path.startsWith(prefix)) {
        const idx = Number(path.slice(prefix.length));
        if (Number.isInteger(idx) && idx >= 0 && idx < total) n += 1;
      }
    }
    return n;
  } catch {
    return 0;
  }
}

/** 下载整本：顺序预取全部章节（4 并发），另取书详情/目录/封面。 */
export async function downloadBook(
  bookId: string,
  total: number,
  onProgress: (done: number) => void,
): Promise<void> {
  // 书详情、目录、封面也进缓存（走各自的 workbox 规则）。
  await Promise.allSettled([
    fetch(`/api/books/${encodeURIComponent(bookId)}`),
    fetch(`/api/books/${encodeURIComponent(bookId)}/chapters?page=1&page_size=1`),
    fetch(`/api/books/${encodeURIComponent(bookId)}/cover`),
  ]);
  let done = 0;
  let next = 0;
  const worker = async () => {
    while (next < total) {
      const idx = next;
      next += 1;
      try {
        // 失败的章节不计入 done（以前 catch 后照样 +1，进度到 100%
        // 其实有章节没下下来）；15 秒超时，免得一章挂住拖死整批。
        await fetch(chapterUrl(bookId, idx), { signal: AbortSignal.timeout(15000) });
        done += 1;
      } catch {
        // 离线/失败的章节跳过，其余继续；已缓存数以实际为准。
      }
      onProgress(done);
    }
  };
  await Promise.all([worker(), worker(), worker(), worker()]);
}

/** 删除一本的离线缓存（章节 + 封面 + 书详情/目录）。 */
export async function deleteOfflineBook(bookId: string, total: number): Promise<void> {
  try {
    const [chapters, images, meta] = await Promise.all([
      caches.open(CHAPTER_CACHE),
      caches.open(IMAGE_CACHE),
      caches.open(META_CACHE),
    ]);
    const base = `/api/books/${encodeURIComponent(bookId)}`;
    for (let i = 0; i < total; i++) await chapters.delete(chapterUrl(bookId, i));
    const imgKeys = await images.keys();
    for (const req of imgKeys) {
      if (new URL(req.url).pathname.startsWith(`${base}/`)) await images.delete(req);
    }
    const metaKeys = await meta.keys();
    for (const req of metaKeys) {
      const path = new URL(req.url).pathname;
      if (path === base || path.startsWith(`${base}/`)) await meta.delete(req);
    }
  } catch {
    /* 无缓存时忽略 */
  }
}
