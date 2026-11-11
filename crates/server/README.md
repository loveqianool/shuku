# shuku

自托管小说服务（对标阅读3服务器版）：本地书库 + 阅读3 JSON 书源引擎 + tantivy 全文检索 + SolidJS PWA 前端，单静态二进制。

## 编译

```bash
# 开发 / GNU
cargo build --release

# 静态单 binary（musl，需 musl-gcc）
cargo build --release --target x86_64-unknown-linux-musl
```

> 前端 dist 在构建期嵌入：`frontend/dist` 存在时由 `include_dir` 打包进二进制；不存在时编译仍通过，但只提供占位页（见 `crates/server/BUILD_NOTES.md`）。

## 运行

```bash
SHUKU_BOOKS_DIR=./books \
SHUKU_DATA_DIR=./data \
SHUKU_ADDR=0.0.0.0:8080 \
./target/release/shuku
```

环境变量：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `SHUKU_BOOKS_DIR` | `./books` | 本地 epub/txt 书库目录（递归扫描） |
| `SHUKU_DATA_DIR` | `./data` | SQLite + tantivy 索引 + 缓存章节文件 |
| `SHUKU_ADDR` | `0.0.0.0:8080` | 监听地址 |
| `SHUKU_HTTP_PROXY` / `SHUKU_SOCKS_PROXY` | — | 书源抓取代理（可选） |
| `RUST_LOG` | `shuku=info` | 日志级别 |

首次启动会自动增量扫描书库并建索引；后续文件变动由 notify 监听 + 周期扫描补齐。

## API

完整契约见 `docs/API.md`。要点：

- `GET /api/books` 书架分页筛选；`GET /api/books/:id/chapters/:idx` 正文段落 `[{text, img}]`
- `GET /api/search?q=&scope=local|cached|all` tantivy 章节检索，snippet 用 `<em>` 高亮
- 书源 CRUD / 导入导出：`/api/sources`；测试：`POST /api/sources/:id/test`（直连执行，不排队；返回 raw/extracted/js_output/logs）
- 进度/书签/标签：`/api/books/:id/progress|bookmarks|tags`
- 任务：`/api/tasks`、`/api/tasks/scan`、`/api/tasks/crawl`、`/api/tasks/rebuild-index`

## 架构

见 `docs/ARCHITECTURE.md`。关键约束：全文检索只走 tantivy（禁 SQLite LIKE）；HTML 解析与 JS 沙箱全部在 `spawn_blocking`；rquickjs 沙箱仅做字符串清洗（禁网络/文件/定时器，内存上限 16MB + 中断超时）；扫描与爬虫是两套独立 SQLite 队列 + 域名令牌桶 + 熔断；三级缓存 search 60s / toc 10min / content 24h。

## 自测

```bash
cargo test            # 单元测试：章节切分、规则校验、沙箱、令牌桶、熔断、增量扫描、检索高亮
cargo test --test integration   # 端到端：样书扫描 → 书架 → 章节 → 检索 → 进度
```
