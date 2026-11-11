# REST 契约

基础：`GET /api/health` → `{ok:true, version}`；所有列表默认 `page=1&page_size=20`，返回 `{items, total, page, page_size}`。

## 书库
- `GET /api/books?q=&author=&tag=&sort=updated|title|author&page=&page_size=` — 分页筛选；q 同时匹配书名拼音（音节/连写/首字母，如 `wlcl` 命中《武林沉沦》）
- `GET /api/books/recent` — 最近阅读：最近打开的 100 本书（按阅读进度更新时间倒序；`{items:[{...书目, last_read_at, last_chapter_idx}]}`，点开续读上次章节）
- `GET /api/books/:id` — 书籍详情（含章节数、来源 local|cached、状态）
- `GET /api/books/:id/chapters?page=&page_size=200` — 目录（仅标题+索引，不含正文）
- `GET /api/books/:id/chapters/:idx` — 正文：`{book_id, chapter_idx, title, paragraphs:[{text?:string, img?:string}]}`；本地书从文件按需解析，网络书走缓存/爬取；epub 章节里的 `<img>` 变成 `img` 段落（URL 形如 `/api/books/:id/images/<epub 内路径>`）
- `GET /api/books/:id/images/*path` — 取 epub 内嵌图片的字节（`path` 是图片在 epub 里的路径，如 `OEBPS/images/pic.jpg`）；仅 epub 书可用，`..` 拒绝，Content-Type 按 epub 内 mime
- `GET /api/books/:id/cover` — epub 封面字节（Content-Type 为封面 mime；无封面 404）
- `DELETE /api/books/:id` — 删书（DB 行级联清章节/书签/进度/标签；清索引；删章节文件与封面）
- `GET /api/books/duplicates` — 疑似重复：`[{title, author, books:[...]}]`（同书名+同作者，组内按章节数/大小排）
- `POST /api/books/:id/check-update` / `POST /api/tasks/check-updates` — 检查缓存书更新（有新章则追加并重建索引，`has_update` 置位）
- `POST /api/books/:id/change-source` `{source_id, book_url?}` — 换源（book id 不变，进度/书签保留）
- `GET /api/books/:id/chapter-issues` — 本书缺章/乱章即时检查（序号不连续、重复、空章节）
- `GET /api/books/chapter-issues` — 全库有问题的书（需先跑 `POST /api/tasks/chapter-check` 或单本检查过）
- `POST /api/books/:id/reading-time` `{seconds}` — 阅读器上报阅读时长（单次上限 3600 秒）

## 全文检索
- `GET /api/search?q=&scope=local|cached|all&book_id=&page=&page_size=20` — `book_id` 限定某本书内搜索
- 返回章节级命中：`{book_id, book_title, author, chapter_idx, chapter_title, snippet（高亮 <em>…</em>）, score}`；scope=local 只查本地书，cached 只查已缓存网络书，all 两者
- 检索走 tantivy，禁止 SQLite LIKE；索引含 `pinyin` 字段（书名+作者+章节名拼音，空格分隔与连写两种形式），查询侧把拉丁字母按音节切分（`xiuxian` → `xiu xian`），所以打拼音能找到汉字书名/作者；索引版本 2（老库下次扫描自动重建）
- `GET /api/suggest?q=` — 搜索联想：书名/作者子串匹配（书名优先）+ 全文索引补足（覆盖拼音），最多 8 条 `{id, title, author}`

## 阅读统计
- `GET /api/stats` — `{total_books, reading_books, finished_books（读到最后一章）, total_seconds（阅读器上报的累计时长）, active_days, finished_by_month[[月, 本数]], recent_finished[[id, title, author, ts]], top_books_by_time[[id, title, author, seconds]]}`

## OPDS（供 KOReader / 静读天下等外部阅读器）
- `GET /opds/` — 导航：全部图书 / 按作者 / 最新入库
- `GET /opds/books?page=` / `GET /opds/recent?page=` — 采编 feed（50 本/页，分页链接 first/previous/next/last）
- `GET /opds/authors` / `GET /opds/authors/:author` — 按作者导航与采编 feed
- `GET /opds/search?q=` — OpenSearch 搜索；`GET /opds/opensearch.xml` — 描述文档
- `GET /opds/download/:id` — 下书：本地书直接给原文件（epub/txt），缓存书由章节文件拼成 txt（去掉 epub 图片标记行）

