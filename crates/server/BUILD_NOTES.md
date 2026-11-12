# BUILD_NOTES — shuku 后端

## 依赖版本决策（已验证可编译）

- `tantivy 0.27.0` + `tantivy-jieba 0.21.0`：直接兼容，无需降级。
- `rusqlite 0.40.2` + `tokio-rusqlite 0.8.0`：tokio-rusqlite 0.8 要求 rusqlite 0.40（0.32 会因 `libsqlite3-sys` links 冲突无法解析）。`Connection::call` 返回 `tokio_rusqlite::Error<E>`，本项目用 `Database::call_db` 统一转 anyhow。
- `reqwest 0.12` 必须 `default-features = false`：默认特性带 `default-tls`（native-tls → openssl-sys），musl 下找不到 OpenSSL 而失败。已改为纯 rustls（`rustls-tls` + `hickory-dns` + `http2` + `socks` + `charset`）；musl 构建后 `cargo tree` 无 openssl/native-tls。
- `axum 0.8`：路由占位符是 `{id}` 语法，`:id` 会在启动时 panic（已踩过）。
- `tantivy 0.27`：`TopDocs` 本身不是 `Collector`，需 `.order_by_score()`；`SnippetGenerator` 在 `tantivy::snippet`；文档取值需 `use tantivy::schema::Value`（`as_str`/`as_u64` 是 trait 方法）。
- `epub-rs 2.1.6`：`get_current_str()` 返回 `Option<(String, String)>`（正文, mime），不是 `Option<String>`。
- `rquickjs 0.14`：`set_interrupt_handler` 签名是 `Option<Box<dyn FnMut() -> bool>>`；`set_memory_limit`/`set_max_stack_size` 是 `&self` 方法。

## 构建结果

- `cargo build --release`：通过，`target/release/shuku`（~26MB）。
- `cargo build --release --target x86_64-unknown-linux-musl`：通过，`target/x86_64-unknown-linux-musl/release/shuku`（~26MB，`static-pie linked`，已实测启动 + 检索 + 前端 200）。
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
- 书源规则是阅读3 JSON 的常用子集：`&&`/`@attr`/`##`/`js:`、bookList/chapterList/content 对象规则已覆盖；XPath/`@json:` 解析已实现（2026-11-10，见下）；规则嵌套对象的高级形式未实现，遇到会返回空提取（不崩溃）。
- notify 监听是 best-effort：监听失败不影响周期扫描；事件触发的是增量扫描任务（3s 轮询拾取）。
- 缓存书的书名/作者来自爬取任务入参（或回退 book_url）；若书源 toc 页无书名，正文页才会有。

（已解决，从本节移出：并行扫描 2026-11-06；epub 图片段落 + `/api/books/:id/images/*path` 2026-11-10；`searchUrl` 模板扩展 `{{JS}}`/请求规则 JSON/charset 链 2026-11-10；令牌桶/熔断/缓存 TTL 全部可配 `SHUKU_RATE_*`/`SHUKU_BREAKER_*`/`SHUKU_CACHE_TTL_*` 2026-11-10，见 docs/API.md 环境变量表。）

## 并行扫描（2026-11-06）

- 结构：stat 筛选（mtime）→ 解析并发（`SHUKU_SCAN_WORKERS`，默认核数，上限 16，信号量限流 + JoinSet）→ mpsc → 单索引器批量提交（64 本或 64MB 一批，一次 tantivy commit）→ 整批一个 SQLite 事务入库。先索引后写库的不变式按本保持；坏书只坏自己。
- 找到的三处瓶颈（200 本 txt / 159MB / 1 万章，4 核机实测）：
  1. 每本一次 tantivy commit：串行 20.9s → 批量提交后 16.7s。
  2. writer 线程数：`writer_with_num_threads(1)` 改多线程几乎无收益（probe：1 线程 5.7s vs 8 线程 4.8s）；jieba 分词本身只要 2.1s，大头在 tantivy 建段/存正文的单线程部分。默认改为核数一半（`SHUKU_INDEX_THREADS`，1..8），堆默认 150MB（`SHUKU_INDEX_HEAP_MB`）。
  3. 每本两次 SQLite 提交：400 次 fsync 曾被怀疑是瓶颈，实测改成整批一事务后无变化（内存库入库只要 0.1s），遂保留批量事务（仍是正确做法），不再归因于 fsync。
- 最终成绩：串行 20.9s → 并行 15.0s（约 1.4 倍，2 解析线程 + 2 索引线程、批量 64 时最优）；线程总数远超核数反而变慢（4+4 要 17.2s）。probe 拆分：解析 4.3s / 索引（磁盘、批量）5.2s / 入库 0.1s。
- 性能探针：`cargo test --release probe_scan_split / probe_jieba_modes`（读 /tmp/shuku-perf/books，缺失则跳过）。

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
- 测试：51 单元 + 1 端到端全绿；新增 `engine_picker_fetch`（POST 搜索模板抓回含「书一」的页、非法 URL 报错）；前端 `npm run build` 通过，拾取脚本已打进 bundle（`shuku-picker` 标记在列）；musl 二进制冒烟全通（picker 抓 HTML 页 200 含「书一」、抓 JSON 接口 200、非法 URL 明确报错，前端 200）。
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
- 限流参数可配：`SHUKU_RATE_PER_SEC`（默认 2）/`SHUKU_RATE_BURST`（默认 5）每域名令牌桶；`SHUKU_BREAKER_FAILURES`（默认 5）/`SHUKU_BREAKER_COOLDOWN_SECS`（默认 60）熔断；`SHUKU_CACHE_TTL_SEARCH_SECS`/`SHUKU_CACHE_TTL_TOC_SECS`/`SHUKU_CACHE_TTL_CONTENT_SECS`（默认 60/600/86400）三级缓存 TTL。`Config::from_env` 读环境变量，非法值回退默认、不影响启动；`Cache::with_ttls` + `Engine::with_limits` 接线，`main.rs` 用配置值，`Engine::new`/`Cache::new` 保留默认值供测试用。

## 第二批功能（2026-11-10 晚，Ye 选的 17 项）

- ① 书架更新检查：`update_check` crawl 任务 + `run_update_worker`（启动 60s 后首轮，每 6h，`SHUKU_UPDATE_CHECK_INTERVAL_SECS`）；`due_update_books`/`mark_update_checked`/`set_has_update`；只抓新增章、重建索引；`POST /api/tasks/check-updates`、`POST /api/books/:id/check-update`；前端书架「检查更新」+「有更新」徽章。坑：fixture `/toc` 的 starts_with 曾吞掉 `/toc-update`。
- ② 换源：`change_source` 任务（搜索找书或显式 book_url，全量重抓、删多余旧章节文件、book id 不变、进度保留）；`POST /api/books/:id/change-source`；前端缓存书行内换源下拉。顺带修了旧 crawl 单章失败跳过导致章节序号错位的 bug（现改为一一对应，失败写空文件）。
- ③ 书源健康检查：`health_check` 任务（checkKeyWord 搜索或 explore 探针）+ `run_health_worker`（启动 300s 后首轮，每天，`SHUKU_SOURCE_HEALTH_INTERVAL_SECS`）；`health`/`health_error` 列；前端徽章 + 每源检查按钮。
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
- ⑮ 缓存自动清理：`prune_source_cache`（先删过期行，再按行数/字节上限删最旧行；字节超限按平均大小估算条数，循环复核，绝不整表删）；`run_cache_worker`（启动 60s 后首轮，每天，`SHUKU_CACHE_MAX_MB`=512/`SHUKU_CACHE_MAX_ROWS`=20000/`SHUKU_CACHE_PRUNE_INTERVAL_SECS`=86400）；`GET /api/tasks/stats` 加 `cache{rows,bytes}`。教训：LIMIT 固定 100 会一次删光超限不多的表；字节估算必须按平均大小算条数。
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

