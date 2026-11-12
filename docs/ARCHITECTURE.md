# 书库 Shuku — 自托管小说服务（对标阅读3服务器版）

## 定位
NAS / 低功耗 x86 / ARM 常驻服务，单静态二进制（musl），前端 dist 由 `include_bytes!` 嵌入。
两大核心：本地十几万本书库；兼容阅读3 JSON 书源引擎。

## 技术栈（固定，不可改）
- 后端：Rust + Tokio + Axum
- 存储：SQLite（rusqlite + tokio-rusqlite），仅元数据/任务/书源/缓存/进度；正文不进 SQLite
- 检索：tantivy + tantivy-jieba，文档粒度=章节
  - schema 固定字段：`book_id`(fast)、`book_title`(indexed)、`author`(indexed)、`chapter_title`(indexed)、`content`(indexed, snippet 高亮)、`chapter_idx`(u64, fast + stored)、`pinyin`(indexed，书名+作者+章节名拼音，空格分隔与连写两种形式，仅元数据不含正文)
- 电子书解析：epub-rs、chardetng 自动编码识别
- 本地扫描：notify + walkdir，增量靠 mtime + 索引版本标记
- 书源运行时：reqwest（域名隔离连接池、HTTP/2、hickory-dns 缓存、代理支持）、scraper(html5ever)、regex 预编译缓存、rquickjs JS 沙箱（仅字符串清洗，禁网络/文件/定时器；Runtime 对象池、字节码缓存、单脚本超时+内存熔断；selector/regex 预编译，不在运行时编译）
- 调度：两套独立 SQLite 表驱动持久化队列（scan_tasks / crawl_tasks）；独立令牌桶、域名限流、失败熔断冷却；书源测试请求优先级高于后台爬虫
- 前端 PWA：SolidJS + Vite + vite-plugin-pwa；TanStack Virtual 虚拟滚动；CodeMirror 6 书源 JSON 编辑器
- 阅读页：前端 JS+CSS 滚动/仿真翻页、字号/间距/缩进/主题、图片懒加载、批注高亮；后端输出标准化段落数组 `[{text, img}]`
- SW 策略：仅缓存静态资源 + 最近阅读章节；整本离线是用户在阅读器里主动点的例外（走同一章节缓存，上限 5000 章/30 天，全库共享），绝不默认缓存全书库

## 禁止清单（硬约束）
- 禁 Elasticsearch / Meilisearch / Bleve / Sonic / Lucene
- 禁 Node 作为主服务
- 禁后端做分页排版渲染
- 禁前端承担主全文检索、爬虫 HTML 解析
- 禁默认多服务/多容器架构；前端 dist 最终 include_bytes 嵌入 Rust 二进制
- 禁直接用 SQLite LIKE 做全文搜索

## 数据库表（11 张：9 张见 migrations/001_init.sql，另 2 张运行时建）

`books, chapters, sources, source_cache, scan_tasks, crawl_tasks, bookmarks, reading_progress, tags`（001_init.sql）；`reading_seconds`（阅读时长累计）与 `chapter_issues`（缺章/乱章检查结果）不在 001 里，由 `Database::migrate()` 用 `CREATE TABLE IF NOT EXISTS` 在运行时创建。另有若干幂等 `ALTER TABLE ADD COLUMN`（先查 `pragma_table_info` 再补列）。

## 性能规则（自测必覆盖）
1. reqwest 域名隔离连接池、DNS 缓存；三级缓存：搜索短 TTL、目录中 TTL、章节长 TTL；测试面板支持强制刷新绕过缓存
2. Html 一次解析复用；selector、regex 预编译，书源保存时校验合法性
3. HTML 解析、rquickjs 执行全部进 spawn_blocking，严禁阻塞 tokio IO worker
4. 域名独立令牌桶，连续失败熔断冷却
5. 书籍增量扫描，mtime + 索引版本，坏书单独标记不阻断队列
6. 扫描索引队列、爬虫队列资源隔离，重建索引时可暂停爬虫

## 模块划分（crates/server/src）
- `db/` — rusqlite 连接池 + migrations + 11 表 CRUD
- `search/` — tantivy 索引：建索引、增量更新（按 book_id 删除重建）、检索+snippet 高亮
- `scanner/` — walkdir 全量/增量扫描 + notify 监听 + mtime 对比
- `parser/` — epub（epub-rs）/ txt（chardetng + 章节切分正则）→ 章节流；坏书标记
- `source/` — 书源模型（阅读3 JSON）、规则编译（selector/regex 预编译+校验）、reqwest 域名池、scraper 提取、rquickjs 沙箱池
- `scheduler/` — 双队列 worker（scan / crawl）、令牌桶、熔断器、三级缓存
- `cache/` — 三级 TTL 缓存（内存 + source_cache 表）
- `api/` — Axum 路由 +  handlers + 测试接口结构化调试日志

## REST 契约（见 docs/API.md）
- 书架：GET /api/books（分页/筛选/排序）、GET /api/books/:id、GET /api/books/:id/chapters、GET /api/books/:id/chapters/:idx（段落数组）、GET /api/books/recent（最近阅读 100 本）、GET /api/books/:id/matches（本书内逐处匹配）、GET /api/books/duplicates（疑似重复）、GET /api/books/:id/cover、GET /api/books/:id/images/*path
- 检索：GET /api/search?q=&scope=local|cached|all&page=（scope 走索引 `source` 字段过滤，不再查 SQLite 取 ID）、GET /api/suggest（联想，最多 8 条，走 tantivy `search_book_ids` + 全文补足）；书库列表 `GET /api/books?q=` 也走 tantivy（`search_book_ids`）再按 ID 取行
- 书源：CRUD /api/sources、POST /api/sources/import、GET /api/sources/export、POST /api/sources/:id/health-check、POST /api/sources/subscribe
- 测试：POST /api/sources/:id/test {kind: search|explore|toc|content|login, ...} → {raw, extracted, js_output, elapsed_ms, error, logs[]}（`explore` 跑 exploreUrl + ruleExplore 不需要 keyword；`login` 单跑 loginUrl + loginCheckJs，配了 loginUi 先走 DOM 填表）；POST /api/picker/fetch（规则拾取器代抓）
- 进度/书签/标签：reading_progress、bookmarks、tags CRUD；POST /api/books/:id/reading-time
- 任务：GET /api/tasks、POST /api/tasks/scan、POST /api/tasks/crawl、POST /api/tasks/:id/retry、POST /api/tasks/:id/pause、POST /api/tasks/rebuild-index、POST /api/tasks/check-updates、POST /api/tasks/chapter-check、GET /api/tasks/stats
- 统计：GET /api/stats；OPDS：GET /opds/（导航）、/opds/books、/opds/recent、/opds/authors、/opds/search、/opds/download/:id

## 部署
- `cargo build --release --target x86_64-unknown-linux-musl`
- 环境变量：`SHUKU_HOME`（默认 `.`，书库在 `$SHUKU_HOME/books`、数据在 `$SHUKU_HOME/data`）、`SHUKU_BOOKS_DIR` / `SHUKU_DATA_DIR`（单独覆盖，优先于 `SHUKU_HOME`）、`SHUKU_ADDR`（默认 0.0.0.0:8080）
- 单 binary + data 目录（sqlite + tantivy index）即可运行，无外部依赖。