## 书源
- `GET /api/sources` / `POST /api/sources` / `GET /api/sources/:id` / `PUT /api/sources/:id` / `DELETE /api/sources/:id`
- `POST /api/sources/import` `{sources:[...]}` / `GET /api/sources/export` — 阅读3 JSON 格式
- 书源字段（阅读3 JSON 子集）：`bookSourceUrl, bookSourceName, bookSourceGroup, bookSourceType, searchUrl, exploreUrl, bookUrlPattern, ruleSearch, ruleExplore, ruleBookInfo, ruleToc, ruleContent, jsLib?, enabled, weight, lastCheckTime, customOrder`
- 健康：`health`/`healthError`/`lastCheckTime`（定时跑 `ruleSearch.checkKeyWord` 或 explore 探针，默认每天；`POST /api/sources/:id/health-check` 手动触发）
- 订阅：`POST /api/sources/subscribe` `{url}` — 订阅 URL（http(s)）定期刷新规则（与健康检查同一周期）；源上记 `subscriptionUrl`

## 书源测试（优先级高于后台爬虫）
- `POST /api/sources/:id/test` `{kind:"search"|"explore"|"toc"|"content"|"login", keyword?, book_url?, chapter_url?, force_refresh?:bool, page?:int, max_pages?:int(1..10)}` — search 支持多页（`max_pages`，空页即停）；`explore` 跑 exploreUrl + ruleExplore（不需要 keyword）；`login` 单独跑 loginUrl + loginCheckJs（配了 loginUi 则先走 DOM 填表提交）
- `POST /api/picker/fetch` `{url, keyword?, headers?}` — 规则拾取器代抓一页（url 是 searchUrl 同款模板，支持 POST/`{key}`/`{{key}}`；返回 `{ok, status, body, elapsed_ms}`；正文超 2MB 拒绝；只接受 http(s)）
- 返回 `{ok, elapsed_ms, raw_status, raw_body_truncated, extracted, js_output, error, logs:[{level, module, msg, ms}]}`；`logs` 供前端面板展示；`force_refresh=true` 绕过三级缓存

## 进度 / 书签 / 标签
- `GET/PUT /api/books/:id/progress` `{chapter_idx, offset}`（PUT 幂等）
- `GET/POST/DELETE /api/books/:id/bookmarks` `{chapter_idx, offset, note?}`
- `GET/POST/DELETE /api/books/:id/tags`；`GET /api/tags`

## 任务
- `GET /api/tasks?type=scan|crawl&status=` — 两套队列分别查
- `POST /api/tasks/scan` `{path?}` — 触发增量扫描；`POST /api/tasks/crawl` `{source_id, book_url}` — 入队爬取
- `POST /api/tasks/:id/retry` / `POST /api/tasks/:id/pause` / `POST /api/tasks/:id/resume`
- `POST /api/tasks/rebuild-index` — 重建 tantivy 索引（期间可暂停爬虫：`{pause_crawl:true}`）
- `POST /api/tasks/chapter-check` — 全库缺章检查（后台扫全部书，写 `chapter_issues` 表）
- `GET /api/tasks/stats` — 队列长度、熔断状态、令牌桶余量、source_cache 行数/字节数（调试用）

## 环境变量（新增）
- `SHUKU_UPDATE_CHECK_INTERVAL_SECS`（默认 21600）更新检查周期；`SHUKU_SOURCE_HEALTH_INTERVAL_SECS`（默认 86400）健康检查/订阅刷新周期
- `SHUKU_CACHE_MAX_MB`（默认 512）/ `SHUKU_CACHE_MAX_ROWS`（默认 20000）source_cache 上限；`SHUKU_CACHE_PRUNE_INTERVAL_SECS`（默认 86400）清理周期（每天清过期行 + 按最旧淘汰到上限内）