- `Dockerfile`（仓库根）：alpine:3.21 + 预编的 musl 静态二进制（CI 每架构编一个，`bin-<arch>/shuku`），建 `/tmp`（扫描暂存用）`/data` `/books`；默认 `SHUKU_BOOKS_DIR=/books`、`SHUKU_DATA_DIR=/data`、`SHUKU_ADDR=0.0.0.0:8080`，端口 8080。reqwest 用 rustls 自带根证书，镜像里不需要 CA 证书包。
- `.github/workflows/docker.yml`：push 到 main 或 `v*` 标签时跑；`binary` 任务在 amd64（ubuntu-24.04）与 arm64（ubuntu-24.04-arm）原生 runner 上各编前端 + musl 二进制（CI 自 2026-11-12 不再跑测试，见下「CI 移除 Test 步骤」；推送前本地须跑全量测试全绿）；`docker` 任务（PR 不跑）用 buildx 各推 `amd64-<sha>` / `arm64-<sha>` 镜像，再 `imagetools create` 合成多架构清单 `:latest`、`:sha-<sha>`（打标签时另加 `:<版本号>`）。GHCR 包默认私有，工作流最后一步用 `gh api` 把它改公开（best-effort，失败不影响构建）。
- 本地编镜像：先编 musl 二进制，放进 `bin-amd64/shuku`，再 `docker build -t shuku .`（本机无 docker，未本地验证，靠 CI 首跑验证）。

## 发布（PAT 推送，2026-11-11，Ye 要求）

- 项目 PAT「legado-server Muse」（fine-grained，只限本仓库，Contents + Workflows 读写，到期 2027-11-11），存在连接器 custom.github-legado-pat，token 值只在保险库里，不进任何文件。
- 推送走 `~/workspace/custom-connectors/custom.github-legado-pat/bin/gh_push.py`（Git Data API：blob→tree→commit→更新 main，一次提交）；不要用 MCP push_files（在有提交的仓库上建 tree 会 403），不要用 MCP create_or_update_file 逐文件推（每个文件一次提交、一次批准卡）。
- `.github/workflows/*` 同样走 gh_push.py（PAT 有 Workflows 权限，App 没有）。

## CI actions 升版（2026-11-11，Ye 贴出 Node.js 20 弃用警告）

- 全部 action 升到用 Node.js 24 的大版：checkout v4→v7、setup-node v4→v7、upload-artifact v4→v7、download-artifact v4→v8、build-push-action v6→v7、login-action v3→v4、setup-buildx-action v3→v4、setup-qemu-action v3→v4（升版前逐个核对过最新 release 的 `runs.using: node24` 与大版标签存在）。dtolnay/rust-toolchain 不动（composite，无 Node 运行时）。

## 数据目录统一（2026-11-11，Ye 要求）

- 新增 `SHUKU_HOME`（默认 `.`）：书库固定在 `$SHUKU_HOME/books`，全部服务数据固定在 `$SHUKU_HOME/data`（shuku.db、index/、chapters/、covers/）。`SHUKU_BOOKS_DIR` / `SHUKU_DATA_DIR` 保留，单独覆盖时优先于 `SHUKU_HOME`。
- 不设 `SHUKU_HOME` 时行为与以前完全一致（`./books`、`./data`），已有部署不用搬数据；Docker 镜像改设 `SHUKU_HOME=/`，对应原来的 `/books`、`/data`。
- 扫描暂存目录仍在系统临时目录（`shuku-scan-*`，用完即删），不进数据目录。
- 修复经过：config 单元测试新增 `home_derivation_and_precedence`（派生 + 优先级 + 默认），全量测试 100 单元 + 2 端到端全绿。

## 改名 shuku（2026-11-11）

- 项目名 legado-server → **书库 Shuku**（仓库、crate、二进制、镜像、前端标题、PWA 名均为 shuku；中文名「书库」）。
- 环境变量前缀 `LEGADO_*` → `SHUKU_*`（`SHUKU_HOME` / `SHUKU_BOOKS_DIR` / `SHUKU_DATA_DIR` / `SHUKU_ADDR` 等全部 19 个）。**破坏性变更**：已有部署（NAS 的 docker run/compose）须把旧变量名改掉，否则用默认路径。
- 数据库文件 `legado.db` → `shuku.db`；`Database::open` 首次打开时若只有旧文件，自动改名迁移，数据不丢。
- 浏览器侧 localStorage 键 `legado.*` → `shuku.*`（阅读设置、高亮、主题），读不到新键时读一次旧键并迁移。
- OPDS feed id 从 `urn:legado:*` 改为 `urn:shuku:*`；User-Agent `shuku/0.1 (compatible; Reading3)`。
- 不改：对「Legado（阅读3）JSON 书源格式」的兼容描述仍保留（那是格式名，不是项目名）；PAT「legado-server Muse」与连接器 custom.github-legado-pat 是凭据侧的既有名字，也不动。
- 旧仓库名历史：此前 BUILD_NOTES 条目里的 legado-server 指同一项目改名前。

## CI 构建缓存（2026-11-11）

- npm 早已有缓存（setup-node `cache: npm`）。
- 新增 cargo 缓存：binary 任务用 Swatinem/rust-cache@v2（`workspaces: crates/server`，按 `matrix.arch` 分 key，amd64/arm64 各一份）。首跑只填充缓存（仍约 10 分钟），之后 test + musl 构建走增量，预期降到约 6 分钟。缓存 miss/被逐出只会回落到全量重编，不影响正确性。
- 新增 Docker 层缓存：两个 build-push 步骤各加 `cache-from/cache-to type=gha`（按架构分 scope，mode=max）。收益较小（镜像层只有 alpine 底 + COPY 二进制），主要是底包层复用。

## 删「Make package public」步骤（2026-11-11）

- 该步骤每次都回 404（`gh api PATCH /user/packages/container/shuku`，exit 1，靠 `continue-on-error` 才没拖垮 job），从 run #3 到 run #8 从没成功过。镜像本身早已是公开（匿名拉取 manifest 多次核实），可见性是套件级属性、推新版本不会变回私有，所以这步是纯噪音，已删。万一将来套件被删重建，仍需在 GHCR 网页上手动点公开（这步的 API 路径本来也调不通）。

## 修书架虚拟滚动越界崩溃（2026-11-11）

- 现象：书架筛选框打拼音时整页崩 `can't access property "tags", Z() is undefined`（Firefox 报错；`Z()` 是压缩后的书卡 accessor `b()`）。
- 根因：`Bookshelf.tsx` 用 `@tanstack/solid-virtual`，项渲染是 `b = () => books()[item.index]`。虚拟滚动的测量缓存滞后于列表缩短一拍，旧项的 index 会越界，`b()` 返 `undefined` 再读 `.tags` 就崩。筛选框无防抖，打字会并发多个 `load()`，过期响应还会覆盖新结果，加剧触发。
- 修：① 项渲染先取 `book = books()[item.index]`，越界直接 `return null`；② `load()` 加序号，丢弃过期响应；③ 筛选变化时滚回顶部。另注：书架筛选框目前是 `instr` 精确子串（不走拼音），拼音搜在全文检索页；要不要给筛选框也加拼音，待 Ye 定。

## 本书内搜索结果不全（2026-11-11）

