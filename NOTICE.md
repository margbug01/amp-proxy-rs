# NOTICE

`amp-proxy-rs` is licensed under the MIT License. Portions of this
codebase are derived from prior work, reproduced here under the same MIT
terms.

## Upstream attributions

The Amp CLI routing model, custom provider registry design, and the
non-streaming variants of the three protocol translators (Anthropic
Messages stream upgrade, Gemini ↔ OpenAI Responses, OpenAI Responses ↔
chat/completions) were originally developed in:

- **CLIProxyAPI** — <https://github.com/router-for-me/CLIProxyAPI>
  - License: MIT
  - Copyright (c) 2025 Luis Pater
  - Copyright (c) 2025 Router-For.ME

The MIT `LICENSE` file at the repository root preserves the original
copyright notices required by the upstream license.

## Original work in this repository

The following are original to `amp-proxy-rs` and not present in any
upstream codebase:

- The streaming Gemini translator
  (`src/customproxy/gemini_stream_translator.rs`), which translates an
  OpenAI Responses SSE stream into Gemini `:streamGenerateContent` SSE.
- AMP-CLI / Vertex AI route patterns for
  `/api/provider/<provider>/v1beta{,1}/publishers/google/models/*action`
  in `src/amp/routes.rs`.
- The structured per-request access logging (`amp router: request` /
  `amp router: response` paired entries with route decision metadata) and
  the explicit `BILLABLE` warning emitted by the ampcode.com fallback
  proxy.

Copyright (c) 2026 margbug01. Released under MIT.
