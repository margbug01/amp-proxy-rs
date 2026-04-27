# Translator parsing benchmarks

Measures the cost of JSON parsing in the customproxy translators and decides
whether `simd-json` is worth adopting in place of `serde_json` for
`translate_responses_request_to_chat` (the hottest 700-LOC path).

## Setup

- Hardware: Intel(R) Core(TM) Ultra 9 185H, Windows 11 Pro 22621
- Toolchain: cargo 1.94.0 / rustc 1.94.0 (release profile, `opt-level = "z"`, `lto = "fat"`)
- Crates: `serde_json = "1"`, `simd-json = "0.13.11"`, `criterion = "0.5"`
- Date: 2026-04-27
- Fixtures (synthetic, modeled on real production body shapes — see
  `benches/fixtures/generate.js`):
  - `small.json`  ~4.2 KB  Anthropic Messages, 5 turns, 2 tools
  - `medium.json` ~48 KB  Gemini `generateContent`, 22 turns, 10 tools
  - `large.json`  ~143 KB OpenAI Responses, 28 messages + 5 late tool calls
- Criterion settings used to keep wall time reasonable: `--warm-up-time 1
  --measurement-time 3 --sample-size 30`. Estimates are stable; outlier counts
  reported below were ≤17% (mostly mild).

## Numbers

Median time (point estimate) and throughput on each scenario. Lower µs is
better; higher MiB/s is better.

| body            | scenario               | median (µs) | throughput     | vs serde |
|-----------------|------------------------|-------------|----------------|----------|
| small (4.2 KB)  | serde_json             |  12.69      |  324 MiB/s     | 1.00x    |
| small (4.2 KB)  | simd_json (owned)      |  14.07      |  292 MiB/s     | 0.90x    |
| small (4.2 KB)  | simd_json (borrowed)   |   7.07      |  582 MiB/s     | 1.79x    |
| medium (48 KB)  | serde_json             |  89.14      |  521 MiB/s     | 1.00x    |
| medium (48 KB)  | simd_json (owned)      |  91.94      |  505 MiB/s     | 0.97x    |
| medium (48 KB)  | simd_json (borrowed)   |  58.82      |  790 MiB/s     | 1.52x    |
| large (143 KB)  | serde_json             | 122.16      | 1.09 GiB/s     | 1.00x    |
| large (143 KB)  | simd_json (owned)      | 146.33      |  931 MiB/s     | 0.83x    |
| large (143 KB)  | simd_json (borrowed)   |  98.17      | 1.36 GiB/s     | 1.24x    |

End-to-end translator (`translate_responses_request_to_chat`, large body):

| scenario                              | median (µs) | throughput |
|---------------------------------------|-------------|------------|
| translate full (parse + walk + emit)  | 243.15      | 560 MiB/s  |
| parse only (serde_json)               | 121.21      | 1.10 GiB/s |
| parse only (simd_json owned)          | 145.29      |  937 MiB/s |

So on the large body the **parse step is ~50% of total translator time**; the
remaining ~50% is the input-array walk, the chat-shape rebuild, and the
final `serde_json::to_vec` re-serialize. That ratio is the central number for
the decision.

## Decision

**Not adopted.** `simd-json` does not earn its complexity in this codebase
right now. None of the variants meets the >2x parse-speedup bar set in the
methodology, and the variants that produce a `serde_json::Value` (the type
the translators traverse end-to-end) are actually **slower** than
`serde_json::from_slice` at every body size.

## Reasoning

1. **simd_json::serde::from_slice → `serde_json::Value` is slower than
   `serde_json::from_slice`** on this hardware at every size we care about
   (0.83x–0.97x). That is the only drop-in variant; `Value` is what the
   translator already operates on. Adopting it would be a regression.

2. **`simd_json::to_borrowed_value` is the only faster path (1.24x on the
   large body, 1.79x on small).** It returns simd-json's own borrowed `Value`,
   not `serde_json::Value`. To use it the translator would have to:
   - traverse simd-json's `BorrowedValue` API (different `as_str` /
     `as_array` / `Map` types),
   - keep the input `Vec<u8>` alive for the whole translate call (it owns
     the strings the borrowed value points into),
   - convert to `serde_json::Value` (or rewrite the rebuild to emit simd-json
     output) before re-serializing,
   That is structurally invasive across all branches in
   `translate_input_to_messages`, `extract_message_text`,
   `extract_reasoning_text`, and the tool-loop — well beyond a 50-LOC bridge,
   and it would diverge from the Go port's shape-for-shape mapping that the
   tests pin down. Plus the fastest variant only saves **24 µs out of 243 µs**
   (≈10%) on a 143 KB body — the dominant cost is the graph walk + rebuild,
   not parsing.

3. **Parse share is ~50% of translator time on the large body, not 90%+.**
   Halving the parse step buys us ~25% on the big bodies, not the headline
   number simd-json benchmarks usually advertise. Real-world bodies hit the
   translator one-per-request, not in tight loops, so we'd be optimizing
   double-digit microseconds per request — well below the network and
   upstream-LLM latencies (hundreds of ms) the proxy actually sits behind.

4. **Real production bodies cap at ~170 KB.** From the production-log
   distribution given in the task brief
   (`gpt-5.4 -> deepseek` Responses 94–170 KB), even the worst case sees
   serde_json parse at ~140 µs. That is not a hot spot.

5. **Cost side.** Keeping `simd-json` only as a dev-dependency for the bench
   is fine. Promoting it to a runtime dep would add a non-trivial transitive
   dependency (ahash, halfbrown, value-trait, simdutf8, lexical-core,
   getrandom, ref-cast) for a sub-10% wall-time improvement on one route.

## Future revisit signal

Re-run this benchmark and revisit the decision **if any of these become
true**:

- Observed Responses request bodies regularly exceed **1 MB** (e.g. very
  long agentic threads with many tool-call outputs). At that size parse
  cost grows linearly while graph-rebuild may not, shifting the ratio.
- The translators are placed inside a hot loop (e.g. batch replay or fuzz
  harness) where per-request fixed costs add up.
- A future translator path is built around `simd-json`'s borrowed value
  natively (no `serde_json::Value` round-trip), at which point the 1.2–1.8x
  parse-only win plus avoided allocations may make the migration pencil out.
- `simd-json` ships a zero-copy bridge that produces a `serde_json::Value`
  faster than `serde_json::from_slice` on AVX2 hardware. (As of 0.13.11 it
  does not on this CPU.)

## How to reproduce

```
cargo bench --bench translators -- --warm-up-time 1 --measurement-time 3 --sample-size 30
```

Fixtures regenerate via `node benches/fixtures/generate.js` if the shapes
need updating; total fixture size is capped under 200 KB.