- 现象：阅读器内搜「李茉」只显示 4 条、每条一段 200 字节录还半路截断，Ye 指结果不可能只有这几行。
- 根因：后端每章只返一个 `SnippetGenerator` 片段（正文上限 200 字），点结果只跳到章首，不定位、不标亮。
- 修：① 点结果跳章后在正文标亮该词全部出现处（复用批注的 `<mark>` 机制，新增 `mark.search-hit`），滚到第一处，并显示「本章 X 处」（`createMemo` 按章重算）；② 搜新词/清除时清掉旧标亮；③ 正文节录上限 200→400 字。全量 `cargo test --release` 100 单元 + 2 端到端全绿，前端构建通过。未做：每章返多个片段（有跳章标亮已能看到全部出现处，不再需要）。

## 书架筛选列表滞后 + 本书内搜索仍只显示片段（2026-11-11）

- 现象：Ye 发 6 张截图——书架筛「wl」显示「共 1 本」但列表还留着旧的 3 本书（筛「wlcl」显示「共 0 本」列表仍是那 3 本）；本书内搜索即使节录加到 400 字仍只是一章一个片段。
- 根因有二：① `@tanstack/solid-virtual` 的 `getVirtualItems()` 返回的是内部 store，经 `reconcile(key: "index")` 复用旧项，列表换了它也可能不更新，造成数已变、列表还是旧书（后来自行恢复，是虚拟滚动最终重测才纠正）；② 后端每章只返一个 tantivy 最佳片段，章内出现几十次也只看得到一个。
- 修：① 书架列表改由 `books()` 直接派生（`visibleBooks()`：虚拟项 + 当前 books() 一一对应，越界项滤掉），`books()` 变化时调 `virtualizer.measure()` 强制重测容器高度；② 后端新增 `content_fragments`：在存的正文里找查询词的全部字面出现位置，按前后 60/90 字切窗、重叠的合并，最多返 5 个片段（`<em>` 标亮、HTML 转义），另加 `match_count`（整章出现总次数；拼音匹配时为 0、回退到原 tantivy 节录）；③ 前端搜索页与本书内搜索结果旁显示「· X 处」。全量 `cargo test --release` 101 单元 + 2 端到端全绿，前端构建通过。

## 书架筛选支持拼音（2026-11-11）

- 现象：Ye 问筛「wlcl」为什么不显示书名「武林沉沦」的书——`wlcl` 是「武林沉沦」的拼音首字母，理应命中。
- 根因：书架筛选只是 `instr(title)` 字面子串，拼音只在全文检索页才有。
- 修：books 表新增 `title_pinyin` 列（`search::pinyin_text` 的输出：音节 + 连写 + 全部首字母前缀）；迁移时对已有行一次性回填；`upsert_book` / `upsert_books_batch` 写行时同步算好（不改 BookRow / BOOK_COLS）；`list_books` 的 q 筛选改为 `instr(title, ?1) > 0 OR instr(title_pinyin, lower(?1)) > 0`（拼音匹配不区分大小写，仍用 instr、不走 LIKE）。测试：新增 `list_books_matches_pinyin_initials`（wlcl / wl / wulin / WLCL 命中，武林字面命中，zz 无命中）。全量 `cargo test --release` 102 单元 + 2 端到端全绿。注意：已部署的库在新版启动时自动回填 title_pinyin（几千本只需一次）。

## 书架改名书库 + 最近阅读（2026-11-11）

- Ye 要求：书架改名书库，只用来搜索打开书库的书；加最近阅读，显示最近打开的 100 本书。
- 改名：App.tsx 导航页签「书架」→「书库」（品牌本来就是「📚 书库」），Bookshelf 空态文案同步改「书库为空」；组件/文件名不动。
- 只用来搜索打开：Bookshelf 剥走管理操作——工具栏「检查更新」按钮、每本缓存书的「检查」「换源」按钮与换源下拉对话框（及 `sources` / `changingId` / `changeSourceId` / `notice` 状态）。后端接口（check-update / check-updates / change-source）保留不动；「有更新」徽标仍显示（只是状态提示）。
- 最近阅读：新接口 `GET /api/books/recent`（db `recent_books(100)`：books JOIN reading_progress 按 updated_at 倒序，LIMIT 100；打开书时阅读器 loadChapter 会写进度，所以 updated_at 就是上次打开时间）。前端书库页工具栏下方加「最近阅读」区块（可点标题折叠/展开，默认展开），每行书名+作者+「第 X 章」，点开直接续读上次章节。无最近阅读时区块不显示。
- 测试：新增 `recent_books_orders_by_last_opened`（排序、last_chapter_idx/last_read_at、limit、未打开的书不出现；时间戳用 SQL 固定，避免同一秒写进度导致排序不稳）。全量 `cargo test --release` 103 单元 + 2 端到端全绿，前端构建通过。

## 本书内搜索改成逐处列出（2026-11-11）

- Ye 拍了另一家阅读器的录屏：书内搜「李茉」逐个列出全部 180 处（编号+片段+位置百分比），点一个跳去那一处。要求书库按同样的交互逻辑做。
- 先对数：那本 `【武林沉沦番外】（1-2）李茉.txt` 全文「李茉」共 177 处（直接 grep 数的），书库之前显示的 60+16+100+1=177 是对的；另一家显示 180，多出的 3 处很可能是把章节标题/目录也计了进去（内容本身只有 177 处）。
- 后端：search/mod.rs 抽出 `find_occurrences` + `highlight_window` 两个共用函数（content_fragments 改用它们，行为不变）；新增 `BookMatch {chapter_idx, chapter_title, offset, snippet}` 与 `SearchIndex::list_matches(book_id, q, limit, offset)`——同款 tantivy 查询按书过滤，取全部匹配章节，按章排序后对每章正文找全部字面出现处，每处一条（前后 60/90 字窗口，词包 `<em>`），分页返回；新接口 `GET /api/books/:id/matches`（PageQuery 上限 500/页）。拼音查询无字面出现处，返回空。
- 前端：Reader 的书内搜索改走 `api.listMatches`，结果逐个编号列出（序号+片段+章名），点一个跳去那一处：加载该章、标亮全部出现处、再把选中的那一处单独标红（`mark.search-hit-current`）并滚到它；顶栏变「第 N / 共 total 处」+ 上一处/下一处（跨章也行，超出已加载时自动加载下一页）；超过 500 处时列表底有「加载更多」。无字面结果时回退到按章节分组的旧搜索（拼音还能用）。另把阅读器左上角「‹ 书架」改成「‹ 书库」。
- 测试：新增 `list_matches_returns_every_occurrence`（总数、章节+位置排序、片段高亮、分页、按书过滤、拼音返回空）。全量 `cargo test --release` 104 单元 + 2 端到端全绿，前端构建通过。

## 阅读器仿另一家 App 的交互（2026-11-11）

- Ye 拍了另一家阅读器的两张截屏，要求把交互学过来：底部工具栏（进度滑杆 + 四个按钮）与点一下弹出的快选浮层。讨论后定稿：不要书签、不要版式；要进度滑杆、目录与搜索；点正文收起/浮出顶栏和底栏；底栏滑杆是全书进度（不是本章）；右边竖屏再加一条本章的竖向滑杆。
- 实现（Reader.tsx + styles.css，前端改动）：
  - 点正文开合：正文包一层 onClick toggleChrome（有选词时不切换，免得干扰批注）；顶栏与底栏平时由 showChrome 控制显隐（默认显示，点一下收起、再点浮出）。
  - 顶栏极简化：只剩「‹ 书库 + 书名 + 第 X 章 / 共 N 章」，目录与搜索框移到底栏。
  - 底栏（position: fixed，底部固定）：第一行「‹ 上一章 | 全书进度滑杆 | NN% | 下一章 ›」，滑杆值 =（章序号 + 本章内滚动进度）/ 总章数，拖动跟手、松手才跳章（bookDrag）；第二行「📑 目录 | 🔍 搜索 | 🔊 朗读 | ⚙️ 设置」。
  - 搜索改由底栏唤出：showSearch 面板（输入框 + 搜 + ✕），结果列表/回退/无结果只在面板打开时显示；点结果跳去后收起面板，搜索导航条（第 N / 共 total 处 + 上一处/下一处）跟顶栏一起收起。
  - 设置面板改由底栏设置按钮唤出（默认收起），语速从旧底部栏移进设置面板。
  - 右边本章竖向滑杆：position: fixed 右侧中部，writing-mode: vertical-lr，值 = 本章内滚动进度（0..100），拖动滚动本章；滚动模式用 window 滚动事件同步进度，仿真翻页模式用正文容器 onScroll。
  - 删旧底部 .toolbar（上一章/上一屏/下一屏/下一章/朗读/语速），语速进设置、翻页由竖向滑杆与键盘方向键承担。
