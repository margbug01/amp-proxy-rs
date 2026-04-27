# amp-proxy-rs

[amp-proxy](../amp-proxy) 的 Rust 移植版。**功能完整且端到端验证过**：自定义
provider 路由、配置热重载、四个协议翻译器、`init` 向导、结构化访问日志。

**Release 构建产物 3.6 MB**（Windows x64，开 LTO + strip + opt-z）；
`cargo test` 共 **120 个单测全过**。

## 与 Go 版的差异

Rust 版**功能性 ≈ Go 版 + 两项额外能力**：

1. **`:streamGenerateContent` 翻译** — Go 版对这条路径直接 fallthrough 到
   ampcode.com（消耗 Amp credits），Rust 新增了
   [`gemini_stream_translator`](src/customproxy/gemini_stream_translator.rs)
   把它接到 custom provider。Amp CLI `finder` sub-agent 因此**完全不再泄露
   credits**。
2. **AMP-CLI / Vertex AI 路径变体** — Amp CLI 给 google provider 发的请求
   形如 `/api/provider/google/v1beta1/publishers/google/models/<model>:<action>`，
   带额外的 `publishers/google/` 段。Go 版 gin router 未注册这条 pattern，
   Rust 在 `amp::routes` 里补齐了。

完整变更清单见 [CHANGELOG.md](CHANGELOG.md)，派生关系见 [NOTICE.md](NOTICE.md)。

## 功能完成度

| 模块 | Go 版 | Rust 版 |
|---|---|---|
| 配置加载 + Validate | ✅ | ✅ `src/config.rs` |
| API key 鉴权（Bearer / x-api-key） | ✅ | ✅ `src/auth.rs` |
| 配置热重载（mtime 轮询） | ✅ | ✅ `src/main.rs::watch_config` |
| `amp-proxy init` 向导 | ✅ | ✅ `src/init.rs` |
| Custom provider 注册表 + 按 model 路由 | ✅ | ✅ `src/customproxy/mod.rs` |
| 路径剥离（`/api/provider/<name>/v1...` → upstream leaf） | ✅ | ✅ `src/customproxy/extract_leaf.rs` |
| 重试 transport（瞬态错误一次重试） | ✅ | ✅ `src/customproxy/retry_transport.rs` |
| 模型映射（exact + 顺序 regex） | ✅ | ✅ `src/amp/model_mapping.rs` |
| Thinking suffix 解析（`(high)` / `(16384)` 等） | ✅ | ✅ `src/thinking.rs` |
| Anthropic `/v1/messages` 流式升级 | ✅ | ✅ `src/customproxy/sse_messages_collapser.rs` |
| OpenAI Responses SSE rewriter（`response.completed` patch） | ✅ | ✅ `src/customproxy/sse_rewriter.rs` |
| Gemini `:generateContent` ↔ OpenAI Responses | ✅ | ✅ `src/customproxy/gemini_translator.rs` |
| **Gemini `:streamGenerateContent` ↔ OpenAI Responses SSE** | ❌ | ✅ `src/customproxy/gemini_stream_translator.rs` |
| Responses ↔ chat/completions translator（请求 + 响应） | ✅ | ✅ `src/customproxy/responses_translator.rs` |
| Responses SSE 流式 translator | ✅ | ✅ `src/customproxy/responses_stream_translator.rs` |
| ampcode.com 兜底反代 | ✅ | ✅ `src/proxy.rs` |
| Custom provider 转发（`request_overrides` 合并、SSE 折叠等） | ✅ | ✅ `src/amp/proxy.rs` |
| Gemini bridge（translate 模式） | ✅ | ✅ `src/amp/gemini_bridge.rs` |
| 路由决策（`AmpRouteType` 五分支） | ✅ | ✅ `src/amp/fallback_handlers.rs` |
| **AMP-CLI / Vertex `publishers/google/models/...` 路由** | ❌ | ✅ `src/amp/routes.rs` |
| **结构化访问日志（含 BILLABLE 警告）** | 部分 | ✅ `src/amp/routes.rs` + `src/proxy.rs` |

## 测试覆盖

```bash
cargo test
# test result: ok. 120 passed; 0 failed
```

