# Changelog

All notable changes to amp-proxy-rs are documented in this file. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-04-27

First release. End-to-end validated against a real Amp CLI session on
Windows; 120 unit tests; 3.6 MB stripped release binary.

### Added

- **HTTP server** — axum 0.7 + tokio multi-threaded runtime, graceful
  Ctrl+C / SIGTERM shutdown, `/healthz` endpoint.
- **API key authentication** — middleware accepting `x-api-key` or
  `Authorization: Bearer <key>`, hot-reloadable from config.
- **Custom provider registry** — model-keyed routing with atomic
  hot-reload via `arc_swap::ArcSwap`, case-insensitive lookup,
  thinking-suffix fallback (`gpt-5.4-mini(high)` → `gpt-5.4-mini` for
  registry match).
- **Five protocol translators**:
  - Anthropic `/v1/messages` non-streaming → SSE upgrade + collapse
    (workaround for an upstream content-loss bug observed on the
    `librarian` sub-agent path).
  - Gemini `:generateContent` ↔ OpenAI Responses (request + response).
  - Gemini `:streamGenerateContent` ↔ OpenAI Responses SSE (streaming
    state machine).
  - OpenAI Responses ↔ chat/completions (request + non-streaming reply).
  - OpenAI Responses SSE → Responses SSE for chat-completions-only
    upstreams (DeepSeek-style endpoints).
- **AMP-CLI / Vertex AI path routing** — claims
  `/api/provider/<provider>/v1beta{,1}/publishers/google/models/*action`
  paths so Amp CLI's `finder` sub-agent stays on configured custom
  providers instead of falling through to ampcode.com.
- **`amp-proxy init` interactive wizard** — generates a ready-to-run
  `config.yaml` with a 9-entry default model mapping table, a random
  local API key, `force-model-mappings: true`, and
  `gemini-route-mode: "translate"`.
- **Config hot-reload** — mtime-polled file watcher rebuilds the API
  key validator and the amp module's compiled fallback handler
  atomically on every config save.
- **Structured per-request logging**:
  - `amp router: request` / `amp router: response` paired INFO entries
    with method, path, body bytes, route decision, requested/resolved
    model, provider, gemini_translate flag, status, elapsed_ms.
  - `gemini-translate: forwarding` per-bridge call with stream flag,
    in/out byte counts, target URL.
  - `customproxy: forwarding` per custom-provider call with
    `upgraded_messages` / `translate_responses` flags.
  - `ampcode fallback: forwarding (BILLABLE — uses Amp credits)`
    explicit warning whenever a request consumes Amp credits.
- **Retry transport** — wraps reqwest with single-retry on transient
  transport errors (timeouts, connection resets, EOFs).
- **`config.example.yaml`** — full schema reference with kebab-case keys
  and `serde(default)` on every optional block.

### Validated

End-to-end testing through a real Amp CLI session covered four
high-risk paths:

| Path | Verdict |
|---|---|
| `claude-sonnet-4-6` (main agent + librarian) → custom provider | ✅ 9 calls, all 200, no warnings |
| `gemini-3-flash-preview` (finder) → custom provider via translate | ✅ 17 calls, all 200, no credit leak |
| `gpt-5.4` → DeepSeek via Responses↔chat/completions | ✅ multi-turn reasoning + tool_use, all 200 |
| `/api/internal`, `/api/telemetry` (Amp control plane) → ampcode.com | ✅ correctly billable (expected) |

### Build

- Release profile: `opt-level = "z"`, `lto = "fat"`, `codegen-units = 1`,
  `strip = "symbols"`, `panic = "abort"`.
- Resulting binary: 3.6 MB on Windows x64.