- 前端构建通过；后端无改动。

## CI：apt 装 musl-tools 加重试（2026-11-11）

- run #17（阅读器交互重构，8dc1958）失败：amd64 job 在 `Install musl tools` 步挂了——runner 偶发解析不到 `azure.archive.ubuntu.com`（Temporary failure resolving 'azure.archive.ubuntu.com'），musl/musl-dev/musl-tools 三个包下不下来，exit 100。arm64 job 成功，docker job 因依赖失败没跑。代码本身没问题，是 runner 侧偶发 DNS 抖动。
- PAT 没有重跑 workflow 的权限（rerun-failed-jobs 返回 403），改走修 workflow 触发新一轮：`Install musl tools` 步改成最多 3 遍重试（每次失败睡 15 秒），装完校验 `command -v musl-gcc`，三次都失败才报错退出。

## CI：apt 装 musl-tools 改预编译交叉工具链（2026-11-11）

- run #18（apt 三遍重试，f5413cd）实质失败：amd64 在 `Install musl tools` 步挂了 19 分 20 秒，azure.archive.ubuntu.com 解析失败，重试 3 次仍失败；arm64 同一步卡了 21 分钟。结论：这不是一两分钟的抖动，是 Ubuntu 镜像 DNS 持续解析不到，重试修不了。
- 改法：`Install musl tools`（apt）换成 `taiki-e/setup-cross-toolchain-action@v1`（target 由 matrix 传），从 GitHub 下载预编译交叉工具链，完全不经 apt、不依赖 Ubuntu 镜像。注意：它会设 CARGO_BUILD_TARGET，cargo test 随之跑 musl 目标（同架构 runner 上可直接运行，与发版二进制一致）。

## 滑杆换功能 + 顶栏跟 chrome + 智能排版（2026-11-12）

- 滑杆换功能（Ye 拍板）：底栏滑杆改成本章滚动（scrollFrac/scrollToFrac，显示本章百分比），竖向滑杆改成全书进度（bookPct/jumpToBookPct，拖动跳章）。竖向滑杆长度 40vh → 88vh（轨道更长，章数多时好瞄准）。
- 竖向滑杆拖动提示：拖动时在滑杆左侧显示「第 X 章 / 共 N 章 · NN%」，停 1.2 秒后淡出（0.3 秒透明过渡）再移除。bookTip/tipFading 两个信号 + 两个定时器（淡出 1200ms、清除 1500ms），unmount 时清定时器。
- 顶栏跟 chrome：新增 `frontend/src/lib/readerChrome.ts` 共用信号（readerChrome/setReaderChrome）。App.tsx 的顶栏导航包进 `<Show when={route().name !== "read" || readerChrome()}>`，进阅读界面后顶栏导航也随底栏一起点按隐藏/显示；离开阅读界面后导航恒显示。Reader.tsx 的 showChrome/setShowChrome 改成该共用信号的别名，余下代码不动；进阅读界面时 onMount 重置 chrome 为显示。
- 顶栏拖动后不显示（bug）：`.reader-bar` 原来是 `position: sticky`，条件渲染（Show）后在已滚动的页面上复现时粘不住、藏在视口上方，底栏（fixed）却正常。改成 `position: fixed`（与底栏同样居中 `left: 50%; translateX(-50%); width: min(720px,100%)`，z-index 20），`.reader.chrome-on` 加 `padding-top: 52px` 免得顶栏盖住正文开头。
- 智能排版：中文排版库用 pangu.js（v10.4.1，`pangu/browser` 入口——默认入口是 Node 版，用了 `node:fs`，Vite 打包会炸；方法名是 `spaceText` 不是旧版 `spacingText`）。`disp` 里繁简转换后过 `pangu.spaceText`，中英文/数字间自动加空格（「我爱iPhone」→「我爱 iPhone」）。高亮切分（splitOnTerm/renderTextWithHighlights）在 disp 之前按原文做，pangu 只在每段切分后加空格，不整坏搜索定位与批注。
- CSS 中文排版：`.reader-content p` 加 `text-align: justify; line-break: strict; hanging-punctuation: first; text-justify: inter-ideograph`（两端对齐、严格断行、标点悬挂）。
- 测试：前端构建通过；104 单元 + 2 端到端全绿。竖向滑杆方向（往下拖=往后翻）待线上实测确认，若反了再调 direction。

## 撤 pangu + 移植 vvsolo 通用规则（2026-11-12）

- 更正上一节：pangu 已撤。Ye 发线上截图指出 pangu 处理不了这种排版（断行错误、单字孤行），且网上下载的小说排版错误多、pangu 不适合小说文件。根因不在浏览器也不在 pangu——是后端 `paragraphs_from_text` 把源文件每一行非空行都当成一个段落，定宽硬换行被当成段落边界。`npm uninstall pangu`，Reader.tsx 删 import、disp 恢复原样（只剩繁简转换）。CSS `.reader-content p` 保留 `text-wrap: pretty; word-break: keep-all; overflow-wrap: break-word;`（与后端断行修复互补，解决浏览器自身的孤行/拆词）。
- vvsolo 断行规则移植（`src/parser/mod.rs`，展示层，只影响分段，不改存档正文与搜索索引）：`fix_wrong_line_breaks`——行尾不是句末标点（。！？…”：；〗】）的换行视为错误断行，删掉换行并回上一行；主规则跑两遍（单遍会因匹配消耗行尾字符漏掉相邻断行，vvsolo 也是两遍）；另有 `，”\n`、`\n”`、`…\n…` 三组引号/省略号修正（注意：第一组源码里曾把全角引号 `”` 写成半角 `"`，已在复核时改正）。`paragraphs_from_text` 先按空行切块，块内过断行修正，再按保留下来的换行逐行成段；`\x01IMG:` 标记行 flush 当前块并单独成 img 段，永不参与合并。
- 通用规则（Ye 追加「其他合适的统一规则也搬过来」）：除断行外，再移植三条——**标点**（Punctuation：99 对半角/异体标点逐字映射为全角 + 13 组 rps 合并重复标点、省略号/破折号后缀清理、?! 连用规范化）、**空格**（Space：删汉字/全角标点及 &、/、\ 左右的多余半角空格）、**全角数字字母转半角**（NumberLetter，全角区与半角区固定差 0xFEE0，直接算不用查表）。三条都在 `paragraphs_from_text` 里每行先进 `apply_typeset`（顺序：全角转半角 → 标点 → 空格 → 断行合并），标点要在断行之前跑（半角 `.` 先变全角 `。`，断行规则才认得出句末）。不搬的：Quote（跨调用 stateful masking，`maskStore`/`maskSeq`，搬到无状态 Rust 不稳阵）、Variant（改中文字形，有搜索风险）、Unicode 转义（txt 里罕见）、Chapter/Separator/SerialNumber/English/End（结构类，与段落展示无关）。Punctuation 的 R1（重复标点）与 R2/R4 含 backreference/look-ahead，Rust `regex` 不支援，R1 改手写扫描连续相同标点，R2 拆三步、R4 改 capture group。
- epub 不併行：`paragraphs_from_text` 加 `reflow: bool` 参数。epub 的行本身就是 HTML block（`html_to_text_with_images` 每 text 节点一行、无空行分隔），併行会把标题和正文粘成一段；只有 txt/缓存纯文本才做断行合并。API 按副档名传：`.epub` → false，其余 → true；缓存的网络书（纯文本）→ true。字符级排版（标点/空格/全角转半角）两边都跑，epub 只是不併行。注意 `path` 要在 move 进 `spawn_blocking` 之前算好 reflow，移进去后不能再借用。
- 测试：106 单元 + 2 端到端全绿。新增 `paragraphs_join_hard_wrapped_lines`（截图原文三行硬换行并成一段；行尾句号的换行仍是 2 段）、`typeset_general_rules`（半角标点转全角、全角数字字母转半角、汉字间空格、重复标点合并、?! 规范化）。既有 `epub_image_paragraphs` 仍过（4 段：标题/正文/img/正文）。