重点模块测试数量：
- `customproxy::tests` — Registry + lookup（5 个）
- `customproxy::sse_*` — collapser/rewriter 流式正确性（10+ 个）
- `customproxy::gemini_translator` — 请求/响应双向（5 个）
- `customproxy::gemini_stream_translator` — SSE 状态机（3 个）
- `customproxy::responses_translator` — 字段映射、tool 翻译（7 个）
- `customproxy::responses_stream_translator` — Responses SSE 翻译（3 个）
- `amp::fallback_handlers` — 五种 RouteType 决策 + Vertex path 提取（10 个）
- `init` — 配置生成 + 解析往返（3 个）

## 端到端验证

通过真实 Amp CLI 会话验证四条主要数据路径，全部通过：

| 路径 | 验证情形 |
|---|---|
| `claude-sonnet-4-6` 主 agent + librarian → augment-gpt | 9 次调用全 200，零 WARN |
| `gemini-3-flash-preview` finder → augment-gpt via translate | 17 次调用全 200，零 BILLABLE 模型流量 |
| `gpt-5.4` → DeepSeek 经 Responses↔chat/completions | 多轮 reasoning + tool_use 全 200 |
| `/api/internal`、`/api/telemetry` 控制面 → ampcode.com | 正确兜底（这些本来就该走 ampcode） |

## 构建 & 运行

```bash
cd amp-proxy-rs

# 调试构建
cargo run -- --config config.yaml

# 发布构建（3.6 MB）
cargo build --release
ls -lh target/release/amp-proxy.exe

# 交互式生成 config.yaml
./target/release/amp-proxy.exe init
```

冒烟测试：

```bash
./target/release/amp-proxy.exe --config config.yaml &
curl http://127.0.0.1:8317/healthz                                       # 200
curl -i http://127.0.0.1:8317/v1/messages -X POST -d '{}'                # 401
curl -i -H "x-api-key: <你的 key>" http://127.0.0.1:8317/v1/messages -X POST \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-opus-4-6","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}'
```

## 怎么读 `run.log`

amp-proxy-rs 默认 INFO 日志，每个请求两行：**入口**（路由决策）+ **出口**（状态码 + 时延）。
日常运维和回归排查只看以下几个 prefix 就够：

| 日志前缀 | 出现位置 | 含义 |
|---|---|---|
| `amp router: request` | 任何 Amp CLI 模型请求 | 决策已生成；带 `route=...` `requested_model=...` `resolved_model=...` `provider=...` `gemini_translate=...` |
| `amp router: response` | 同上配对 | 终态；带 `status=...` `elapsed_ms=...` |
| `customproxy: forwarding` | 命中 custom provider 时 | 实际外发；带 `upgraded_messages=...` `translate_responses=...` |
| `gemini-translate: forwarding` | Gemini 翻译器走流式 / 非流式分支前 | 带 `stream=...` `in_bytes=...` `translated_bytes=...` `url=...` |
| `ampcode fallback: forwarding (BILLABLE — uses Amp credits)` | 落到 ampcode.com 兜底 | 控制面流量正常出现这条；**模型流量出现这条就是路由 miss / 翻译器拒收，需要排查** |
| `WARN gemini-translate: response translation failed` | translator 解析失败 | 上游回了非预期 shape；客户端会拿到原始字节 |

排查节奏：

```bash
# 看是否有模型流量泄露到 ampcode.com（控制面那几条排除掉）
grep BILLABLE run.log | grep -v "/api/internal\|/api/telemetry\|/news.rss"

# 看翻译器是否有失败
grep WARN run.log

# 看某次请求的完整链路（拼路径关键字）
grep "publishers/google/models/gemini-3-flash-preview" run.log
```

要更详细，临时开 debug：

```bash
RUST_LOG=amp_proxy=debug,tower_http=info ./target/release/amp-proxy.exe --config config.yaml
```

## 学习路线（剩余的 Rust 练手机会）

1. **流式上行替换 buffered body** — `src/proxy.rs::forward` 和
   `src/amp/routes.rs::handle` 现在用 `axum::body::to_bytes` 把请求体读到内存。
   把它换成 `impl Stream<Item = Bytes>` 真正的流式转发，能把内存占用从"按最大请求体"
   降到"按 chunk"——这是 Rust 异步真正的甜区。
2. **`gjson` 风格的零拷贝 JSON 访问** — 现在所有 translator 走 `serde_json::Value`
   反序列化。引入 `simd-json` 或自己写一个 path-based 访问器，比对 Go 版 `gjson`
   在大 body 下的延迟差异。
