# BUILD_NOTES — legado-server 后端

## 依赖版本决策（已验证可编译）

- `tantivy 0.27.0` + `tantivy-jieba 0.21.0`：直接兼容，无需降级。
- `rusqlite 0.40.2` + `tokio-rusqlite 0.8.0`：tokio-rusqlite 0.8 要求 rusqlite 0.40（0.32 会因 `libsqlite3-sys` links 冲突无法解析）。`Connection::call` 返回 `tokio_rusqlite::Error<E>`，本项目用 `Database::call_db` 统一转 anyhow。
- `reqwest 0.12` 必须 `default-features = false`：默认特性带 `default-tls`（native-tls → openssl-sys），musl 下找不到 OpenSSL 而失败。已改为纯 rustls（`rustls-tls` + `hickory-dns` + `http2` + `socks` + `charset`）；musl 构建后 `cargo tree` 无 openssl/native-tls。
- `axum 0.8`：路由占位符是 `{id}` 语法，`:id` 会在启动时 panic（已踩过）。
- `tantivy 0.27`：`TopDocs` 本身不是 `Collector`，需 `.order_by_score()`；`SnippetGenerator` 在 `tantivy::snippet`；文档取值需 `use tantivy::schema::Value`（`as_str`/`as_u64` 是 trait 方法）。
- `epub-rs 2.1.6`：`get_current_str()` 返回 `Option<(String, String)>`（正文, mime），不是 `Option<String>`。
- `rquickjs 0.14`：`set_interrupt_handler` 签名是 `Option<Box<dyn FnMut() -> bool>>`；`set_memory_limit`/`set_max_stack_size` 是 `&self` 方法。

## 构建结果

- `cargo build --release`：通过，`target/release/legado-server`（~26MB）。
- `cargo build --release --target x86_64-unknown-linux-musl`：通过，`target/x86_64-unknown-linux-musl/release/legado-server`（~26MB，`static-pie linked`，已实测启动 + 检索 + 前端 200）。
- `cargo test`：25 个测试全绿（24 单元 + 1 端到端），零警告。

## 架构要点（与 docs/ARCHITECTURE.md 一致）

- 9 张表一次迁移（`migrations/001_init.sql`）；正文不进 SQLite：本地书按需解析，缓存书按章节存 `data/chapters/<book_id>/<idx>.txt`。
- 检索：tantivy 文档粒度=章节，schema 固定字段 + 额外 `chapter_idx`（u64 fast/stored，用于定位命中章节）；jieba 分词；snippet 先 content、再 chapter_title、再 book_title 三级兜底，高亮 `<b>`→`<em>`。
- 扫描：mtime + `index_version` 增量；坏书 `status=broken` 不阻断；**先索引成功再写 DB 行**（否则书架可见但搜不到的竞态）。
- 书源：selector 链支持 `&&` 串联、`@attr`、`##regex##replacement`、`js:`；selector/regex 全局预编译缓存；保存/导入时校验 selector/regex/JS 语法/URL 模板。JS 沙箱：无 host 函数、无模块，内存上限 16MB，中断处理器 5s 超时；执行语义——`result` 为输入，eval 补全值或改动后的 `result` 为输出，含 `return` 的脚本自动走 Function 包装重试。
- 调度：scan/crawl 两套 SQLite 队列表 + 各自 worker；域名独立令牌桶（2/s，burst 5）；熔断 5 次连续失败冷却 60s；三级缓存 search 60s / toc 10min / content 24h（内存 + `source_cache` 表，key 含 kind）；测试接口直连执行不排队。
- `Engine::fetch`：HTTP ≥400 按错误返回（不静默存空章节、不触发 4xx 熔断；≥500 才计熔断）。
- 前端：`build.rs` 检测 `../../frontend/dist`，存在则 `include_dir` 嵌入（当前已嵌入，`/` 与 `/assets/*` 实测 200），不存在则占位页、编译不失败。

## 已修复的 bug（自测发现）

1. 重建索引后搜不到：`delete_all` 后 DB 的 `index_version` 仍为 1，增量扫描跳过本地书；且缓存书不在 books 目录内。修：重建前 `index_version` 置 0，另加 `reindex_cached_books` 从章节文件重建缓存书（集成测试已覆盖此回归）。
2. 扫描竞态：书 upsert 后、索引提交前即可见。修：先索引、后写库（扫描与爬虫同理）。
3. 404 被当成成功：爬虫会把 404 页面提空章节。修：`fetch` 对 ≥400 返回错误。

## 已知限制（留待后续）

