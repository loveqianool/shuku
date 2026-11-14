# shuku — 自托管小说服务（对标阅读3服务器版）

单静态二进制 + 内嵌前端的常驻小说服务：本地书库扫描索引、tantivy 全文检索、阅读3 JSON 书源引擎、SolidJS PWA 阅读器。

## 编译

```sh
# 前端（先编前端，后端会把 dist 嵌进二进制）
cd frontend && npm install && npm run build && cd ..

# 后端（gnu）
cd crates/server && cargo build --release

# 后端（musl 单静态二进制，无外部依赖）
cargo build --release --target x86_64-unknown-linux-musl
# 产物：target/x86_64-unknown-linux-musl/release/shuku（约 25MB）
```

前置：Rust stable（`rustup target add x86_64-unknown-linux-musl`）、musl 交叉工具链（CI 用 `taiki-e/setup-cross-toolchain-action` 的预编译工具链；本地可用 `rustup target add` + musl-gcc）、Node 20+。

## 运行

```sh
SHUKU_HOME=/srv/shuku \
SHUKU_ADDR=0.0.0.0:8080 \
./shuku
```

- `SHUKU_HOME`：服务根目录，默认 `.`（当前目录）；书库和数据都按固定结构放它下面：

```text
$SHUKU_HOME/
  books/      本地书库（epub/txt），默认 $SHUKU_HOME/books
  data/       服务数据，默认 $SHUKU_HOME/data
    shuku.db     SQLite 元数据（书/书源/任务/进度/书签）
    index/        tantivy 章节索引
    chapters/     缓存书的章节正文（<book_id>/<序号>.txt）
    covers/       epub 封面字节（<book_id>）
```

- `SHUKU_BOOKS_DIR` / `SHUKU_DATA_DIR`：单独覆盖书库或数据目录，优先级高于 `SHUKU_HOME`；不设就走上面的固定结构。Docker 镜像里 `SHUKU_HOME=/`，对应 `/books` 和 `/data`。
- `SHUKU_ADDR`：监听地址，默认 `0.0.0.0:8080`
- 完整环境变量列表见 `docs/API.md`。
- 打开浏览器访问 `http://<host>:8080/` 即是 PWA 阅读器；书源、任务、测试面板都在前端里。

## Docker

```sh
docker run -d --name shuku \
  -p 8080:8080 \
  -v /path/to/books:/books:ro \
  -v /path/to/data:/data \
  ghcr.io/loveqianool/shuku:latest
```

- 镜像是 amd64 + arm64 双架构（`docker pull` 自动按本机架构取）；`/books` 放书（只读挂载即可），`/data` 存数据库、索引、缓存章节，必须持久化；端口 8080；环境变量同上（`SHUKU_*`）。
- CI（`.github/workflows/docker.yml`）：推 main 或 `v*` 标签时自动编 musl 二进制、建双架构镜像推 GHCR（`:latest`、`:sha-<提交>`，打标签时另加 `:<版本号>`）；PR 只编不推。CI 不跑测试，推送前本地须 `cargo test --release` 全绿。

## 自测

```sh
cd crates/server && cargo test   # 单元 + 端到端，全绿才推
```

## 结构

- `crates/server/` — Rust 后端（Axum + SQLite + tantivy/jieba + 书源引擎 + 双队列调度）
- `frontend/` — SolidJS + Vite + PWA 阅读器
- `docs/` — `ARCHITECTURE.md`（架构/约束）、`API.md`（REST 契约）
- `docs/CHANGELOG.md` — 依赖版本决策、已修复 bug、已知限制

## 已知限制（v0.1.0）

- 扫描已并行化（解析并发 + 索引批量提交 + 入库整批事务，见 docs/CHANGELOG.md）：4 核机上 200 本 txt（159MB、1 万章）首次全量约 15 秒（原来串行约 21 秒）；瓶颈在 tantivy 建段/存正文的单线程部分，再加线程收益很小，十几万本首次全量仍需数小时，之后靠 mtime 增量。
- 书源高级规则已实现（2026-11-10）：POST 搜索、`{{JS}}` URL 模板、登录（loginUrl + loginCheckJs + 按书源隔离 cookie jar）、nextTocUrl/nextContentUrl 跟随、搜索多页、webJs（sourceRegex→webJs→content→replaceRegex）。XPath/`@json:` 解析已实现（2026-11-10，见 docs/CHANGELOG.md）。webJs 沙箱无 DOM（`document` 不可用）；`loginUi`（WebView 填表登录）不支持；ruleToc 的 preUpdateJs/formatJs 只做保存时语法校验、不执行。