3. **替换 `Arc<RwLock>` 为 `arc_swap::ArcSwap`** — `auth.rs` 现在用 `RwLock<HashSet>`，
   读路径每次都得 `.read()`，换成 `ArcSwap` 是经典优化（`customproxy::Registry`
   已经这么做了）。
4. **Capture middleware** — Go 版 `internal/server/body_capture.go` 把请求/响应 body
   写到磁盘做调试，Rust 版没移植。是个学 `tower::Layer` 自定义 middleware 的好题目。
5. **Access log middleware with model peek** — 同上，`server/access_log.go` 在 Rust
   这边也是空的（虽然 `amp::routes::handle` 自己已经打了一对 INFO 日志覆盖掉大部分用途）。
6. **CI workflow** — `.github/workflows/ci.yml` 跑 `cargo test` + `cargo build --release`，
   现在还没有。
7. **`scripts/restart.ps1`** — Go 版有 PowerShell 启停脚本，Rust 还在靠手动
   `taskkill /F /IM amp-proxy.exe` + 重启。

## 模块导览

| 文件 | 角色 | 对应 Go 版 |
|---|---|---|
| `src/config.rs` | YAML 解析 + Validate | `internal/config/config.go` |
| `src/auth.rs` | API key 中间件 | `internal/auth/` |
| `src/proxy.rs` | ampcode 兜底反代（带 BILLABLE 警告） | `internal/amp/proxy.go` |
| `src/server.rs` | axum Router 装配 | `internal/server/server.go` |
| `src/main.rs` | 入口 + 信号处理 + 配置 watcher | `cmd/amp-proxy/main.go` |
| `src/error.rs` | `AppError` | （Go 用裸 `error`） |
| `src/init.rs` | `amp-proxy init` 向导 | `cmd/amp-proxy/init.go` |
| `src/bodylimit.rs` | 限长读取 | `internal/bodylimit/` |
| `src/thinking.rs` | thinking 后缀解析 | `internal/thinking/` |
| `src/util.rs` | 路径辅助 | `internal/util/` |
| `src/amp/mod.rs` | `AmpModule` + 子模块汇总 | `internal/amp/amp.go` |
| `src/amp/fallback_handlers.rs` | 路由决策（含 Vertex path 模型提取） | `internal/amp/fallback_handlers.go` |
| `src/amp/model_mapping.rs` | 模型映射 | `internal/amp/model_mapping.go` |
| `src/amp/secret.rs` | API key 生成 | `internal/amp/secret.go` |
| `src/amp/gemini_bridge.rs` | Gemini translate 模式桥（流式 / 非流式分发） | `internal/amp/gemini_bridge.go` |
| `src/amp/proxy.rs` | custom provider 转发 | `internal/customproxy/customproxy.go::buildProxy` |
| `src/amp/routes.rs` | amp axum router（含 Vertex path 模式 + 结构化访问日志） | `internal/amp/routes.go` |
| `src/amp/response_rewriter.rs` | 响应后处理 | `internal/amp/response_rewriter.go` |
| `src/customproxy/mod.rs` | Provider Registry | `internal/customproxy/customproxy.go` |
| `src/customproxy/extract_leaf.rs` | 路径剥离 | `internal/customproxy/customproxy.go::extractLeaf` |
| `src/customproxy/retry_transport.rs` | 重试 transport | `internal/customproxy/retry_transport.go` |
| `src/customproxy/sse_messages_collapser.rs` | Anthropic SSE → JSON | `internal/customproxy/sse_messages_collapser.go` |
| `src/customproxy/sse_rewriter.rs` | OpenAI Responses SSE 修补 | `internal/customproxy/sse_rewriter.go` |
| `src/customproxy/gemini_translator.rs` | Gemini `:generateContent` ↔ Responses | `internal/customproxy/gemini_translator.go` |
| `src/customproxy/gemini_stream_translator.rs` | **Gemini `:streamGenerateContent` ↔ Responses SSE** | （Go 版无对应实现） |
| `src/customproxy/responses_translator.rs` | Responses ↔ chat/completions | `internal/customproxy/responses_translator.go` |
| `src/customproxy/responses_stream_translator.rs` | Responses 流式 translator | `internal/customproxy/responses_stream_translator.go` |