- txt 章节按需解析：每次读章节都要全文件解析一次（大文件可接受，十几万本规模下建议后续加章节偏移索引）。
- 扫描目前是单 worker 顺序处理；十几万本首次全量扫描会较慢（可并行化解析 + 批量索引，后续优化）。
- epub 章节不含图片段落（只出 text）；图片懒加载需另加 `/api/books/:id/images/:name` 之类的图片直出端点。
- 书源规则是阅读3 JSON 的常用子集：`&&`/`@attr`/`##`/`js:`、bookList/chapterList/content 对象规则已覆盖；XPath/`@json:` 解析已实现（2026-11-10，见下）；规则嵌套对象的高级形式未实现，遇到会返回空提取（不崩溃）。
- `searchUrl` 模板只替换 `{{key}}`/`{{keyword}}`/`{{page}}`。
- 令牌桶/熔断参数目前是硬编码（2/s、burst 5、5 次/60s），未做成环境变量。
- notify 监听是 best-effort：监听失败不影响周期扫描；事件触发的是增量扫描任务（3s 轮询拾取）。
- 缓存书的书名/作者来自爬取任务入参（或回退 book_url）；若书源 toc 页无书名，正文页才会有。

## 并行扫描（2026-11-06）

- 结构：stat 筛选（mtime）→ 解析并发（`LEGADO_SCAN_WORKERS`，默认核数，上限 16，信号量限流 + JoinSet）→ mpsc → 单索引器批量提交（64 本或 64MB 一批，一次 tantivy commit）→ 整批一个 SQLite 事务入库。先索引后写库的不变式按本保持；坏书只坏自己。
- 找到的三处瓶颈（200 本 txt / 159MB / 1 万章，4 核机实测）：
  1. 每本一次 tantivy commit：串行 20.9s → 批量提交后 16.7s。
  2. writer 线程数：`writer_with_num_threads(1)` 改多线程几乎无收益（probe：1 线程 5.7s vs 8 线程 4.8s）；jieba 分词本身只要 2.1s，大头在 tantivy 建段/存正文的单线程部分。默认改为核数一半（`LEGADO_INDEX_THREADS`，1..8），堆默认 150MB（`LEGADO_INDEX_HEAP_MB`）。
  3. 每本两次 SQLite 提交：400 次 fsync 曾被怀疑是瓶颈，实测改成整批一事务后无变化（内存库入库只要 0.1s），遂保留批量事务（仍是正确做法），不再归因于 fsync。
- 最终成绩：串行 20.9s → 并行 15.0s（约 1.4 倍，2 解析线程 + 2 索引线程、批量 64 时最优）；线程总数远超核数反而变慢（4+4 要 17.2s）。probe 拆分：解析 4.3s / 索引（磁盘、批量）5.2s / 入库 0.1s。
- 性能探针：`cargo test --release probe_scan_split / probe_jieba_modes`（读 /tmp/legado-perf/books，缺失则跳过）。

## 分页反序列化 bug（2026-11-06，实测发现）

- `/api/books`、`/api/search`、`/api/tasks` 的查询结构用了 `#[serde(flatten)] PageQuery`，serde_urlencoded 在 flatten 下解析不到数字，`?page_size=1` 直接 400。前端每次都带 page/page_size，等于带分页参数的调用全坏，只有默认调用能跑。
- 修：去掉 flatten，分页字段直接放各查询结构里，统一走 `pq()`；集成测试加回归断言。

## 书源高级规则（2026-11-10）

