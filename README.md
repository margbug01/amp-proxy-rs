<div align="center">

# amp-proxy-rs

**专注的 [Sourcegraph Amp CLI](https://ampcode.com) 反向代理 · Rust 实现**

[![CI](https://github.com/margbug01/amp-proxy-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/margbug01/amp-proxy-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange?logo=rust&logoColor=white)](Cargo.toml)
[![Binary](https://img.shields.io/badge/binary-3.7%20MB-blue)](https://github.com/margbug01/amp-proxy-rs/releases)

把指定 model 路由到你自己的 OpenAI 兼容网关 · 五个协议翻译器 · 混合流式转发 · 配置热重载

</div>

---

## 亮点

- 🪶 **单文件 3.7 MB** —— LTO + strip + opt-z，零运行时依赖
- 🔀 **五个协议翻译器** —— Anthropic Messages / OpenAI Responses ↔ chat-completions / Gemini `:generateContent` / **Gemini `:streamGenerateContent`**（上游 Go 版没有）
- 🚿 **混合流式** —— peek 请求体前 16 KiB 做路由决策，剩余字节直接 stream 到上游，不占用内存
- 🔁 **配置热重载** —— `mtime` 轮询，改 `config.yaml` 不用重启
- 💸 **`BILLABLE` 警告** —— ampcode.com 兜底每次都打日志，credit 漏出秒级可见
- 🧪 **142 个单元测试 + 端到端验证** —— 真实 Amp CLI 会话验证过 main agent / librarian / finder / DeepSeek 多轮 tool use

---

## 快速开始

```bash
git clone https://github.com/margbug01/amp-proxy-rs.git
cd amp-proxy-rs

# 1. 构建（约 1 分钟）
cargo build --release

# 2. 交互式生成 config.yaml
./target/release/amp-proxy init

# 3. 启动
./target/release/amp-proxy --config config.yaml
```

把 Amp CLI 指过来：

```bash
export AMP_URL=http://127.0.0.1:8317
export AMP_API_KEY=<config.yaml 里的 api-keys 之一>
amp
```

> Windows 下用 [`scripts/restart.ps1`](scripts/restart.ps1) 一键启停 + 日志重定向。

---

## 工作原理

```
                          ┌──────────────────────────┐
                          │       Amp CLI            │
                          └────────────┬─────────────┘
                                       │  HTTPS
                                       ▼
                          ┌──────────────────────────┐
                          │    amp-proxy-rs          │
                          │  ──────────────────      │
                          │  1. healthz / 鉴权        │
                          │  2. peek 16 KiB          │
                          │  3. FallbackHandler      │
                          │     ┌────────┐           │
                          │     │ model? │           │
                          │     └────┬───┘           │
                          └──────────┼───────────────┘
                                     │
              ┌──────────────────────┼─────────────────────┐
              │                      │                     │
              ▼                      ▼                     ▼
    ┌─────────────────┐   ┌──────────────────┐   ┌──────────────────┐
    │ custom-provider │   │ Gemini bridge    │   │ ampcode.com      │
    │ (你的网关)       │   │ (translate)      │   │ (兜底·BILLABLE)   │
    └─────────────────┘   └──────────────────┘   └──────────────────┘
       │  /messages           /generateContent       /api/internal
       │  /responses          /streamGenerateContent  /api/telemetry
       │  /chat/completions
       │
       └─ Anthropic SSE 升级 + 折叠
       └─ Responses ↔ chat/completions 翻译
```

---

## 配置示例

最小配置（`config.yaml`）：

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

完整字段说明见 [`config.example.yaml`](config.example.yaml)。

---

## 路由决策

| 步骤 | 判断 | 动作 |
|---|---|---|
| 1 | 从 body 或 URL path 提取 `model` | — |
| 2 | `force-model-mappings` + `model-mappings` 命中 | 就地改写 `model` |
| 3 | 改写后的 model 出现在某 `custom-providers[*].models` | 转发到对应网关，注入 Bearer token |
| 4 | Google v1beta 路径 + `gemini-route-mode: translate` | 先跑 Gemini ↔ Responses 翻译再转发 |
| 5 | 以上都不命中 | 兜底走 `ampcode.com`（**消耗 Amp credits**） |

---

## 五个协议翻译器

| 翻译器 | 用途 | 文件 |
|---|---|---|
| **Anthropic Messages 流式升级** | 修上游非流式 `/messages` 内容丢失 bug | `src/customproxy/sse_messages_collapser.rs` |
| **Gemini ↔ OpenAI Responses (非流式)** | `finder` sub-agent 的 `:generateContent` | `src/customproxy/gemini_translator.rs` |
| **Gemini ↔ OpenAI Responses (流式)** | `:streamGenerateContent` —— 上游 Go 版没有 | `src/customproxy/gemini_stream_translator.rs` |
| **Responses ↔ chat/completions (请求 + 响应)** | DeepSeek 等 chat-only 上游 | `src/customproxy/responses_translator.rs` |
| **Responses SSE 流式** | 同上的流式响应路径 | `src/customproxy/responses_stream_translator.rs` |

---

## 怎么读 `run.log`

每个 Amp CLI 请求两行：**入口**（路由决策）+ **出口**（状态码 + 时延）。

| 日志前缀 | 含义 |
|---|---|
| `amp router: request` | 决策已生成；含 `route` `requested_model` `resolved_model` `provider` `gemini_translate` `streaming` 字段 |
| `amp router: response` | 终态；含 `status` `elapsed_ms` |
| `customproxy: forwarding` | 实际外发 custom provider |
| `gemini-translate: forwarding` | Gemini 翻译器分流式 / 非流式分支 |
| `ampcode fallback: forwarding (BILLABLE — uses Amp credits)` | **见到这条说明 credit 在被扣**，控制面流量正常出现，模型流量出现就要排查 |

排查节奏：

```bash
# 模型流量是否泄露到 ampcode.com（控制面流量排除掉）
grep BILLABLE run.log | grep -v "/api/internal\|/api/telemetry\|/news.rss"

# 翻译器是否失败
grep WARN run.log

# 单次请求完整链路
grep "publishers/google/models/gemini-3-flash-preview" run.log
```

更详细的日志：`RUST_LOG=amp_proxy=debug,tower_http=info`。

---

## 调试 middleware（按需启用）

两个零成本默认关闭的 middleware，加配置就开：

```yaml
debug:
  # 给每个请求加 "request log" INFO 行，含 model/stream peek
  access-log-model-peek: true

  # 把指定路径的请求/响应 body 落盘
  capture-path-substring: "/v1/responses"
  capture-dir: "./capture"
```

---

## 性能

`cargo bench --bench translators` 跑过 simd-json vs serde_json 的实测对比，
发现 **drop-in 兼容路径下 simd-json 反而慢 17%**，结论是不采用。
完整数据见 [`BENCHMARKS.md`](BENCHMARKS.md)。

---

## 端到端验证

通过真实 Amp CLI 会话验证四条主要数据路径：

| 路径 | 验证情形 |
|---|---|
| `claude-sonnet-4-6` 主 agent + librarian → augment-style 上游 | 9 次调用全 200，零 WARN |
| `gemini-3-flash-preview` finder → 上游 via translate | 17 次调用全 200，零 BILLABLE 模型流量 |
| `gpt-5.4` → DeepSeek 经 Responses↔chat/completions | 多轮 reasoning + tool_use 全 200 |
| `/api/internal`、`/api/telemetry` 控制面 → ampcode.com | 正确兜底（这些本来就该走 ampcode） |

---

## 测试

```bash
cargo test
# test result: ok. 142 passed; 0 failed
```

完整模块测试覆盖：

| 模块 | 测试数 |
|---|---|
| `customproxy` (Registry / SSE / 5 个 translator) | 38+ |
| `amp::fallback_handlers` (5 路决策 + Vertex path) | 10 |
| `amp::routes` (peek + can_stream 决策 + dispatch) | 9 |
| `amp::prefixed_body` (混合流式适配器) | 3 |
| `auth` (ArcSwap 并发读 / 原子换值) | 3 |
| `proxy` (兜底流式 + 64 MiB cap) | 2 |
| `access_log` / `body_capture` (debug middleware) | 7 |
| `init` (配置生成 + 解析往返) | 3 |

---

## 路线图

完成的项见 [CHANGELOG.md](CHANGELOG.md)。剩余开放项：

- **Prometheus `/metrics` 端点** —— request count / latency histogram / billable counter，给 Grafana 看
- **Provider health checks + 自动 failover** —— 上游连续超时时临时切换，恢复后切回
- **Body capture pretty-print 工具** —— 抓的 .log 文件格式化成可读 JSON

---

## 鸣谢

`amp-proxy-rs` 的协议翻译算法 + custom provider 路由决策模型源自
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)（MIT），
原归属见 [NOTICE.md](NOTICE.md)。

## License

[MIT](LICENSE)