## 竖向滑杆方向反了 + 顶栏重叠（2026-11-12，run #20 线上实测）

- 线上实测（run #20 部署后，全新 shuku-test profile）：(a) 底栏本章滚动通过；(b) 竖向滑杆 88vh、跳章、拖动提示「第 X 章 / 共 N 章 · NN%」格式对、2 秒内淡出，基本通过（提示 % 是拖动位置而非落章章首进度，属正常）；(f) 数字间空格与两端对齐通过；英文单词混排因书库正文里找不到英文样本，未能验证。
- (c) 失败——竖向滑杆方向反了：往下拖是从第 27 章回第 20 章（往前翻），滑块 0% 在底部、100% 在顶部。根因：竖向 range 在这套 CSS（writing-mode: vertical-lr; direction: rtl）下 0 在底、100 在顶，value 直接等于全书进度就是反的。修：value/事件里用 `100 - pct` 反一下（onInput/onChange 里的 tip 与跳章都走反后的值），CSS 不动；往下拖=往后翻，滑块位置与全书进度一致。
- (d)(e) 失败——点正文重显顶栏后，App 导航与阅读器顶栏挤在同一行重叠（导航只剩「📚 书库 检」）。根因：导航是 sticky top:0，阅读器顶栏是 fixed top:0，两栏都贴在视口顶部；`.reader.chrome-on` 的 padding-top: 52px 只给一栏留了位置。修：Reader 量导航实高（offsetHeight，换行时变高也算到，resize 时重测），阅读器顶栏 style top 顶到导航下方，正文 padding-top 改为导航高 + 顶栏高（内联样式，chrome 收起时撤掉）。
- 另：章内标题编号与顶栏差两章（如顶栏第 27 章、章内标题「第二十五章」），是书本身目录里多了两章非正文章（前言类），不是代码问题，不修。
- 注意：此修曾随 05987cc 推过一次，但随后并行的撤 pangu 提交 8ed4e90 以旧版 Reader.tsx 覆盖了它，故在 8ed4e90 之上重贴了一遍。
- 测试：前端构建通过；后端无改动。方向与重叠待线上复测确认。

## CI 移除 Test 步骤（2026-11-12）

- Ye 要求：CI 里的 `cargo test --release` 意义不大、浪费时间，移除。本地推送前已跑全量测试（106 单元 + 2 端到端），CI 再跑一次是重复；且 CI 的 test 跑 musl 目标、耗时更长。移除后 `binary` job 只剩：前端构建 → Rust 工具链 → cargo 缓存 → musl 交叉工具链 → musl 构建 → 上传。相关注释同步更新（删 cargo test 跑 musl 目标的说明、build + test jobs 的说法）。
- 注意：本地测试门禁不变，推送前仍须 `cargo test --release` 全绿。

## 前端更新后不变：补 Cache-Control（2026-11-12）

- 问题：Ye 报更新后前端一直不变。根因是后端 `static_files::serve` 完全不发 `Cache-Control`：Cloudflare（站前有 CF）按扩展名缓存 `sw.js`，浏览器也启发式缓存，SW 更新检查每次都拿到旧版 `sw.js`，见不到新 precache 清单，旧前端一直被 serve。SW 本身没问题（`registerSW.js` 注册 `/sw.js`，`sw.js` 含 `skipWaiting` + `clientsClaim` + `precacheAndRoute`）。
- 修法（`src/static_files.rs` 新增 `cache_control`，两个 serve 分支都带上）：
  - `sw.js`、`index.html`、`manifest.webmanifest`、`registerSW.js` → `no-cache`（每次必重新验证，新版一部署下次访问即生效）。
  - `assets/*`（Vite 内容 hash 文件名，内容变文件名变）→ `public, max-age=31536000, immutable`。
  - 其余（icons 等固定名文件）→ `public, max-age=3600`。
- 用户侧：部署后刷新一次页面即可自愈（浏览器拿新 `sw.js` 后 SW 自动更新）；若旧 SW 已钉死多时仍不变，清一次该站点的网站数据即可。
- 测试：107 单元 + 2 端到端全绿（新增 `static_files::tests::cache_headers`）。

## PWA 改手动更新：长缓存 + 即时更新（2026-11-12）

- Ye 要求：既要长缓存，又要能即时更新、可以手动。做法：PWA 由 `autoUpdate` 改 `prompt` 模式（`vite.config.ts` 的 `registerType`），不再后台自动更新，改由前端接管。
- 前端（`src/App.tsx` 用 `virtual:pwa-register/solid` 的 `useRegisterSW`，新增 `src/vite-env.d.ts` 引用 `vite-plugin-pwa/client` 类型）：
  - 新版 SW 就绪时（`needRefresh` 为 true）顶部弹更新横幅：「有新版本可用，点更新后立即生效（不会丢阅读进度）。」+「立即更新」（调 `updateServiceWorker(true)`，新 SW 接管并刷新）+「稍后」（关闭横幅）。
  - 导航栏加「🔄 检查更新」按钮：调 `registration.update()` 手动查一次；有新版则横幅弹出，4 秒内无动静显示「已是最新版本」3 秒；未注册 SW 或检查失败也给提示。
- 长缓存不变：带内容 hash 的 `assets/*` 仍 `immutable` 一年；`sw.js`/`index.html` 仍 `no-cache`，保证每次都能检测到新版。两者结合：平时走长缓存很快，有新版时手动一点即更新。
- 构建验证：`registerSW.js` 不再自动注入（prompt 模式），`sw.js` 仍含 `skipWaiting`；新 bundle 含更新横幅与「检查更新」文案。测试：107 单元 + 2 端到端全绿。

## 撤销 PWA 手动更新，恢复 autoUpdate（2026-11-12）

- Ye 决定还是自动更新。撤销上一节的手动更新：`vite.config.ts` 的 `registerType` 恢复 `autoUpdate`，`src/App.tsx` 的 `useRegisterSW`、更新横幅、「检查更新」按钮全部删掉，`src/styles.css` 的 `.update-banner`/`.update-check-msg` 样式删掉，`src/vite-env.d.ts`（只为 `virtual:pwa-register/solid` 类型而建）删掉。
- 长缓存 + 即时检测的根基不变：带内容 hash 的 `assets/*` 仍 `immutable` 一年；`sw.js`/`index.html` 仍 `no-cache`，保证每次都能检测到新版，后台自动更新。
- 测试：107 单元 + 2 端到端全绿（见下）。

## 竖向全书滑杆改按全本百分比滚动，不按章节跳（2026-11-12）