- 请求规则：`searchUrl` / `exploreUrl` / `loginUrl` 按 Legado 格式解析——`{key}`/`{page}` 单括号和 `{{key}}`/`{{page}}` 双括号占位、`{{expr}}`（JS，绑定 `key`/`page`/`result`）、尾部 `,{...}` 配置 JSON（`method` GET/POST、`body`、`headers`、`charset`）。相对 URL 按 `bookSourceUrl` 补全；畸形配置 JSON 保存时报错。响应解码：charset 提示 > Content-Type charset > 严格 UTF-8 > chardetng 嗅探（gbk 站无 charset 也能读）。
- 顺带修的真 bug：旧 `render_url` 只认 `{{key}}` 不认 `{key}`，且保存校验直接 `Url::parse` 原模板——标准书源（`?q={key}`）导入即被拒。现在两处都走 `parse_request_rule`。
- 登录：`loginUrl`（POST 为主，账号密码就写在书源 JSON 里，和真书源一样）+ `loginCheckJs`（跑在响应体上，返回 `"true"` 才算登录成功；没有则 2xx/3xx 即成功）。cookie 用 reqwest `cookies` 特性，按（域名，书源 id）隔离客户端，登录 cookie 不会串到同域名其他书源；`enabledCookieJar=false` 则不存 cookie。首次请求前自动登录一次（成功 30 分钟内复用）；401/403 时登录一次再重试一次；登录失败只记 warn 继续走（公开内容仍可抓），测试面板有单独的 `login` 种类，失败即报错。
- 分页：`nextTocUrl`（上限 20 页）、`nextContentUrl`（上限 10 页）逐页跟随，空/重复/与当前页相同即停；搜索 `max_pages`（1..10，空页即停），起始页 `page`。下一页 URL 按当前页补全。
- webJs：ruleContent 按 Legado 顺序 sourceRegex → webJs → content → replaceRegex。webJs 在 rquickjs 沙箱里跑，输入是原始 HTML 字符串（`result`），没有 DOM——`document`/`window`/`java` 桥不可用，需要 DOM 的脚本会报明确错误。输出超长时测试面板只留前 2000 字。
- 校验：保存时校验 loginUrl、loginCheckJs、`header` JSON、webJs/preUpdateJs/formatJs/init 语法、sourceRegex/replaceRegex、next*Url 规则；JS 字段不再被当成 CSS 选择器校验（曾误报）。
- 顺带修的真 bug：`BookHit.coverUrl`/`intro` 有 `skip_serializing_if` 却没有 `#[serde(default)]`，空值时序列化省略、反序列化报 `missing field`。已补 default。
- 测试：39 单元 + 1 端到端全绿；新增本地 fixture 服务器集成测试（POST 搜索分页、目录/正文分页、登录 + 401 重试 + cookie 隔离）；真机冒烟用 musl 二进制 + Python fixture 站跑过：搜索 3 本、登录 true、目录 3 章、正文两页全通，前端 `/` 200。
- 未做（2026-11-10 晚已全部补齐，见「剩余规则补齐」一节）：`loginUi`；ruleToc 的 `preUpdateJs`/`formatJs` 只校验不执行；XPath/`@json:` 解析模式仍未实现。

## XPath / @json: 解析（2026-11-10）

- XPath：手写子集引擎（`src/xpath.rs`，跑在 scraper 已解析的 DOM 上；不用 libxml2，否则 musl 静态编译会断）。支持 `//`、`/、.//、.、..；tag、*、@attr、text()`（末步才有效）；谓词 `[@a='v']`、`[@a]`、`[n]`、`[last()]`、`[position() >/>=/</<=/= n]`、`[contains(@a,'v')]`、`[contains(text(),'v')]`、`[starts-with(@a,'v')]`，可连写多个。写法：`@xpath:` 前缀，或直接以 `/`、`./` 开头；相对路径（`.//a/text()`）以当前元素为作用域。列表规则（bookList/chapterList）和 content 规则都能用 XPath；字段规则里 `//a/@href`、`//a/text()` 直接返回值。不支持的语法（过滤器、轴名如 `following-sibling::`、函数调用）保存时就报错，运行时绝不静默返回空。
- @json：JSON 响应自动识别（响应体以 `{` 或 `[` 开头即走 JSON 分支；解析失败报错，不回退 HTML）。JSONPath 子集（`src/jsonpath.rs`）：`$`、`.field`、`['field']`、`[n]`、`[-n]`、`[*]`、链式组合；不支持过滤器 `[?(...)]`、递归 `..`、切片，写了保存时报错。写法：`@json:$.data[*]`、裸 `$.data[*]`、裸字段名（`title` 即 item["title"]）。bookList/chapterList 取 JSON 数组元素后子规则逐个跑；content 规则取值后按行切段落；nextTocUrl/nextContentUrl 也能用 JSONPath。JSON 里没给子规则时按默认字段名取（name/title、author、bookUrl/url、coverUrl/cover、intro/description）。
- 规则前缀全集（保存时都校验）：`js:`、`@xpath:`/`/`、`@json:`/`$`、`@regex:`/`:`（对节点文本做正则搜索，取第一捕获组）、`@css:`/裸（原 CSS 链）。`##regex##replacement` 后缀对所有模式都生效。
- 顺带修的真 bug：XPath 引擎第一版把 `@attr`/`text()` 末步当成子轴去找子元素，结果永远取不到值；已改为读上一步选出的元素自身的属性/文本。
- 测试：50 单元 + 1 端到端全绿；新增本地 fixture 服务器集成测试（JSON 搜索/目录/正文 + XPath 搜索/目录/正文分页全通）；musl 二进制 + 本地 fixture 站冒烟全通（JSON 源搜索 2 本/目录 2 章/正文 2 段，XPath 源搜索 3 本/目录 3 章/正文两页，前端 200）。
- 未做：XPath 轴（`following-sibling::` 等）、`ancestor::`、`@json:` 过滤器、`@json:` 递归下降；ruleSearch 的 checkKeyWord 仍只做校验。（2026-11-10 晚已全部补齐，见「剩余规则补齐」一节。）

