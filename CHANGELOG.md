# Changelog

All notable changes to amp-proxy-rs are documented in this file. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-04-27

Adds the three operational features that were on the roadmap after v0.1.0:
Prometheus `/metrics`, provider health checks with automatic failover, and a
`capture-pretty` body-capture viewer. 159 unit tests pass.

### Added

- **Prometheus `/metrics` endpoint** — exposes `requests_total` (counter),
  `request_duration_seconds` (histogram), and `billable_requests_total`
  (counter, increments on every ampcode.com fallback). Wired via
  `metrics_middleware` so every Amp CLI request feeds the histogram.
- **Provider health checks + auto-failover** — multiple `custom-providers`
  entries serving the same model are now tried in order; consecutive
  upstream errors trip a per-provider `healthy = false` flag, the registry
  routes around the unhealthy one, and a 30-second background probe to
  `<provider>/v1/models` flips it healthy again on success. Implemented
  via `customproxy::Registry::record_success / record_failure /
  health_snapshots` and `main::provider_health_checker`.
- **`amp-proxy capture-pretty` subcommand** — pretty-prints a
  `body_capture` `.log` file as structured JSON (auto-detects
  `application/json` request and response bodies and formats them).
  Optional `--output <path>` writes to disk instead of stdout.
- **Bilingual README** — Chinese primary (`README.md`), English mirror
  (`README.en.md`).

### Changed

- `customproxy::Registry` internal layout changed from
  `HashMap<model, Arc<Provider>>` to
  `HashMap<model, Vec<Arc<Provider>>>` to support multiple providers per
  model. Behaviour for single-provider setups is unchanged.
- `gemini_bridge::forward_gemini_translated` now goes through
  `RetryTransport::send_with_retry` and feeds health-tracking on every
  attempt.
- `proxy::sanitize_request_headers` now also strips any header listed
  inside the inbound `Connection:` header value (per RFC 9110).
- `config::validate` now rejects duplicate provider names but ALLOWS
  duplicate models across providers (required for failover).

### Fixed

- Triple-emitted release notes when the matrix-parallel release jobs each
  regenerated notes via `softprops/action-gh-release@v2`.

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