- Ye 要求：右侧全本滚动条不需要分章节，按全本百分比滚动就行。
- 改法（`frontend/src/pages/Reader.tsx`）：
  - 新增 `pctToChapterPos(pct)`：全书百分比换算成连续值 pos = pct/100*n（章序号 + 章内位置），target 章 = floor(pos)，章内位置 = pos - target（0..1），不取整即不按章跳。
  - `jumpToBookPct`：落到目标章的对应位置（章内 frac），而不是章首；同章时只滚动。
  - `loadChapter(i, frac?)`：新增可选章内位置参数；换章后等渲染好（requestAnimationFrame）再滚到章内位置，并按实际 offset 保存进度；不传 frac 时行为不变（滚到章首、保存 offset 0）。
  - 滑杆 onInput：同章时拖动即实时滚动（像真正的滚动条）；跨章等松手（onChange）再换章，避免拖动途中反复发请求。
  - 拖动提示 `bookTip` 去掉章节，只显示全书百分比「NN%」。
- 百分比口径不变：仍按章数均权（(章序号 + 章内滚动进度) / 总章数），即 `bookPct()` 不变；变的是落点（章内位置而非章首）与提示。
- 测试：107 单元 + 2 端到端全绿（纯前端改动，后端未动）。

## 排版大空格：去 justify + 章节缓存改 NetworkFirst（2026-11-12）

- Ye 发截图：正文每行十几个字、字距被拉到两三个字宽，比之前更糟。
- 根因（两处夹击）：
  1. 用户读到的章节是旧版未合并的短行。PWA 的 `recent-chapters` 运行时缓存是 `CacheFirst`（30 天）：之前读过这章时旧版已入缓存，后端上了 vvsolo 断行合并也不会再取新版，一直吃旧短行。已用截图原文验证：合併逻辑本身正确，21 行短行应并成 4 段（14/60/29/152 字），不是合併 bug。
  2. 短行 + `text-align: justify` + `text-justify: inter-ideograph` 是灾难：每行十几个字，justify 要填满整行宽度，字距就被拉大。justify 是 run #20 加的，之前左对齐时短行只是断行、不会有大空格，所以用户说“更糟了”。
- 修法：
  - `frontend/src/styles.css`：`.reader-content p` 删掉 `text-align: justify` 与 `text-justify: inter-ideograph`，改回左对齐（默认）；保留 `line-break: strict`、`hanging-punctuation: first`、`text-wrap: pretty`、`word-break: keep-all`、`overflow-wrap: break-word`。构建产物已确认无 justify。
  - `frontend/vite.config.ts`：`recent-chapters` 由 `CacheFirst` 改 `NetworkFirst`（`networkTimeoutSeconds: 3`）：有网时永远取新版（后端排版规则再变也能立即生效），离线或 3 秒无响应才回落缓存。离线整本功能不受影响（仍走同一缓存预取）。
- 部署后用户刷新一次即可：新 SW 接管后正文左对齐无大空格，章节走 NetworkFirst 取到已合并的新版。
- 测试：107 单元 + 2 端到端全绿（纯前端改动，后端未动）。

## 前端定期检查 SW 更新（2026-11-12）

- Ye 19:26 发截图：大空格没了（左对齐已生效），但行还是短的、没合并。
- 核实：后端 API 返第 1 章第 9、10 段是已合并的长段（147 字、107 字），合并逻辑与后端都正常。问题在用户浏览器：旧版 SW（CacheFirst）一直钉住，章节还吃旧版缓存。
- 根因：阅读器翻页是 hash 跳转（`#/read/...`），不触发浏览器的 SW 更新检查；用户长时间在同一标签页内翻页，SW 从不更新。CSS 能生效是因为 `index.html` 是 `no-cache` + 资源带内容 hash，刷新即取新版；但章节走 SW 运行时缓存，旧 SW 不更新就一直是旧内容。
- 修法（`frontend/src/App.tsx`）：`onMount` 里加定期 SW 更新检查——启动时查一次，之后每 30 分钟查一次，切回前台时也查一次（调 `registration.update()`）；有新版时 autoUpdate 自动接管并刷新。以后再有后端排版改动，不用等用户手动刷新也会自己跟上。
- 用户侧：这次要先做一次完整重载（彻底关掉标签页/应用再重开，不是翻页），新 SW 接管后章节即取已合并的新版。
- 测试：107 单元 + 2 端到端全绿（纯前端改动，后端未动）。

## 删 `word-break: keep-all`（2026-11-12）

- Ye 指出私隐标签打开也是短行，SW 缓存推断是错的（私隐标签无旧 SW、无旧缓存）。浏览器实测（桌面 Chromium）第 1 章显示为长合并段落，与 API 一致，进一步说明不是后端/缓存问题。
- 真正根因：`.reader-content p` 的 `word-break: keep-all`。Firefox 对中文的处理是 `keep-all` 禁止在汉字之间断行，只允许在标点/空格处断行；一个 147 字的长段因此只能在逗号、句号、引号处断开，变成每行 10 来字、行行停在标点后的短行。Chromium 对中文 `keep-all` 的处理不同，会在字间断行，所以桌面浏览器测出来是满行。
- 修法：`frontend/src/styles.css` 的 `.reader-content p` 删掉 `word-break: keep-all`（改回默认 `normal`，Firefox 即可在汉字间断行填满行宽）；保留 `line-break: strict; hanging-punctuation: first; text-wrap: pretty; overflow-wrap: break-word;`。构建产物已确认无 `keep-all`。
- 测试：107 单元 + 2 端到端全绿（纯 CSS 改动，后端未动）。

## 顶栏/底栏改为浮层，显示/隐藏不改变阅读区（2026-11-12）

- Ye 指出：显示/隐藏顶栏底栏时不应改变阅读区内容，现在会压阅读区窗口、导致内容变了。
- 根因：`.reader` 在 chrome 显示时加了两重 padding——CSS `.reader.chrome-on` 的 `padding-top: 52px; padding-bottom: 120px` + inline `padding-top: navH()+barH()`；另 App 顶栏 `.nav` 是 `position: sticky`，显示时占文档流空间、把内容往下推。chrome 一开一合，阅读区高度来回变、内容重新排版、可见文字跟着变。
- 修法（全部改为浮层，不占文档流空间，内容不动）：
  - `frontend/src/pages/Reader.tsx`：`.reader` 删掉 `chrome-on` class 与 inline `padding-top`；删掉只为 padding 服务的 `barH` 信号与 `barRef`。
  - `frontend/src/styles.css`：删掉 `.reader.chrome-on` 规则；新增 `.nav.nav-overlay`（`position: fixed; top: 0; left: 50%; transform: translateX(-50%); width: min(960px, calc(100% - 24px))`）。
  - `frontend/src/App.tsx`：阅读页的 `<nav>` 加 `classList={{ "nav-overlay": route().name === "read" }}`，只在阅读页变 fixed 浮层，其他页仍是 sticky。
  - 效果：顶栏、阅读器顶栏、底栏三者都浮在正文之上，点正文唤出/收起只遮住部分正文，阅读区高度与内容完全不动。`navH` 仍量导航实高，用来把阅读器顶栏顶到导航下方（fixed 元素的 offsetHeight 照常可量）。
- 测试：107 单元 + 2 端到端全绿（纯前端改动，后端未动）。

## 全量代码审查（2026-11-12）