## 规则拾取器（2026-11-10）

- 后端：`POST /api/picker/fetch`（`Engine::fetch_picker`）：url 用 searchUrl 同款请求规则模板（`{key}`/`{{key}}`、`{{page}}`、尾部 `,{...}` POST 配置），走书源同一套请求池、charset 解码、域名限流、熔断；不缓存、不登录、不提取，原样返回页面（超 2MB 拒绝，避免 iframe 里截断 DOM 生成错规则；只接受 http(s)）。
- 前端：`#/picker` 与书源列表每行的「拾取」入口（`#/sources/:id/pick`）。服务端代抓后页面以快照形式在 iframe（srcdoc，同源）里打开：先剥离页面自身 `<script>`（点元素不会跳转、页面不会重渲染），补 `<base>` 让相对资源正常加载，再注入拾取脚本（`frontend/src/lib/pickerScript.ts`）。
- 拾取脚本：悬停描边高亮；点字段旁「选」再点元素即生成规则。列表字段（bookList/chapterList）点一个条目生成匹配全部同类条目的选择器（并虚线标出所有命中项），之后的名称/作者/链接在第一个条目内相对生成（`a.title@text`、`a.title@href` 这类）；正文 content 点容器本身。格式开关：CSS / XPath。预览按钮在页面内即时算前 3 条提取结果（最终以服务端测试面板为准）。
- JSON 响应：不走 iframe，渲染可折叠 JSON 树；点数组节点生成 `$.data[*]` 列表规则，点标量节点生成 `@json:` 规则（已设列表时自动相对到条目，如 `$.title`）。
- 回填：复制规则 JSON，或保存到书源（已选书源则合并 ruleSearch/ruleToc/ruleContent 后 PUT；没选则按站点 origin 新建一个），随后去测试面板验证。
- 测试：51 单元 + 1 端到端全绿；新增 `engine_picker_fetch`（POST 搜索模板抓回含「书一」的页、非法 URL 报错）；前端 `npm run build` 通过，拾取脚本已打进 bundle（`legado-picker` 标记在列）；musl 二进制冒烟全通（picker 抓 HTML 页 200 含「书一」、抓 JSON 接口 200、非法 URL 明确报错，前端 200）。
- 未做：拾取器不处理登录后页面（需登录的站先在书源里配好 loginUrl 再用测试面板）；JS 渲染的站（内容不在首屏 HTML 里）抓回来是空壳，拾取器帮不上；iframe 里页面自带样式可能遮挡高亮。

## 剩余规则补齐（2026-11-10）

把此前各阶段报告里列的全部未做缺口一次做完（Ye 原话「把剩下没做的做了」）。

