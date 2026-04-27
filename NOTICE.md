# NOTICE

`amp-proxy-rs` is a **second-order derivative work**: a Rust port of
`amp-proxy`, which itself is a Go derivative of CLIProxyAPI.

## Provenance chain

```
router-for-me/CLIProxyAPI  (Go, MIT)
        │
        │  extracts the Amp subsystem + adds customproxy routing layer
        ▼
margbug01/amp-proxy        (Go, MIT)              ← parent, ../amp-proxy
        │
        │  port to Rust + axum/tokio/reqwest stack
        │  + new streaming Gemini translator
        │  + AMP-CLI Vertex path support
        ▼
margbug01/amp-proxy-rs     (Rust, MIT)            ← this project
```

The upstream MIT `LICENSE` is preserved at the repository root.

## What was ported, not what was rewritten

The Rust port is **structural, not line-by-line**. Algorithms and protocol
behaviour from the Go version are preserved 1:1 (model lookup keys,
thinking-suffix stripping, `extract_leaf` rules, SSE event handling for the
three translator paths, etc.); types and idioms are native Rust:

| Concern | Go version | Rust port |
|---|---|---|
| HTTP framework | `gin-gonic/gin` | `axum 0.7` |
| Reverse proxy primitive | `httputil.ReverseProxy` w/ `Director` + `ModifyResponse` | hand-rolled `reqwest` round-trip + axum `Body::from_stream` |
| Hot-reloadable shared state | `sync.RWMutex` | `arc_swap::ArcSwap` |
| JSON path access | `gjson` / `sjson` | `serde_json::Value` (no path-DSL equivalent in Rust ecosystem) |
| Logging | `logrus` | `tracing` + `tracing-subscriber` |
| Error model | bare `error` | `thiserror::Error` enum + `anyhow::Result` at boundaries |

Module-by-module map lives in [README.md](README.md) under "模块导览".

## Additions beyond the Go upstream

The Rust port intentionally adds a few things the Go version does not have:

- **`customproxy::gemini_stream_translator`** — translates an OpenAI Responses
  SSE stream into Gemini `:streamGenerateContent` SSE so the `finder`
  sub-agent runs on custom providers without falling through to ampcode.com.
  The Go version explicitly punts on this:
  > `gemini-translate: streamGenerateContent not yet supported; falling through to ampcode.com`  
  > — `internal/amp/fallback_handlers.go:412`
- **AMP-CLI / Vertex AI path variants** — routes for
  `/api/provider/google/v1beta{,1}/publishers/google/models/<model>:<action>`
  paths that the Go port's gin route table didn't cover. Without these,
  every `finder` request silently fell through to ampcode.com and consumed
  Amp credits.
- **`BILLABLE` warning at the ampcode.com fallback** — the Go fallback
  forwards silently. The Rust fallback emits an INFO line whenever a
  request hits the credit-consuming path, making credit drain immediately
  visible in `run.log`.
- **Structured per-request access logs** — every Amp CLI request gets a
  matched pair of `amp router: request` / `amp router: response` lines with
  `route_type`, `requested_model`, `resolved_model`, `provider`,
  `gemini_translate`, `status`, `elapsed_ms`. Easier to grep than the Go
  version's per-component logs.

## Things deliberately left out

- **OAuth flows** (ChatGPT / Claude Code / Gemini CLI). Inherited from the
  Go parent's deletion. Use a sidecar gateway if you need them.
- **`internal/access/`, `internal/registry/`, `internal/modules/`,
  `internal/handlers/{claude,gemini,openai}` from the Go upstream**. These
  exist for SDK-managed providers that this port doesn't speak. The
  customproxy / amp router pair covers the entire Amp CLI request surface
  on its own.
- **`scripts/test_gemini_translate.js`** — the Go version ships a Node smoke
  test against a running instance. Not ported; the Rust unit tests cover
  the same translation paths in-process.

## Local divergence from the Go parent

Tracking divergence per-file isn't worth the bookkeeping for a Rust port —
the languages differ, so every file is "divergent". The intent is parity of
**behaviour**, and that parity is kept by:

1. Mirroring the Go decision logic (route ordering, request-overrides
   merge order, header sanitisation list) verbatim in the Rust port.
2. Cross-referencing each Rust module's doc-comment to its Go origin.
3. Running a real Amp CLI session through the Rust port and confirming
   `finder`, `librarian`, main agent, and DeepSeek translation all work
   without falling back to ampcode.com.

When the Go upstream gains a fix that affects translator semantics, the
patch must be reproduced manually — the README's "学习路线" section
suggests using `../amp-proxy/capture_outbound/` as a fixture diff target
for that work.

## Original copyright

Inherited from the upstream MIT LICENSE:

```
Copyright (c) 2025-2025.9 Luis Pater
Copyright (c) 2025.9-present Router-For.ME
```

Rust port additions (this repository) © 2026 margbug01, MIT.
