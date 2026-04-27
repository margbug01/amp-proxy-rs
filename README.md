<div align="center">

# amp-proxy-rs

**专注于 [Sourcegraph Amp CLI](https://ampcode.com) 的 Rust 反向代理**

[![CI](https://github.com/margbug01/amp-proxy-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/margbug01/amp-proxy-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.75%2B-orange?logo=rust&logoColor=white)](Cargo.toml)
[![Binary](https://img.shields.io/badge/binary-3.7%20MB-blue)](https://github.com/margbug01/amp-proxy-rs/releases)

把指定 model 路由到你自己的 OpenAI 兼容网关，控制面流量保留给 ampcode.com，并清楚记录每一次 billable 兜底。

[English](README.en.md) · [配置示例](config.example.yaml) · [基准测试](BENCHMARKS.md) · [更新日志](CHANGELOG.md)

</div>

---

## 它解决什么问题

`amp-proxy-rs` 位于 Amp CLI 和上游模型服务之间。它会读取请求里的 model，按配置决定走自定义网关还是 ampcode.com；必要时自动做协议翻译，让 Amp CLI 能稳定使用 OpenAI 兼容网关、DeepSeek、Gemini bridge 等路径。

| 能力 | 说明 |
|---|---|
| 🪶 小体积 release 二进制 | LTO + strip + `opt-level = "z"`，无需额外运行时服务 |
| 🔀 五个协议翻译器 | Anthropic Messages、OpenAI Responses、chat/completions、Gemini 非流式与流式 |
| 🚿 混合流式转发 | 只 peek 前 16 KiB 做路由，后续 body 继续流式转发 |
| 🔁 配置热重载 | API key、model mapping、provider 路由可热更新；监听地址变更仍需重启 |
| 🩺 Provider failover | 同一个 model 可配置多个上游，主上游异常后自动切换，恢复后切回 |
| 📈 Prometheus 指标 | `/metrics` 暴露请求数、耗时 histogram、billable 兜底计数 |
| 🧪 覆盖验证 | 159 个单元测试，并用真实 Amp CLI 验证 main agent / librarian / finder / DeepSeek tool use |

---

## 快速开始

```bash
git clone https://github.com/margbug01/amp-proxy-rs.git
cd amp-proxy-rs

cargo build --release
./target/release/amp-proxy init
./target/release/amp-proxy --config config.yaml
```

把 Amp CLI 指向本代理：

```bash
export AMP_URL=http://127.0.0.1:8317
export AMP_API_KEY=<config.yaml 里的某个 api-keys>
amp
```

PowerShell：

```powershell
$env:AMP_URL = "http://127.0.0.1:8317"
$env:AMP_API_KEY = "<config.yaml 里的某个 api-keys>"
amp
```

Windows 下可以用 [`scripts/restart.ps1`](scripts/restart.ps1) 一键重启并重定向日志。

---

## 架构

```mermaid
flowchart TD
    A[Amp CLI] -->|local API key| P[amp-proxy-rs]
    P --> H[/healthz 和 /metrics/]
    P --> R{model route?}
    R -->|custom provider| C[OpenAI 兼容网关]
    R -->|Gemini translate| G[Gemini ↔ OpenAI Responses bridge]
    R -->|fallback| B[ampcode.com BILLABLE]
    C --> T1[Anthropic Messages SSE 升级/折叠]
    C --> T2[Responses ↔ chat/completions]
    G --> T3[generateContent / streamGenerateContent]
```

关键分工：

- **模型流量** 可以转发到你自己的 provider。
- **Amp 控制面流量**，例如 `/api/internal`、`/api/telemetry`，仍然兜底到 ampcode.com。
- 每次 ampcode.com 兜底都会打 `BILLABLE` 日志，并增加 `billable_requests_total`。

---

## 配置

最小 `config.yaml`：

```yaml
host: "127.0.0.1"
port: 8317

api-keys:
  - "change-me"

ampcode:
  upstream-url: "https://ampcode.com"
  upstream-api-key: "" # 可选 Amp session token

  custom-providers:
    - name: "primary-gateway"
      url: "http://localhost:8000/v1"
      api-key: "your-bearer-token"
      models:
        - "gpt-5.4"
        - "gpt-5.4-mini"
      responses-translate: true

    - name: "backup-gateway"
      url: "http://localhost:8001/v1"
      api-key: "backup-token"
      models:
        - "gpt-5.4"

  model-mappings:
    - from: "claude-opus-4-6"
      to: "gpt-5.4(high)"

  force-model-mappings: true
  gemini-route-mode: "translate"
```

完整字段说明见 [config.example.yaml](config.example.yaml)。

---

## 路由决策

| 步骤 | 条件 | 动作 |
|---|---|---|
| 1 | 从 body 或 Gemini URL path 提取 `model` | 进入路由判断 |
| 2 | `force-model-mappings` / `model-mappings` 命中 | 改写上游请求里的 `model` 字段 |
| 3 | 解析后的 model 出现在 `custom-providers[*].models` | 转发到第一个健康 provider，并注入 Bearer token |
| 4 | 多个 provider 服务同一个 model | 连续传输失败后切到后备；探活恢复后切回主上游 |
| 5 | Google Gemini 路径且 `gemini-route-mode: translate` | 先做 Gemini ↔ OpenAI Responses 翻译再转发 |
| 6 | 以上都不命中 | 兜底到 ampcode.com，计为 **billable** |

---

## 可观测性

| 信号 | 说明 |
|---|---|
| 日志 | `amp router:*`、`customproxy: forwarding`、`gemini-translate: forwarding`、显式 `BILLABLE` 兜底行 |
| 指标 | `/metrics` 暴露 `requests_total`、`request_duration_seconds`、`billable_requests_total` |
| 抓包辅助 | `capture-pretty` 把 body capture 日志转换成结构化 JSON |

```bash
./target/release/amp-proxy capture-pretty ./capture/in.log --output ./capture/in.pretty.json
```

---

## 验证

```bash
cargo fmt --check
cargo test --all-features --no-fail-fast
cargo clippy --all-targets --all-features -- -D warnings
```

当前本地结果：

```text
test result: ok. 159 passed; 0 failed
```

---

## 友链

- [LINUX DO](https://linux.do/) — 新的理想型社区。

---

## 致谢

协议翻译算法与 custom-provider 路由模型源自 [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)，许可证为 MIT。归属信息见 [NOTICE.md](NOTICE.md)。

## License

[MIT](LICENSE)