- JS 核心：`run_js` 绑定补齐——`input`/`result`/`src` 同义，`base_url`/`baseUrl`/`url`（当前页 URL）；`java.*` 纯函数（md5Encode/base64Encode/base64Decode/encodeURIComponent/decodeURIComponent/encodeURI/decodeURI/timeFormat/now/uuid，MD5 是 JS 内实现，已用已知向量验证）；全局 `regex_replace`、`strip_ws`；最小 `URL` 构造器（href/origin/host/pathname/search/hash/toString，相对解析含 `..` 归一化）。`js:` 返回数组/对象时转 JSON 字符串交下一阶段；`<js>...</js>` 与 `js:` 等价；`@js:` 后缀（先取值再交 JS，`@js:` 在 `js:` 判定之后切，防误切脚本内字面量）。异常信息带真实 JS 异常文本，不再是泛化的 "Exception generated by QuickJS"。
- js: 列表规则：HTML 模式下 `bookList`/`chapterList` 可以是 `js:`——对整页跑 JS，输出按 JSON 解析（数组→条目、单对象→一条、不可解析→空）。`bookUrl` 可写 URL 模板（如 `https://example.com/book/{{id}}`：`{{字段}}` 直查，其余 `{{expr}}` 以条目字段为变量跑 JS）。
- 列表语义：`chapterList`/`bookList` 的 `-` 前缀反转、`+` 保持原序；章节/书籍按 URL 去重。`BookHit` 扩展字段 `kind`/`lastChapter`/`updateTime`/`wordCount`（JSON 回退别名 latestChapter/updated/words）。搜索结果为空且未配 `bookUrlPattern` 时，把当前页按 `ruleBookInfo` 当详情页回退解析一本书（HTML 需显式 name 规则，否则不回退以防整页文本当书名；book_url 为空时用页面 URL）。
- checkKeyWord：按官方文档，它是测试面板默认关键词、不参与搜索结果校验；保存校验时当纯字符串，不再按选择器校验。前端打开书源时若它非空且 keyword 为空，用它预填 keyword 输入框。
- 发现页 explore：`Engine::run_explore`（exploreUrl 模板 + ruleExplore + 搜索提取，登录前置）；测试面板 `kind` 加 `explore`（不需要 keyword/book_url 输入，可翻页）。
- preUpdateJs / formatJs：目录解析前对整页响应跑 preUpdateJs（HTML+JSON 两路）；章节解析完后 formatJs 只重写标题（绑定 `index` 1 起、`title`、`chapter={name,url}`，输出空则保留原名）。`ChapterHit` 加 `isVolume`/`isVip`/`isPay`；chapterUrl 为空时 isVolume 为真→卷节点占位 URL（`volume://{index}`），否则退回当前目录页 URL。
- XPath 轴：`child::`/`descendant::`/`descendant-or-self::`/`parent::`/`ancestor::`/`ancestor-or-self::`/`self::`/`following-sibling::`/`preceding-sibling::`（`//` 仍是 descendant-or-self）。`following::`/`preceding::`（整文档顺序）、`attribute::`（用 `@attr`）保存时明确报错，绝不静默返回空。
- JSONPath：递归 `..`（`$..title`、`$..book[*].title`，DFS 前序）和过滤器 `[?(...)]`（`@.path op 字面量`，op `==/!=/>/</>=/<=`，字面量数字/引号字符串/true/false/null，`&&`/`||` 左结合；`@.vip` 真值性）。为保文档顺序，serde_json 开了 `preserve_order`（Map 不再按字母排序）；点记法字段名须是标识符，否则报错（`$.a b`、`@.b =~ 1` 这类不再静默当成怪字段名）。
- loginUi：`src/login_ui.rs`——登录页解析成元素/表单表 + JS 侧 `document` 垫片（querySelector/querySelectorAll/getElementById/getElementsByClassName/getElementsByTagName/forms；元素 value/checked/textContent 回写、getAttribute/setAttribute/click/submit/closest；CSS 子集：tag/#id/.class/[attr]/[attr=v]、后代、子 `>`、逗号，不支持伪类）。`login()`：loginUrl 取页 → loginUi 脚本填表提交（走同一 cookie jar，GET 拼 query、POST 出 urlencoded）→ 对提交响应跑 loginCheckJs；无 form 提交则明确报错。`window`/`java.ajax`/`kv_*`/`cache` 等不在垫片里，脚本用了会报明确错误。
- epub 图片段落：epub 解析时 `<img>` 产出 `\x01IMG:{epub 内路径}` 标记行（src 相对当前 xhtml 路径解析，`..`、开头的 `/`、百分号编码都处理）；`get_chapter` 两条路径（本地按需解析/缓存文本）都走 `paragraphs_from_text` 转成 `{img}` 段落（前端 Reader 已原生渲染）；新增 `GET /api/books/:id/images/*path`（axum 通配符语法是 `{*path}`；仅 epub，`..` 拒绝，mime 按 epub 内记录）；扫描索引前 `strip_img_markers` 去标记行，不污染检索。
- 测试：69 单元 + 2 端到端全绿（含新增：XPath 轴、JSONPath 递归/过滤器、epub 图片解析 + 端到端 images 200/防穿越/检索不污染、fixture explore/loginUi 表单全链路/ruleBookInfo 回退）。fixture 站（Rust 内嵌 + `smoke-fixture.py` 镜像）新增 `/explore`、`/login-form`（GET 表单 + POST 校验 alice/s3cret 种 cookie）、`/bookinfo`。
- 仍未做（有意不做，见下）：`java.ajax/get/post`、`kv_*`、`cache`、`cookie` 这类带网络/持久化的 JS API（与沙箱禁网络约束冲突）；`@put`/`@get`/`init` 跨规则变量作用域（单独工程量）；JS 渲染站（首屏 HTML 是空壳，需真浏览器引擎，违反 musl 单二进制约束）；`nextTocUrl` 返回多个分页 URL（仅跟单个）；loginUi 选择器子集限制（见上）。
- 限流参数可配：`LEGADO_RATE_PER_SEC`（默认 2）/`LEGADO_RATE_BURST`（默认 5）每域名令牌桶；`LEGADO_BREAKER_FAILURES`（默认 5）/`LEGADO_BREAKER_COOLDOWN_SECS`（默认 60）熔断；`LEGADO_CACHE_TTL_SEARCH_SECS`/`LEGADO_CACHE_TTL_TOC_SECS`/`LEGADO_CACHE_TTL_CONTENT_SECS`（默认 60/600/86400）三级缓存 TTL。`Config::from_env` 读环境变量，非法值回退默认、不影响启动；`Cache::with_ttls` + `Engine::with_limits` 接线，`main.rs` 用配置值，`Engine::new`/`Cache::new` 保留默认值供测试用。