- Ye 要求做一次全量检查、找可优化处与 bug 并修。审查分后端正确性/性能、安全与健壮性、前端、文档五个面，原始约 121 项、去重后约 110 项。
- 已修 52 项（本地，测试全绿）：
  - Critical 1 项：前端 `listSources` 解包 `{items}`（后端返 `{items,total}`，前端断言裸数组，书源页会空白）；`createSource`/`updateSource` 返回类型改 `{id}`。
  - 后端正确性：任务认领放宽到 `failed` 且退避到期自动重试、手动重试清 `next_retry_at`；`replace_chapter_issues` 包进单事务；`list_books` tag 过滤参数化；`UPSERT_BOOK_SQL` 去重；`list_all_tasks` 用 UNION ALL 一次分页（修无 type 时第一页返 2×page_size）；`delete_bookmark` 改 `(book_id,id)` 双参（修越权删书签）；`scan_once` 加 `full_scan` 参数（消失→broken 循环只在全量扫描跑）；`needs_index` 加 `status=="broken"`；`rebuild_index` 拆 `rebuild_index_inner`（出错后恢复爬虫）；`fetch_spec` 401/403 改强制真重登；`dedupe_chapters` 去重键改 `(url,name)`；`update_source` 路径 id 与 body 不一致拒绝；`delete_book` 清理失败记日志；`split_txt_chapters` 标题长度按字符计。
  - 后端性能：migrate 加 PRAGMA（synchronous=NORMAL、cache_size、busy_timeout、temp_store）与 5 个索引；`title_pinyin` 回填自愈；`fetch_raw` 响应体流式读、上限 10MB；熔断器计 429；`get_chapter` 两分支排版搬进 `spawn_blocking`；搜索 fallback snippet 懒建立；`run_js` 的 `return` 重试改 `new Function` 拼接。
  - 安全与健壮性：`trigger_scan` 路径 containment 校验；`PageQuery::page()` 钳 1..10000；数值型环境变量补上限钳制、空字串按未设置；全站加 `Cache-Control: no-store`（不覆盖静态文件已有头）；OPDS `sanitize_filename` strip 控制字符。
  - 前端：Reader（`loadSeq` 序号守卫、滚动进度 1s 防抖保存、恢复进度传像素 offset、换书复位信号、`stepMatch` 跨分页 while 加载、批注内 `splitOnTerm`、繁简 effect 序号守卫、朗读缓存中文语音、`patchSettings` clamp）；删 `api.ts` 10 个零调用者 wrapper；`JsonEditor` 只建一次；Picker iframe 加 `sandbox="allow-scripts"`；Bookshelf 筛选 250ms 防抖；SearchPage 联想序号守卫；Duplicates keep 键改 JSON；Sources 导入重置 file input；App 离线横幅事件驱动；`req()` 默认 15 秒超时；`100vh`→`100dvh`；竖向滑杆避开顶底栏浮层、宽 28px；整本离线失败章节不计 `done` 且单章 15 秒超时。
  - 文档：ARCHITECTURE（11 张表、schema 补字段、SW 策略改述、REST 清单补全）与 API.md（索引版本 3、环境变量 19 个、错误格式、stats 上限、书源字段、OPDS 分页）同步；BUILD_NOTES「已知限制」删已解决的四条，CI 一节改述（自 2026-11-12 不再跑测试）。
- 待定 21 项（未动手，需判断或实测）：SSRF 完整校验、CORS 收紧、章节级缓存、搜索 scope 改索引、爬取有界并发、`list_matches` 流式化、书库列表/联想改走 tantivy、OPDS 流式下载、任务历史归档、错误状态码区分、XPath 序号谓词分组、换源原子替换、失败章节计数、书源凭证加密、整本离线取消按钮、搜索/任务页分页、阅读器首尾常驻 padding（与浮层方案取舍，需 Ye 定）、watcher 回调 panic、QuickJS/图片/OPDS 若干待实测项、`cover` 改名与 `list_matches` 计数。
- 测试：108 单元（新增 `partial_scan_does_not_mark_others_broken`）+ 2 端到端全绿；前端构建通过。

## 顶栏/底栏默认隐藏，点正文唤出；开搜索只出搜索框（2026-11-12）

- 待定项 #19（阅读器首尾常驻 padding）的取舍，Ye 定：不加常驻 padding，改成默认隐藏浮层、点击显示。
- 修法（`frontend/src/lib/readerChrome.ts`、`frontend/src/pages/Reader.tsx`）：
  - `readerChrome` 初始值由 `true` 改 `false`；进阅读界面时 `onMount` 改设 `false`（之前是强制 `true`）。
  - 开搜索（`openSearch`）改收起顶栏/底栏（`setShowChrome(false)`，之前是强制 `true`）、只浮出搜索面板并聚焦输入框；搜索面板本身已由 `showSearch` 单独控制，不依赖顶栏/底栏。
- 测试：108 单元 + 2 端到端全绿；前端构建通过。

## 任务历史保留：30 天 + 1 万条上限（2026-11-12）

- 待定项 #16。Ye 问任务历史有无上限：此前无上限、无删除接口、无后台清理，scan_tasks/crawl_tasks 无限增长；Ye 定 30 天 + 1 万条上限。
- 修法：
  - 新增配置 `SHUKU_TASK_RETENTION_DAYS`（默认 30，钳 1..3650）与 `SHUKU_TASK_MAX_ROWS`（默认 10000，钳 100..1000000），见 `src/config.rs`。
  - 新增 `Database::prune_tasks(retention_days, max_rows)`（`src/db/mod.rs`）：先删超过保留天数的 done/failed 任务（按 created_at）；再在两表合计超上限时，按最旧优先分批删 done/failed 任务（每批 500，循环复核，跨表公平）。pending/running 任务永不删，卡住的队列不会被静默「修好」。
  - 现有缓存清理 worker（`run_cache_worker`，每天跑一次）顺带调 `prune_tasks`；main.rs 传新配置。
  - API.md 环境变量表补两项。
- 测试：新增 `prune_tasks_drops_old_terminal_and_oldest_over_cap`（旧 done/failed 删、旧 pending/running 留、超上限删最旧的 done/failed）；109 单元 + 2 端到端全绿。

## 搜索结果列表回到上次点过的匹配处（2026-11-12）

- Ye 反馈：书内搜索滚到第 100 个匹配项点开，再回搜索结果页要重新滚到第 100 个，没记住位置。
- 根因：`jumpToMatch` 会 `setShowSearch(false)`，结果列表 DOM 被卸载；再开搜索时重新挂载，滚动位置归零。
- 修法（`frontend/src/pages/Reader.tsx`）：`openSearch` 里等一帧后，若已有当前匹配项（`currentMatch() >= 0`），将 `.match-row.current` 那行 `scrollIntoView({ block: "nearest" })` 滚回视野；聚焦输入框改 `preventScroll: true`，免得聚焦把页面拉回顶栏。首次搜索（无当前匹配项）行为不变，仍聚焦输入框。
- 测试：109 单元 + 2 端到端全绿；前端构建通过。

## 待定项 #20、#21 实测（2026-11-12）

- Ye 问 20、21 能否由 Muse 自己测：可以，全部本地实测。
- #20 watcher 回调 `tokio::spawn`：**证实是真 bug**。notify 回调跑在 notify 自己的 inotify 线程（不在 Tokio runtime 内），`tokio::spawn` 一叫就 panic（"there is no reactor running"），watcher 线程死掉，文件变化再也探测不到（定期扫描兜底，所以一直没察觉）。修法（`src/scanner/mod.rs`）：起 watcher 时用 `tokio::runtime::Handle::current()` 攞定 Handle，回调用 `handle.spawn`。新增回归测试 `watcher_enqueues_scan_on_new_book_file`（起 watcher、写新书文件、断言扫描任务入队）。
- #21 之 QuickJS 正则 ReDoS：**证实不是漏洞**。新增测试 `js_redos_regex_is_interrupted`（`(a+)+$` 配 25 个 a + b，2 秒 deadline），2.00 秒准时中断返回错误——QuickJS 中断处理器在正則执行时也会被轮询。无需修。
- #21 之 OPDS 非法头值：**找到真缺口并修了**。`sanitize_filename` 只在缓存书（书名拼文件名）那支用过，本地文件那支直接用磁盘文件名拼 `Content-Disposition`，Linux 文件名可含换行符，会污染头。修法（`src/opds.rs`）：本地文件名也过 `sanitize_filename`。
- #21 之 reqwest 307/308 body 重发：**代码核实无问题**。请求 body 是 `String`（可重发），客户端没自定义 redirect 策略（用默认：跟随重定向，307/308 保留方法与 body 重发）。无需修，未做 live 测试。
- #21 之 `get_book_image` 双重解码：**边缘情况，不改**。axum 已解码一次路径后 `percent_decode` 再解一次，正常路径（含空格）下是 no-op；只有 epub 内图片路径字面含 `%20` 时会查不到，该情况极罕见，改动反而可能破坏正常路径。保持现状。

