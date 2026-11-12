import { defineConfig } from "vite";
import solid from "vite-plugin-solid";
import { VitePWA } from "vite-plugin-pwa";

export default defineConfig({
  plugins: [
    solid(),
    VitePWA({
      registerType: "autoUpdate",
      includeAssets: ["favicon.svg", "icons/icon-192.png", "icons/icon-512.png"],
      manifest: {
        name: "书库",
        short_name: "书库",
        description: "自托管小说服务 · 对标阅读3服务器版",
        start_url: "/",
        display: "standalone",
        background_color: "#121212",
        theme_color: "#121212",
        icons: [
          { src: "/icons/icon-192.png", sizes: "192x192", type: "image/png" },
          { src: "/icons/icon-512.png", sizes: "512x512", type: "image/png" },
          { src: "/icons/icon-512.png", sizes: "512x512", type: "image/png", purpose: "maskable" },
        ],
      },
      workbox: {
        // 仅预缓存构建产物（静态资源）；绝不预缓存书库/搜索结果。
        // 繁简词典（cn2t/t2cn）不预缓存，只在用户第一次切换繁简时按需加载。
        globPatterns: ["**/*.{js,css,html,ico,png,svg,woff2}"],
        globIgnores: ["**/cn2t-*.js", "**/t2cn-*.js"],
        navigateFallback: "/index.html",
        runtimeCaching: [
          {
            // 章节正文：NetworkFirst（2026-11-12 由 CacheFirst 改）。
            // 原因：后端排版规则会变（如 vvsolo 断行合并），CacheFirst 会让
            // 用户 30 天内一直吃旧版章節（未合併的短行），再配 justify 就
            // 出大空格。有网时永远取新版、离线或 3 秒无响应才回落缓存。
            // 整本离线靠阅读器的「下载整本」把全书章节预取进这个缓存；
            // 上限 5000 章、30 天（约几十本全本）。
            urlPattern: ({ url }) => /\/api\/books\/[^/]+\/chapters\/\d+$/.test(url.pathname),
            handler: "NetworkFirst",
            options: {
              cacheName: "recent-chapters",
              networkTimeoutSeconds: 3,
              expiration: { maxEntries: 5000, maxAgeSeconds: 30 * 24 * 3600 },
              cacheableResponse: { statuses: [200] },
            },
          },
          {
            // 封面与 epub 插图：离线阅读时也要能显示。
            urlPattern: ({ url }) =>
              /\/api\/books\/[^/]+\/cover$/.test(url.pathname) ||
              /\/api\/books\/[^/]+\/images\/.+$/.test(url.pathname),
            handler: "CacheFirst",
            options: {
              cacheName: "book-images",
              expiration: { maxEntries: 1000, maxAgeSeconds: 30 * 24 * 3600 },
              cacheableResponse: { statuses: [200] },
            },
          },
          {
            // 书籍详情/目录：短缓存，离线可读最近看过的书。
            urlPattern: ({ url }) =>
              /\/api\/books\/[^/]+$/.test(url.pathname) ||
              /\/api\/books\/[^/]+\/chapters$/.test(url.pathname),
            handler: "StaleWhileRevalidate",
            options: {
              cacheName: "recent-book-meta",
              expiration: { maxEntries: 30, maxAgeSeconds: 24 * 3600 },
              cacheableResponse: { statuses: [200] },
            },
          },
        ],
      },
      devOptions: { enabled: false },
    }),
  ],
  server: {
    port: 5173,
    proxy: {
      "/api": { target: "http://127.0.0.1:8080", changeOrigin: true },
    },
  },
  build: { target: "es2020", outDir: "dist", sourcemap: false },
});