## 第二批功能（2026-11-10 晚，Ye 选的 17 项）

- ① 书架更新检查：`update_check` crawl 任务 + `run_update_worker`（启动 60s 后首轮，每 6h，`LEGADO_UPDATE_CHECK_INTERVAL_SECS`）；`due_update_books`/`mark_update_checked`/`set_has_update`；只抓新增章、重建索引；`POST /api/tasks/check-updates`、`POST /api/books/:id/check-update`；前端书架「检查更新」+「有更新」徽章。坑：fixture `/toc` 的 starts_with 曾吞掉 `/toc-update`。
- ② 换源：`change_source` 任务（搜索找书或显式 book_url，全量重抓、删多余旧章节文件、book id 不变、进度保留）；`POST /api/books/:id/change-source`；前端缓存书行内换源下拉。顺带修了旧 crawl 单章失败跳过导致章节序号错位的 bug（现改为一一对应，失败写空文件）。
- ③ 书源健康检查：`health_check` 任务（checkKeyWord 搜索或 explore 探针）+ `run_health_worker`（启动 300s 后首轮，每天，`LEGADO_SOURCE_HEALTH_INTERVAL_SECS`）；`health`/`health_error` 列；前端徽章 + 每源检查按钮。
- ④ 书源规则订阅：`refresh_subscription` 任务（`fetch_picker` 抓订阅 URL，数组或单个对象，逐条校验入库、坏条目跳过）；`POST /api/sources/subscribe`；前端订阅输入框 + 徽章。顺带修了 `validate_source` 不校验对象/必填字段的真 bug（裸字符串曾被 upsert 成随机源）。
- ⑤ epub 封面：`parse_epub_file` 取 `doc.get_cover()`（≤5MB、image/*）；字节存 `data/covers/<id>`，DB 只存 mime；`GET /api/books/:id/cover`；前端书架缩略图。`scan_once` 签名改成 `scan_once(db, search, dir, data_dir)`。
- ⑥ 本内搜索：`GET /api/search?book_id=`（book_id 优先于 scope，非法 id 返回空）；前端 Reader 搜索框 + 结果跳章。
- ⑦ 拼音搜索：tantivy 新增 `pinyin` 字段（书名+作者+章节名拼音，空格分隔 + 连写，`pinyin` crate）；查询侧 `expand_pinyin_query` 把拉丁字母串贪心切音节（音节表由 crate 字典在 CJK 区扫出，`xiuxian`→`xiu xian`，非拼音原样保留）；`INDEX_VERSION` 1→2（老库下次扫描自动重建）；正文拼音有意不索引（索引翻倍、查询形态少见）。教训：jieba 把 `xiuxian` 当整词，索引里连写 `xiuxianchuan` 是另一词，必须查询侧切音节。
- ⑧ 书库去重：`duplicate_groups`（同书名+作者，组内章节数/大小排）；`DELETE /api/books/:id`（FK 级联 + 清索引 + 删章节/封面文件）；前端 `#/duplicates`（默认留章节最多的副本）。
- ⑨ 同作者归组：纯前端——Reader 用现成的 `listBooks({author})`，前端再按作者全等过滤，底部列「同作者的其他书」。
- ⑩ 阅读统计：`reading_seconds` 表 + `POST /api/books/:id/reading-time`（单次上限 3600s）；`GET /api/stats`（读完=读到最后一章；在读/藏书/累计时长/活跃天数/每月读完/最近读完/耗时最多的书）；前端 Reader 每秒累计（页面可见时）、满 60s 上报、退出时补报；`#/stats` 页。
- ⑪ OPDS：`src/opds.rs`（`GET /opds/`、`/opds/books`、`/opds/authors[/:author]`、`/opds/recent`、`/opds/search`、`/opds/opensearch.xml`、`/opds/download/:id`）；本地书给原文件、缓存书由章节文件拼 txt（去图片标记行）；分页 first/previous/next/last；XML 转义；`list_authors`/`books_by_author`（精确匹配）。
- ⑫ 朗读：纯前端——`speechSynthesis`，优先中文语音，语速 0.5–2，切章/退出停；读显示文本（含繁简转换后）。
- ⑬ 繁简转换：纯前端——`opencc-js`（`cn2t`/`t2cn` 分入口，动态 import 按需加载；`to: "t"` 通用繁体）；阅读设置 `script: orig|trad|simp`（localStorage）；按批注分段后逐段转（批注按原文分段不受影响）；词典 chunk 已从 SW 预缓存排除（`globIgnores`），否则首次访问就下 1.1MB。
- ⑭ 整本离线：章节缓存放宽到 5000 条/30 天（`recent-chapters`），新增 `book-images`（封面/插图 1000 条/30 天）；`src/lib/offline.ts`（`downloadBook` 4 并发预取全部章节 + 书详情/目录/封面，`offlineChapterCount` 实查 Cache Storage，`deleteOfflineBook`）；Reader「下载整本离线读」+ 实际缓存数 + 删除。
- ⑮ 缓存自动清理：`prune_source_cache`（先删过期行，再按行数/字节上限删最旧行；字节超限按平均大小估算条数，循环复核，绝不整表删）；`run_cache_worker`（启动 60s 后首轮，每天，`LEGADO_CACHE_MAX_MB`=512/`LEGADO_CACHE_MAX_ROWS`=20000/`LEGADO_CACHE_PRUNE_INTERVAL_SECS`=86400）；`GET /api/tasks/stats` 加 `cache{rows,bytes}`。教训：LIMIT 固定 100 会一次删光超限不多的表；字节估算必须按平均大小算条数。
- ⑯ 缺章/乱章检测：`src/chapter_check.rs`（`chapter_number`：阿拉伯数字 + 中文数字到万；`detect_gaps`：跳号、重复、倒序、开头缺章；`卷` 换卷重置，不误报；章节号正则排除「卷」，否则「第一卷 第二章」会被当成第 1 章）；`chapter_issues` 表；`POST /api/tasks/chapter-check` 全库扫（标题缺口 + 缓存书空章节文件；本地书正文不解析）；`GET /api/books/:id/chapter-issues` 单本即时检查（本地书解析一次查空章节）；前端 Reader「检查缺章」+ 任务页「全库缺章检查」与问题书列表。
- ⑰ 搜索联想：`GET /api/suggest?q=`（书名/作者 instr 子串，书名优先；不足 8 条时 tantivy 补足，覆盖拼音如 `xiuxian`）；前端搜索框防抖 200ms 下拉，点条目直接进书。
- 测试：97 单元 + 2 端到端全绿；musl 二进制 29.5MB + fixture 冒烟全通（搜索高亮、拼音联想、OPDS feed、下载正文）。新增依赖：`pinyin 0.10`（后端）、`opencc-js 1.4.2`（前端，词典按需加载）。

## 首字母搜索（2026-11-10 晚，Ye 追加）

- 拼音字段再加首字母串及全部前缀：`pinyin_text` 输出从 `xiu xian chuan xiuxianchuan` 变成 `xiu xian chuan xiuxianchuan x xx xxc`（书名/作者/章节名各自都加）。查询侧不用改——`xx` 切不出音节就原样当词项，正好命中前缀词项；`xxc` 命中完整首字母串；`wy` 命中作者忘语的首字母。
- `INDEX_VERSION` 2→3：老库下次扫描自动重建索引（首字母是索引内容，不是结构，旧索引里没有）。
- 测试：98 单元 + 2 端到端全绿（含新增 `search_by_initials`：xx/xxc/wy 命中修仙传、xx 不命中测试小说、全拼 xiuxian 回归；e2e 断言 search xx 与 suggest xxc）。

## 拼音/首字母搜索性能实测（2026-11-10 晚，`probe_pinyin_cost`，200 本/1 万章/166MB 语料）

- 索引大小：有拼音 34.1MB vs 无拼音 33.4MB，**+764KB（+2.3%，约 76 字节/章）**。按十几万本（约 500 万章）外推，索引多约 380MB。注意：建完立刻量会虚高一倍（未合并的段各自带一份正文词典），必须等后台合并跑完再量——第一版探针就踩了这个坑。
- 建索引时间：7.7s vs 7.7s，无差别；`pinyin_text` 全库算一遍只要 4ms。
- 查询耗时（中位数）：中文「江湖」7.5ms vs 拼音 `di` 6.6ms（两者都命中 1 万章）——耗时由命中数决定，与查询是中文还是拼音/首字母无关；无命中的查询（`xx`、`jianghu`）约 0.5ms。音节表首次构建 1ms（一次性）。
- 结论：拼音+首字母搜索对性能没有可感知的影响，代价只是索引大 2% 左右。量级提醒：十几万本时，命中几百万章的宽泛查询（比如单字 `di`）耗时会随命中数涨，和中文查询一样；书名/首字母这种命中几本的典型查询仍是毫秒级。

## 自检自测（2026-11-10 晚，Ye 要求全量自查）

- 抓到一个真 bug：`index_books`（正式代码）调用了 `#[cfg(test)]` 的 `index_books_pinyin`——只跑 `--bin` 测试时 `cfg(test)` 开着，掩盖了；全量 `cargo test`（含集成测试，要以正常模式编 binary）直接编译失败。已修：`index_books_pinyin` 去掉 `#[cfg(test)]`，成为正式方法（探针用它关拼音做对照）。
- 清掉全部 10 个编译警告（`cargo check` 正常与测试两种构建都零警告）：删 `login_ui.rs` 未用的 Duration 导入、`expand_pinyin_query` 多余的 `mut`、正式代码里没人用的 `html_to_text`（被 `html_to_text_with_images` 取代）、`Engine::run_search`（无调用者，被 `run_search_pages` 取代）、测试里死掉的 `crc32`/`stored_zip` 包装；仅测试用的 `Cache::new`/`CacheKind::ttl`、`Engine::new`、`extract_toc`/`extract_content`、`list_chapter_issues` 加 `#[cfg(test)]`；`RuleStr` 的 `regex`/`replacement` 字段写了没人读（调用方都走 `split_rule_parts`），已删字段。
- 全量：99 单元 + 2 端到端全绿，零警告；前端 `npm run build` 通过（dist 11 个预缓存条目）；musl 静态二进制 + fixture 冒烟见下。
- 代码里只剩两处 TODO，都是 build.rs/static_files.rs 里的流程提醒（发布构建前先编前端 dist，否则二进制内嵌占位页）；build.rs 已有防护——dist 不存在时构建会打警告，且本次冒烟验证了内嵌的是真实前端。
- 项目没有 git 仓库（`git status` 报 not a git repository），版本历史不在版本控制里——Ye 定是否要建。

## Docker CI（2026-11-11，Ye 要求）

- `Dockerfile`（仓库根）：alpine:3.21 + 预编的 musl 静态二进制（CI 每架构编一个，`bin-<arch>/legado-server`），建 `/tmp`（扫描暂存用）`/data` `/books`；默认 `LEGADO_BOOKS_DIR=/books`、`LEGADO_DATA_DIR=/data`、`LEGADO_ADDR=0.0.0.0:8080`，端口 8080。reqwest 用 rustls 自带根证书，镜像里不需要 CA 证书包。
- `.github/workflows/docker.yml`：push 到 main 或 `v*` 标签时跑；`binary` 任务在 amd64（ubuntu-24.04）与 arm64（ubuntu-24.04-arm）原生 runner 上各编前端 + musl 二进制，**先跑全量测试，通过才推镜像**；`docker` 任务（PR 不跑）用 buildx 各推 `amd64-<sha>` / `arm64-<sha>` 镜像，再 `imagetools create` 合成多架构清单 `:latest`、`:sha-<sha>`（打标签时另加 `:<版本号>`）。GHCR 包默认私有，工作流最后一步用 `gh api` 把它改公开（best-effort，失败不影响构建）。
- 本地编镜像：先编 musl 二进制，放进 `bin-amd64/legado-server`，再 `docker build -t legado-server .`（本机无 docker，未本地验证，靠 CI 首跑验证）。

## 发布（PAT 推送，2026-11-11，Ye 要求）

- 项目 PAT「legado-server Muse」（fine-grained，只限本仓库，Contents + Workflows 读写，到期 2027-11-11），存在连接器 custom.github-legado-pat，token 值只在保险库里，不进任何文件。
- 推送走 `~/workspace/custom-connectors/custom.github-legado-pat/bin/gh_push.py`（Git Data API：blob→tree→commit→更新 main，一次提交）；不要用 MCP push_files（在有提交的仓库上建 tree 会 403），不要用 MCP create_or_update_file 逐文件推（每个文件一次提交、一次批准卡）。
- `.github/workflows/*` 同样走 gh_push.py（PAT 有 Workflows 权限，App 没有）。