## 待定项剩余 12 项全做 + 第二轮全量检查（2026-11-12）

Ye 指令：剩下的待定全部做，做完再做一次全量检查、精简优化。12 项（#4、#5、#6、#8、#9、#11、#12、#13、#14、#15、#17、#18）全部完成。

- #4 章节级缓存：`src/api/mod.rs` 新增 `ChapterCache`（Mutex + HashMap<String,(i64,Arc<Vec<String>>)> + VecDeque LRU，最多 8 本）。`get_chapter` 本地书分支先取磁盘 mtime 校验，命中直接用缓存；只缓存章节文本，不缓存封面字节。新增测试 `chapter_cache_hit_miss_and_mtime_invalidation`、`chapter_cache_evicts_least_recently_used`。教训：Mutex 缓存 get 后先 clone Arc 断开借用再改 LRU 顺序，否则 E0502。
- #5 搜索 scope 改索引：schema 新增 `source` 字段（raw tokenizer、Basic、stored），`INDEX_VERSION` 3→4。`search()` 加 `source: Option<&str>` 参数（TermQuery Must 子句）；`index_book` 签名加 `source`（本地书 "local"、缓存书 "cached"）；`index_books` 元组变 5 元组。删已死 DB 方法 `book_ids_by_source`。部署影响：线上 index_version=3，部署后首次扫描会全量重索引一遍（预期）。
- #6 爬取章节有界并发：`fetch_chapter_texts` 抓取段改 JoinSet + Semaphore（`CRAWL_CONCURRENCY=4`）；结果按 index 收集再按 TOC 顺序写 temp 目录；暂停时 `set.abort_all()` + 删 temp。引擎每域名令牌桶（默认 2.0/s）仍是实际瓶颈。
- #8 书库列表/联想改走 tantivy：新增 `SearchIndex::search_book_ids(query, limit)`（只查 book_title/author/pinyin 三字段、expand_pinyin_query、TopDocs 取 limit*4 后按书去重保相关序）；DB 新增 `list_books_by_ids`（IN 子句保留 id 顺序）；`list_books` 有 q 走 tantivy 再 `list_books_by_ids`，无 q 走原 SQLite。`suggest` 主来源改 tantivy + 全文补足到 8。删已死 DB 方法 `suggest_books` 及其测试。DB `list_books` 的 `q` 参数与 `title_pinyin` 列保留（完整、已测试的 DB 能力，删除需 DROP COLUMN 迁移，风险大于收益）。
- #9 增量重建索引：`rebuild_index` 加 `full: bool`（默认 false 增量：只重索引 index_version < INDEX_VERSION 的书；full=true 才清索引全量重建）。`reindex_cached_books` 加 `full` 参数，增量时跳过已达标的书，索引后写回 `set_book_index_version`（新增 DB 方法）。`RebuildReq` 加 `full`，`trigger_rebuild_index` 写进 payload 并回显。`cached_books()` 返回变 4 元组（加 index_version）。
- #11 换源原子替换：`fetch_chapter_texts` 先写临时目录 `data/chapters/<book_id>.tmp-<uuid>/` 再原子交换（live→`.old-<uuid>` 备份，tmp→live，删备份；交换失败恢复旧目录）；开头清残留 `.tmp-*`；live 缺失但 `.old-*` 存在先恢复备份。原「删超出新 TOC 长度的旧文件」代码已删（交换后天然无残留）。
- #12 错误状态码：`ApiError` 改 struct `{status, error}`；`not_found`/`bad_request` 构造器；`From<anyhow::Error>`=500、`From<JoinError>`=500、`From<serde_json::Error>`=400。约 25 处调用点改完（不存在→404、参数无效→400）。教训：`spawn_blocking` 闭包返回 `anyhow::Result` 时闭包内不能用 `ApiError`（无 From 转换），改闭包返回 `ApiResult`。
- #13 XPath 序號謂詞按父节点分组（破坏性，Ye 已知情）：`apply_pos_pred` 按父节点分组、组内 1-based 判定；`//li[1]` 现在返回每个父节点下的第一个 li（标准 XPath），原来只返回全局第一个。新增测试 `xpath_positional_predicates_group_by_parent`。
- #14 失败新章节不计入 chapter_count：`process_check_update` 新章节按 TOC 顺序抓、第一个失败即 break（不写空文件、不计数），`titles` 只取成功前缀；`fetched==0` 直接返回不写 DB；下次检查从失败处重试。
- #15 cover 改名 cover_mime + 重叠词计数（破坏性，Ye 已知情）：(a) `find_occurrences` 匹配后 `i += 1`（重叠计入，如 "aa" 在 "aaa" 里算 2 次）；高亮仍非重叠。(b) DB 列 `cover`→`cover_mime`（migrate 前 RENAME COLUMN；ALTER 循环同步改）；`BookRow.cover_mime`、`BOOK_COLS`、`UPSERT_BOOK_SQL`、全部测试 fixture、scanner/scheduler/source/opds 的 `cover: String::new()` 全改。未改名：`ParsedBook.cover`（图片字节）、`item.cover`、`covers/` 目录、`/cover` 路由。API get_book 详情 JSON 键 `"cover"`→`"cover_mime"`；前端 `api.ts`、`Bookshelf.tsx` 同步改完。
- #17 整本离线取消按钮（前端）：`downloadBook` 加 `options?: {signal?: AbortSignal}`（预取与每章都带 signal、取下一章前查 aborted、每章 fetch 用 `AbortSignal.any([signal, AbortSignal.timeout(15000)])`，取消正常 resolve）；`Reader.tsx` 每次下载新建 AbortController、新增 `cancelDownload()` 与下载中「取消」按钮、离线卡片加文案「最多 5000 章、保留 30 天」。
- #18 搜索页与任务页加载更多（前端）：`api.ts` 的 `listTasks` 加 `page/page_size` 参数；`SearchPage.tsx`、`Tasks.tsx` 各加 page/total/loadingMore 与 `loadMore()`（追加下一页），按钮「加载更多（还剩 N 个/条）」仅 loaded<total 时显示；Tasks 刷新/换筛选重置到第 1 页。

第二轮全量检查（本节后半）：
- `cargo check` 仅剩 2 个警告：`static_files.rs` 的 `content_type`、`cache_control` 在本地无 `frontend/dist` 时 never used——CI 有 dist 就用到，不是真死代码，不动。
- `fetch_chapter_texts` JoinSet 提前返回时任务被 detach：任务只做网络抓取、不写磁盘（写盘在主循环收集结果后），无副作用，安全。
- `list_books_by_ids` IN 子句最多 1000 个占位符，rusqlite 捆绑的 SQLite（≥3.32）上限 32766，无问题。
- 文档同步：`docs/API.md`（错误码 404/400/500、rebuild-index 的 full、get_book 的 cover_mime）、`docs/ARCHITECTURE.md`（搜索/联想/书库列表走 tantivy）、本 BUILD_NOTES。
- 测试：`cargo test --release` 115 单元 + 2 端到端全绿（基线 111 + 新增 5：xpath 分组 1、chapter_cache 2、search source 1、search_book_ids 1；删 suggest_books 测试 1）。
