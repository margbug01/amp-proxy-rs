# Changelog

All notable changes to amp-proxy-rs are documented in this file. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-04-27

First Rust port. Functional parity with `../amp-proxy` (Go) on every
data-plane code path that Amp CLI exercises, plus two additions the Go
version does not have. See [NOTICE.md](NOTICE.md) for the full provenance
chain and "additions beyond upstream" list.

### Added

- **Full Rust implementation** — 12 top-level modules, 28 source files,
  120 unit tests (`cargo test`), 3.6 MB stripped release binary on
  Windows x64 (LTO + opt-z + strip).
- **Custom provider registry** with model-keyed routing, atomic hot-reload
  via `arc_swap::ArcSwap`, case-insensitive lookup, thinking-suffix
  fallback (`gpt-5.4-mini(high)` → `gpt-5.4-mini` for registry match).
- **Three protocol translators** ported 1:1 from the Go version:
  - Anthropic `/v1/messages` non-streaming → SSE upgrade + collapse
    (the `librarian` content-loss workaround)
  - Gemini `:generateContent` ↔ OpenAI Responses (request + response)
  - OpenAI Responses ↔ chat/completions (request + non-streaming reply)
  - OpenAI Responses SSE → Responses SSE for chat/completions upstreams
    (DeepSeek's `/v1/responses` translation path)
- **Streaming Gemini translator** *(new beyond Go upstream)* —
  `customproxy::gemini_stream_translator` translates an OpenAI Responses
  SSE stream into Gemini `:streamGenerateContent` SSE chunks. The Go
  version's `serveGeminiTranslate` returns false for this path and falls
  through to ampcode.com; the Rust port keeps `finder` on custom
  providers.
- **AMP-CLI / Vertex AI path routes** *(new beyond Go upstream)* —
  `/api/provider/<provider>/v1beta{,1}/publishers/google/models/*action`.
  Without these, Amp CLI's `finder` requests silently fall through to
  ampcode.com because the Go-style route table only covered the simpler
  `/v1beta/models/<model>:<action>` shape.
- **`amp-proxy init` interactive wizard** — generates a ready-to-run
  `config.yaml` with the 9-entry default model mapping table, random
  local API key, `force-model-mappings: true`, and
  `gemini-route-mode: "translate"`. Match for Go upstream.
- **Config hot-reload** — `mtime`-polled file watcher in `main::serve`
  rebuilds the API key validator and the amp module's compiled
  fallback handler atomically on every config save.
- **Structured per-request logging**:
  - `amp router: request` — INFO entry with method, path, body_bytes,
    route decision, requested/resolved model, provider, gemini_translate
    flag.
  - `amp router: response` — paired entry with status code and elapsed_ms.
  - `gemini-translate: forwarding` — per Gemini bridge call with stream
    flag, in/out byte counts, target URL.
  - `customproxy: forwarding` — per custom-provider call with
    `upgraded_messages` and `translate_responses` flags.
  - `ampcode fallback: forwarding (BILLABLE — uses Amp credits)` —
    explicit warning whenever a request would consume Amp credits.
    Surfaces credit drain immediately in `run.log`.
- **`config.example.yaml`** + **`config.local.yaml`** — copied from the
  Go parent, schema is wire-compatible (kebab-case keys, `serde(default)`
  on all optional blocks).

### Validated against real Amp CLI traffic

End-to-end testing through a real Amp CLI session covered the four
high-risk paths:

| Path | Verdict |
|---|---|
| `claude-sonnet-4-6` (main agent + librarian) → augment-gpt | ✅ 9 calls, all 200, no warnings |
| `gemini-3-flash-preview` (finder) → augment-gpt via translate | ✅ 17 calls, all 200, no credit leak |
| `gpt-5.4` → DeepSeek via Responses↔chat/completions | ✅ many multi-turn reasoning + tool_use calls, all 200 |
| `/api/internal`, `/api/telemetry` (Amp control plane) → ampcode.com | ✅ correctly billable (expected) |

### Fixed (relative to the Go upstream)

- **Vertex path routing** — the Go gin router didn't claim the
  `/publishers/google/models/...` variant Amp CLI sends to its `google`
  provider; requests fell through to ampcode.com and consumed credits.
  Added explicit routes in `amp::routes` and an extra
  `extract_gemini_model_from_path` test case.
- **`gemini_bridge` Accept/stream mismatch** — the initial Rust port
  unconditionally sent `Accept: text/event-stream` on the upstream
  request, but `translate_gemini_response` only parses single-JSON
  bodies. `:generateContent` now requests `application/json` with
  `stream: false`; only `:streamGenerateContent` keeps the SSE accept
  header.

### Not ported (deliberately)

- `internal/access/`, `internal/handlers/`, `internal/registry/`,
  `internal/modules/` — Go SDK plumbing for OAuth-managed providers that
  amp-proxy-rs doesn't ship.
- `scripts/test_gemini_translate.js` — replaced by in-process unit tests
  in `customproxy::gemini_translator::tests`.
- `internal/server/{access_log,body_capture}.go` — left as a pair of
  "good Rust learning exercise" stubs (see README's 学习路线 section).
