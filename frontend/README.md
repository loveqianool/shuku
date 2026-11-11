# legado-server 前端（SolidJS + Vite + PWA）

静态阅读器：书架 / 全文检索 / 阅读 / 书源管理与测试 / 任务。**不是主服务**，
构建产物 `dist/` 最终由 Rust 二进制 `include_bytes!` 嵌入，随单 binary 分发。

## 构建

```bash
cd frontend
npm install
npm run build   # 产物在 frontend/dist
npm run dev     # 开发：http://localhost:5173，/api 代理到 127.0.0.1:8080
```

要求 Node ≥ 20。`node_modules/` 与 `dist/` 不提交。

## 约定

- API 基地址：同源相对路径 `/api/...`（与 `docs/API.md` 契约一致，勿改）。
- 路由用 hash（`#/read/:bookId/:idx` 等），Rust 嵌入式静态服务无需 SPA fallback。
- PWA：`vite-plugin-pwa`，Service Worker 只预缓存构建静态资源 +
  最近 20 章正文（`recent-chapters`，7 天）与最近书目元数据；**绝不缓存全书库、
  搜索结果、书源列表**。离线时书架/最近章节可读。
- 阅读设置（字号/行距/缩进/主题/滚动·翻页）与批注高亮存 `localStorage`。
- 前端不做全文检索、不解析爬虫 HTML、不做后端排版渲染。
