# Gemini Bridge DeepSeek Test Notes

Date: 2026-04-27

## Scope

This note records the current DeepSeek validation status for the Gemini bridge dual-format translation work.

Tested paths:

- Gemini `generateContent` -> OpenAI Responses -> DeepSeek OpenAI `chat/completions`
- Gemini `generateContent` -> OpenAI Responses -> DeepSeek Anthropic Messages `/v1/messages`

Streaming Gemini `streamGenerateContent` was not observed during these Amp CLI finder tests.

## Working Path: OpenAI Format

Configuration:

```yaml
custom-providers:
  - name: "deepseek"
    url: "https://api.deepseek.com/v1"
    models:
      - "deepseek-v4-pro"
      - "deepseek-v4-flash"
    responses-translate: true

model-mappings:
  - from: "gemini-3-flash-preview"
    to: "deepseek-v4-flash"
```

Observed behavior:

- Finder requests route to `https://api.deepseek.com/v1/chat/completions`.
- `gemini-3-flash-preview` maps to `deepseek-v4-flash`.
- Repeated non-streaming requests return HTTP 200.
- Multi-turn finder behavior remains semantically correct for the tested query.
- DeepSeek thinking is forced disabled on this path to avoid multi-turn reasoning content issues.

Conclusion:

- This is the recommended DeepSeek Gemini bridge path for now.

## Experimental Path: Anthropic Messages Format

Configuration used during testing:

```yaml
custom-providers:
  - name: "deepseek"
    url: "https://api.deepseek.com/anthropic"
    models:
      - "deepseek-v4-pro"
      - "deepseek-v4-flash"
    messages-translate: true

model-mappings:
  - from: "gemini-3-flash-preview"
    to: "deepseek-v4-flash" # also tested with deepseek-v4-pro
```

Protocol fixes added during testing:

- Added `anthropic-version: 2023-06-01` for Messages upstream requests.
- Force disabled DeepSeek thinking on the Messages path.
- Added fallback user messages when translated Anthropic `messages` would be empty.
- Normalized `tool_use` / `tool_result` adjacency required by Anthropic Messages.
- Converted orphan or unmatched `tool_result` blocks into plain user text to avoid upstream 400s.

Observed protocol status:

- DeepSeek Anthropic `/v1/messages` accepts the translated requests.
- The previous 400 errors are resolved:
  - `messages: at least one message is required`
  - `tool_use ids were found without tool_result blocks immediately after`
  - `unexpected tool_use_id found in tool_result blocks`
- Repeated non-streaming finder requests return HTTP 200.

Observed semantic issue:

- The first Gemini user message still contains the correct finder query.
- Despite that, finder behavior can drift on the Anthropic Messages path.
- Example: query `gemini_bridge implementation routes tests call chain` was correctly present in the Gemini request, but later generated tool calls searched for `amp-share`, `ampshare`, and `amp_share`.
- Switching from `deepseek-v4-flash` to `deepseek-v4-pro` did not resolve this behavior.
- Switching the same query back to OpenAI `responses-translate` with `deepseek-v4-flash` succeeded.

Conclusion:

- `messages-translate` is protocol-valid but not yet semantically equivalent for finder-style multi-turn tool use.
- Keep this path experimental until the Responses -> Anthropic Messages tool-history translation can preserve semantics more faithfully.

## Debug Evidence

Temporary Gemini request snippet logging showed that the query is present before translation:

```text
gemini_bridge implementation routes tests call chain
```

Later Anthropic-path Gemini requests contained tool calls such as:

```text
Grep pattern="amp-share"
Grep pattern="ampshare"
Grep pattern="amp_share"
```

This indicates the issue is not query injection loss at the inbound Gemini request layer. The likely cause is semantic distortion in the Anthropic Messages translation/tool-history representation.

## Current Recommendation

Use DeepSeek through OpenAI format:

```yaml
url: "https://api.deepseek.com/v1"
responses-translate: true
```

Avoid using `messages-translate` for finder/Gemini bridge workloads until further semantic parity work is completed.

## Follow-up Work

- Capture and compare full upstream bodies for the same query:
  - OpenAI chat/completions translated body
  - Anthropic Messages translated body
- Inspect tool call IDs, function names, arguments, and tool result placement across both formats.
- Avoid empty synthetic tool results where possible; prefer preserving original Gemini/Responses function-call semantics.
- Add integration-style fixtures for finder multi-turn tool-use histories.
- Re-test Gemini `streamGenerateContent` separately; Amp CLI did not trigger it in this test round.
