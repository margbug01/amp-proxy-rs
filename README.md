# amp-proxy-rs

[Sourcegraph Amp CLI](https://ampcode.com) 的反向代理。把指定 model 路由到你自己
搭的 OpenAI 兼容网关、做协议翻译、对未映射流量兜底回 ampcode.com。

**Release 构建产物 3.6 MB**（Windows x64，开 LTO + strip + opt-z）；
`cargo test` 共 **120 个单测全过**。

## 为什么需要

把 Amp CLI 指向你自己的网关而不是 `ampcode.com`，你能决定用哪个 model、怎么计费。
amp-proxy-rs 处理这中间所有脏活：

- **按 model 路由** —— 请求 body 的 `model` 字段决定上游
- **配置热重载** —— 改 `config.yaml` 不用重启
- **协议翻译** —— Amp CLI 的 sub-agent 同时用 Anthropic Messages、OpenAI Responses、
  Gemini generateContent 三种协议，你的网关只要会其中一种就行
- **ampcode.com 兜底** —— 未映射的 model 仍可用，只是会消耗 Amp credits（日志会显式标 BILLABLE）

## 工作原理

```
Amp CLI
  │
  │  POST /api/provider/<name>/v1/...   /  /v1beta/models/...:generateContent
  ▼
amp-proxy-rs
  │
  ├── healthz / 鉴权 (api-keys)
  │
  ├── 路由决策 (FallbackHandler::decide)
  │     1. 提取 model（body 优先，URL fallback）
  │     2. 应用 model-mappings
  │     3. 在 custom-providers Registry 里找 provider
  │     4. 命中 → 转发；未命中 → ampcode.com 兜底
  │
  ├── Custom provider 转发
  │     - Anthropic /v1/messages: stream 升级 + SSE 折叠回 JSON
  │     - OpenAI /v1/responses: 可选翻译成 chat/completions
  │     - Gemini :generateContent / :streamGenerateContent: 翻译成 OpenAI Responses
  │
  └── ampcode.com 兜底（标 BILLABLE）
```

## 功能矩阵

| 能力 | 实现位置 |
|---|---|
| YAML 配置加载 + Validate | `src/config.rs` |
| API key 鉴权（`x-api-key` 或 `Authorization: Bearer`） | `src/auth.rs` |
| 配置热重载（mtime 轮询） | `src/main.rs::watch_config` |
| `amp-proxy init` 交互向导 | `src/init.rs` |
| Custom provider 注册表（按 model 路由） | `src/customproxy/mod.rs` |
| 路径剥离（`/api/provider/<name>/v1...` → upstream leaf） | `src/customproxy/extract_leaf.rs` |
| 重试 transport（瞬态错误一次重试） | `src/customproxy/retry_transport.rs` |
| 模型映射（exact + 顺序 regex） | `src/amp/model_mapping.rs` |
| Thinking suffix 解析（`(high)` / `(16384)` 等） | `src/thinking.rs` |
| Anthropic `/v1/messages` 流式升级 | `src/customproxy/sse_messages_collapser.rs` |
| OpenAI Responses SSE rewriter（`response.completed` patch） | `src/customproxy/sse_rewriter.rs` |
| Gemini `:generateContent` ↔ OpenAI Responses 翻译 | `src/customproxy/gemini_translator.rs` |
| Gemini `:streamGenerateContent` ↔ Responses SSE 流式翻译 | `src/customproxy/gemini_stream_translator.rs` |
| OpenAI Responses ↔ chat/completions 翻译（请求 + 响应） | `src/customproxy/responses_translator.rs` |
| Responses SSE 流式 translator（chat-only 上游） | `src/customproxy/responses_stream_translator.rs` |
| ampcode.com 兜底反代（带 BILLABLE 警告） | `src/proxy.rs` |
| Custom provider 转发（request_overrides 合并、SSE 折叠等） | `src/amp/proxy.rs` |
| Gemini bridge（translate 模式 stream + non-stream 分发） | `src/amp/gemini_bridge.rs` |
| 路由决策（5 类 RouteType） | `src/amp/fallback_handlers.rs` |
| AMP-CLI / Vertex AI publishers/google/models 路由模式 | `src/amp/routes.rs` |
| 结构化访问日志（`amp router: request/response` 配对） | `src/amp/routes.rs` |

## 端到端验证

通过真实 Amp CLI 会话验证四条主要数据路径：

| 路径 | 验证情形 |
|---|---|
| `claude-sonnet-4-6` 主 agent + librarian → augment 风格上游 | 9 次调用全 200，零 WARN |
| `gemini-3-flash-preview` finder → augment 上游 via translate | 17 次调用全 200，零 BILLABLE 模型流量 |
| `gpt-5.4` → DeepSeek 经 Responses↔chat/completions 翻译 | 多轮 reasoning + tool_use 全 200 |
| `/api/internal`、`/api/telemetry` 控制面 → ampcode.com | 正确兜底（这些本来就该走 ampcode） |

## 构建 & 运行

```bash
# 调试构建
cargo run -- --config config.yaml

# 发布构建（3.6 MB）
cargo build --release
ls -lh target/release/amp-proxy.exe   # Windows
ls -lh target/release/amp-proxy       # Linux / macOS

# 交互式生成 config.yaml
./target/release/amp-proxy init
```

最简配置 `config.yaml`：

```yaml
host: "127.0.0.1"
port: 8317
api-keys:
  - "change-me"
ampcode:
  upstream-url: "https://ampcode.com"
  upstream-api-key: ""           # Amp session token，可空
  custom-providers:
    - name: "my-gateway"
      url: "http://host:port/v1"
      api-key: "your-bearer-token"
      models:
        - "gpt-5.4"
        - "gpt-5.4-mini"
      responses-translate: true  # chat/completions-only 网关才需要
  model-mappings:
    - from: "claude-opus-4-6"
      to: "gpt-5.4(high)"
  force-model-mappings: true
  gemini-route-mode: "translate"
```

完整字段见 [`config.example.yaml`](config.example.yaml)。

把 Amp CLI 指过来：

```bash
export AMP_URL=http://127.0.0.1:8317
export AMP_API_KEY=<config.yaml 里的 api-keys 之一>
amp
```

冒烟测试：

```bash
./target/release/amp-proxy --config config.yaml &
curl http://127.0.0.1:8317/healthz                                       # 200
curl -i http://127.0.0.1:8317/v1/messages -X POST -d '{}'                # 401（无 api key）
curl -i -H "x-api-key: <你的 key>" http://127.0.0.1:8317/v1/messages -X POST \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-opus-4-6","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}'
```

## 怎么读 `run.log`

默认 INFO 日志，每个请求两行：**入口**（路由决策）+ **出口**（状态码 + 时延）。
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
RUST_LOG=amp_proxy=debug,tower_http=info ./target/release/amp-proxy --config config.yaml
```

## 测试

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

## 路线图 / Good first issues

1. **流式上行** —— `src/proxy.rs::forward` 和 `src/amp/routes.rs::handle` 现在
   `axum::body::to_bytes` 把请求体读到内存。换成 `impl Stream<Item = Bytes>` 真正的
   流式转发，把内存占用从"按最大请求体"降到"按 chunk"。
2. **零拷贝 JSON 路径访问** —— 所有 translator 走 `serde_json::Value` 反序列化。
   引入 `simd-json` 或自己写一个 path-based 访问器，可显著减少大 body 时延。
3. **替换 `Arc<RwLock>` 为 `arc_swap::ArcSwap`** —— `auth.rs` 现在用
   `RwLock<HashSet>`，读路径每次都得 `.read()`，换成 `ArcSwap` 是经典优化（`customproxy::Registry`
   已经这么做了）。
4. **Capture middleware** —— `tower::Layer` 自定义 middleware 把请求/响应 body 写到磁盘
   做调试。
5. **`scripts/restart.ps1`** —— PowerShell 启停脚本（`taskkill` + 重启 + 重定向日志）。
6. **CI workflow** —— `.github/workflows/ci.yml` 跑 `cargo test` + `cargo build --release`。

## License

MIT。归属与衍生关系详见 [NOTICE.md](NOTICE.md)。
