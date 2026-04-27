//! OpenAI Responses SSE -> Gemini streamGenerateContent SSE translator.
//!
//! Bridges Amp CLI's `finder` subagent (Gemini :streamGenerateContent) to
//! OpenAI-Responses-speaking custom providers. The non-streaming sibling lives
//! in [`super::gemini_translator`].
//!
//! Wire format on output: each emitted chunk is a `data: <json>\n\n` block
//! whose JSON shape mirrors what `generativelanguage.googleapis.com` emits
//! for `:streamGenerateContent`:
//!
//! ```text
//! {
//!   "candidates":[{
//!     "content":{"parts":[{"text":"..."}],"role":"model"},
//!     "index":0
//!   }],
//!   "modelVersion":"gemini-3-flash-preview",
//!   "usageMetadata": { ... }   // populated only on the terminal chunk
//! }
//! ```
//!
//! Function calls map to a `functionCall` part where `args` is a JSON object
//! (NOT a stringified JSON), matching Gemini's wire shape.

use std::collections::BTreeMap;
use std::io;

use bytes::{Bytes, BytesMut};
use futures::stream::{Stream, StreamExt};
use serde_json::{json, Map, Value};
use tracing::warn;

/// Translate an OpenAI Responses SSE stream into a Gemini-shape SSE stream
/// matching what Amp CLI's finder expects when it asked for
/// `:streamGenerateContent`.
///
/// `original_model` is what the client asked for (e.g. "gemini-3-flash-preview")
/// and is echoed back into every emitted chunk's `modelVersion` field so
/// downstream Amp logs stay coherent.
pub fn translate_responses_sse_to_gemini<S>(
    upstream: S,
    original_model: String,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut state = TranslatorState::new(original_model);
        let mut buf: Vec<u8> = Vec::new();

        let mut upstream = Box::pin(upstream);
        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => {
                    // Surface upstream IO error after attempting to flush a
                    // terminal chunk so downstream parsers see a clean tail.
                    if !state.emitted_terminal {
                        let bytes = state.build_terminal_chunk();
                        if let Some(b) = bytes {
                            yield Ok(b);
                        }
                    }
                    yield Err(e);
                    return;
                }
            };
            buf.extend_from_slice(&chunk);

            // Drain complete SSE events delimited by a blank line ("\n\n").
            while let Some(end) = find_event_boundary(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..end).collect();
                // Skip the blank-line delimiter itself.
                let blank = blank_line_len(&buf);
                buf.drain(..blank);
                let event_text = std::str::from_utf8(&event_bytes).unwrap_or("");
                if event_text.trim().is_empty() {
                    continue;
                }
                for out in state.process_event(event_text) {
                    yield Ok(out);
                }
                if state.emitted_terminal {
                    break;
                }
            }

            if state.emitted_terminal {
                break;
            }
        }

        // Process any trailing data left in the buffer (no blank line at end).
        if !state.emitted_terminal && !buf.is_empty() {
            let event_text = std::str::from_utf8(&buf).unwrap_or("").to_string();
            buf.clear();
            if !event_text.trim().is_empty() {
                for out in state.process_event(&event_text) {
                    yield Ok(out);
                }
            }
        }

        // If upstream closed without `response.completed`, synthesize a
        // terminal Gemini chunk so the downstream client sees a finish.
        if !state.emitted_terminal {
            if let Some(b) = state.build_terminal_chunk() {
                yield Ok(b);
            }
        }
    }
}

/// Per-stream state machine accumulating Responses SSE events into Gemini
/// chunks.
struct TranslatorState {
    /// Model name the client originally asked for; echoed back into every
    /// Gemini chunk's `modelVersion`.
    original_model: String,
    /// In-flight function calls keyed by Responses `item_id`. Cleared as
    /// each one finishes via `function_call_arguments.done`.
    function_calls: BTreeMap<String, FunctionCallState>,
    /// `usage` block from `response.completed`, if any.
    final_usage: Option<Value>,
    /// True after at least one `functionCall` chunk has been emitted; flips
    /// the terminal `finishReason` to `"TOOL_USE"`.
    emitted_function_call: bool,
    /// True once the terminal Gemini chunk has been written.
    emitted_terminal: bool,
}

/// In-flight function-call accumulator keyed by Responses item_id.
struct FunctionCallState {
    name: String,
    args_buf: String,
}

impl TranslatorState {
    fn new(original_model: String) -> Self {
        Self {
            original_model,
            function_calls: BTreeMap::new(),
            final_usage: None,
            emitted_function_call: false,
            emitted_terminal: false,
        }
    }

    /// Parses one upstream SSE event (everything between blank lines) and
    /// returns zero or more Gemini-shape `data: ...\n\n` chunks.
    fn process_event(&mut self, event_text: &str) -> Vec<Bytes> {
        let mut out: Vec<Bytes> = Vec::new();
        // SSE events can have multiple `data:` continuation lines per the
        // spec; for OpenAI Responses each event is a single `data:` line, but
        // we tolerate the general case by joining them.
        let mut data_payload = String::new();
        for line in event_text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("data: ") {
                if !data_payload.is_empty() {
                    data_payload.push('\n');
                }
                data_payload.push_str(rest);
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data_payload.is_empty() {
                    data_payload.push('\n');
                }
                data_payload.push_str(rest);
            }
            // `event:`, `id:`, `:` (comment), and blank lines are ignored —
            // we use the JSON's `type` field for routing.
        }
        if data_payload.is_empty() {
            return out;
        }
        if data_payload.trim() == "[DONE]" {
            // OpenAI streaming sentinel — finalize if we somehow missed
            // `response.completed`.
            if !self.emitted_terminal {
                if let Some(b) = self.build_terminal_chunk() {
                    out.push(b);
                }
            }
            return out;
        }

        let v: Value = match serde_json::from_str(&data_payload) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, payload = %data_payload, "gemini stream: skip non-JSON SSE data");
                return out;
            }
        };

        let event_type = v
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();

        match event_type.as_str() {
            "response.created" | "response.in_progress" => {
                // Nothing to emit — Gemini stream has no equivalent envelope.
            }
            "response.output_item.added" => {
                self.handle_item_added(&v);
            }
            "response.output_text.delta" => {
                if let Some(b) = self.handle_text_delta(&v) {
                    out.push(b);
                }
            }
            "response.function_call_arguments.delta" => {
                self.handle_args_delta(&v);
            }
            "response.function_call_arguments.done" => {
                if let Some(b) = self.handle_args_done(&v) {
                    out.push(b);
                }
            }
            "response.output_item.done" => {
                // No-op: text deltas and function-call completion already
                // emitted their Gemini chunks; we don't emit a separator.
            }
            "response.completed" => {
                if let Some(u) = v.pointer("/response/usage") {
                    if !u.is_null() {
                        self.final_usage = Some(u.clone());
                    }
                }
                if let Some(b) = self.build_terminal_chunk() {
                    out.push(b);
                }
            }
            "response.failed" | "response.error" | "error" => {
                warn!(payload = %data_payload, "gemini stream: upstream error event; finalizing");
                if let Some(b) = self.build_terminal_chunk() {
                    out.push(b);
                }
            }
            other => {
                // Many event types (e.g. content_part.added, reasoning
                // events) carry no Gemini-mappable info. Skip quietly except
                // for unrecognized ones.
                if !is_known_silent_event(other) {
                    warn!(event = %other, "gemini stream: ignoring unknown SSE event");
                }
            }
        }

        out
    }

    /// Reacts to `response.output_item.added`. For function_call items we
    /// initialize an accumulator keyed by item_id; for everything else we
    /// wait for the corresponding deltas.
    fn handle_item_added(&mut self, v: &Value) {
        let item = match v.get("item") {
            Some(it) => it,
            None => return,
        };
        let item_type = item.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if item_type != "function_call" {
            return;
        }
        let item_id = item
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if item_id.is_empty() {
            return;
        }
        let name = item
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        // Some upstreams pre-fill `arguments` on the added event.
        let pre_args = item
            .get("arguments")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        self.function_calls.insert(
            item_id,
            FunctionCallState {
                name,
                args_buf: pre_args,
            },
        );
    }

    /// Translates `response.output_text.delta` into a single Gemini chunk
    /// carrying that delta as a `text` part.
    fn handle_text_delta(&mut self, v: &Value) -> Option<Bytes> {
        let delta = v.get("delta").and_then(|x| x.as_str()).unwrap_or("");
        if delta.is_empty() {
            return None;
        }
        Some(self.encode_chunk(json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": delta}],
                    "role": "model",
                },
                "index": 0,
            }],
            "modelVersion": self.original_model,
        })))
    }

    fn handle_args_delta(&mut self, v: &Value) {
        let item_id = v
            .get("item_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let delta = v.get("delta").and_then(|x| x.as_str()).unwrap_or("");
        if item_id.is_empty() || delta.is_empty() {
            return;
        }
        let entry = self
            .function_calls
            .entry(item_id)
            .or_insert_with(|| FunctionCallState {
                name: String::new(),
                args_buf: String::new(),
            });
        entry.args_buf.push_str(delta);
    }

    /// On `response.function_call_arguments.done`, parse the accumulated
    /// arguments string as JSON and emit a single Gemini chunk carrying
    /// `parts: [{functionCall: {name, args}}]`.
    fn handle_args_done(&mut self, v: &Value) -> Option<Bytes> {
        let item_id = v.get("item_id").and_then(|x| x.as_str()).unwrap_or("");
        if item_id.is_empty() {
            return None;
        }
        let state = self.function_calls.remove(item_id)?;
        // Prefer the explicit `arguments` field if upstream provides one;
        // otherwise use what we've buffered from the deltas.
        let raw_args = v
            .get("arguments")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or(state.args_buf);
        let args_value: Value = if raw_args.is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&raw_args).unwrap_or_else(|e| {
                warn!(
                    error = %e,
                    raw = %raw_args,
                    "gemini stream: function_call args not valid JSON; emitting empty object"
                );
                Value::Object(Map::new())
            })
        };
        self.emitted_function_call = true;
        Some(self.encode_chunk(json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": state.name,
                            "args": args_value,
                        }
                    }],
                    "role": "model",
                },
                "index": 0,
            }],
            "modelVersion": self.original_model,
        })))
    }

    /// Builds the terminal Gemini chunk carrying `finishReason` and
    /// `usageMetadata`. Idempotent: returns `None` after the first call.
    fn build_terminal_chunk(&mut self) -> Option<Bytes> {
        if self.emitted_terminal {
            return None;
        }
        self.emitted_terminal = true;

        let finish_reason = if self.emitted_function_call {
            "TOOL_USE"
        } else {
            "STOP"
        };

        let usage = self.final_usage.as_ref();
        let prompt = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let completion = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let total = usage
            .and_then(|u| u.get("total_tokens"))
            .and_then(|x| x.as_i64())
            .unwrap_or(prompt + completion);

        let chunk = json!({
            "candidates": [{
                "content": {"parts": [], "role": "model"},
                "finishReason": finish_reason,
                "index": 0,
            }],
            "modelVersion": self.original_model,
            "usageMetadata": {
                "promptTokenCount": prompt,
                "candidatesTokenCount": completion,
                "totalTokenCount": total,
            },
        });
        Some(self.encode_chunk(chunk))
    }

    fn encode_chunk(&self, payload: Value) -> Bytes {
        let body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
        let mut out = BytesMut::with_capacity(body.len() + 8);
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\n\n");
        out.freeze()
    }
}

/// Returns the index just past the first `\n\n` (or `\r\n\r\n`) blank-line
/// boundary in `buf`, or `None` if no full event has arrived yet.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    // Look for "\n\n" first; fall back to "\r\n\r\n".
    if let Some(p) = find_subslice(buf, b"\n\n") {
        return Some(p);
    }
    find_subslice(buf, b"\r\n\r\n")
}

/// How many bytes of blank-line delimiter sit at the head of `buf`.
fn blank_line_len(buf: &[u8]) -> usize {
    if buf.starts_with(b"\r\n\r\n") {
        4
    } else if buf.starts_with(b"\n\n") || buf.starts_with(b"\r\n") {
        2
    } else if buf.starts_with(b"\n") {
        1
    } else {
        0
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Events we silently ignore (no Gemini equivalent, but well-known so we
/// don't log them).
fn is_known_silent_event(name: &str) -> bool {
    matches!(
        name,
        "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning.delta"
            | "response.reasoning.done"
            | "response.refusal.delta"
            | "response.refusal.done"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, StreamExt};

    fn upstream_from_events(events: &[&str]) -> impl Stream<Item = Result<Bytes, io::Error>> {
        // Each event is already shaped like `event: foo\ndata: {...}` (no
        // trailing blank line); we add the blank-line delimiter here.
        let mut s = String::new();
        for e in events {
            s.push_str(e);
            s.push_str("\n\n");
        }
        stream::iter(vec![Ok::<_, io::Error>(Bytes::from(s))])
    }

    async fn collect_all<S>(s: S) -> String
    where
        S: Stream<Item = Result<Bytes, io::Error>> + Unpin,
    {
        let mut s = s;
        let mut out = String::new();
        while let Some(chunk) = s.next().await {
            let b = chunk.unwrap();
            out.push_str(std::str::from_utf8(&b).unwrap());
        }
        out
    }

    /// Parse the Gemini-shape stream we emit: a sequence of `data: <json>\n\n`
    /// blocks. Returns each block's parsed JSON value.
    fn parse_gemini_chunks(s: &str) -> Vec<Value> {
        let mut out = Vec::new();
        for block in s.split("\n\n") {
            let block = block.trim();
            if block.is_empty() {
                continue;
            }
            if let Some(rest) = block.strip_prefix("data: ") {
                if let Ok(v) = serde_json::from_str::<Value>(rest) {
                    out.push(v);
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn text_delta_translates_to_gemini_chunk() {
        let upstream = upstream_from_events(&[
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"Hello \"}",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"world\"}",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello world\"}]}}",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":2,\"total_tokens\":14}}}",
        ]);
        let stream =
            translate_responses_sse_to_gemini(upstream, "gemini-3-flash-preview".to_string());
        let got = collect_all(Box::pin(stream)).await;
        let chunks = parse_gemini_chunks(&got);

        assert!(
            chunks.len() >= 3,
            "expected text deltas + terminal chunk; got {chunks:?}"
        );

        // First chunk: text part "Hello ".
        let first = &chunks[0];
        assert_eq!(
            first["candidates"][0]["content"]["parts"][0]["text"],
            "Hello "
        );
        assert_eq!(first["candidates"][0]["content"]["role"], "model");
        assert_eq!(first["modelVersion"], "gemini-3-flash-preview");
        assert!(first["candidates"][0].get("finishReason").is_none());

        // Second chunk: text part "world".
        assert_eq!(
            chunks[1]["candidates"][0]["content"]["parts"][0]["text"],
            "world"
        );

        // Terminal chunk: STOP + usage.
        let last = chunks.last().unwrap();
        assert_eq!(last["candidates"][0]["finishReason"], "STOP");
        assert_eq!(last["usageMetadata"]["promptTokenCount"], 12);
        assert_eq!(last["usageMetadata"]["candidatesTokenCount"], 2);
        assert_eq!(last["usageMetadata"]["totalTokenCount"], 14);
        assert_eq!(last["modelVersion"], "gemini-3-flash-preview");
    }

    #[tokio::test]
    async fn function_call_translates_to_function_call_part() {
        let upstream = upstream_from_events(&[
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"name\":\"search\",\"call_id\":\"call_1\",\"arguments\":\"\"}}",
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"query\\\":\"}",
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"\\\"foo\\\"}\"}",
            "event: response.function_call_arguments.done\ndata: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\"}",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"name\":\"search\",\"call_id\":\"call_1\",\"arguments\":\"{\\\"query\\\":\\\"foo\\\"}\"}}",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7}}}",
        ]);
        let stream =
            translate_responses_sse_to_gemini(upstream, "gemini-3-flash-preview".to_string());
        let got = collect_all(Box::pin(stream)).await;
        let chunks = parse_gemini_chunks(&got);

        let fc_chunk = chunks
            .iter()
            .find(|c| {
                c["candidates"][0]["content"]["parts"][0]
                    .get("functionCall")
                    .is_some()
            })
            .expect("expected a functionCall chunk");
        let fc = &fc_chunk["candidates"][0]["content"]["parts"][0]["functionCall"];
        assert_eq!(fc["name"], "search");
        // args MUST be an object, not a string.
        assert!(fc["args"].is_object(), "args should be JSON object");
        assert_eq!(fc["args"]["query"], "foo");

        // Terminal chunk has TOOL_USE finish reason.
        let last = chunks.last().unwrap();
        assert_eq!(last["candidates"][0]["finishReason"], "TOOL_USE");
        assert_eq!(last["usageMetadata"]["promptTokenCount"], 3);
        assert_eq!(last["usageMetadata"]["candidatesTokenCount"], 4);
        assert_eq!(last["usageMetadata"]["totalTokenCount"], 7);
    }

    #[tokio::test]
    async fn unknown_event_does_not_break_stream() {
        let upstream = upstream_from_events(&[
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"a\"}",
            "event: garbage.event\ndata: {\"type\":\"garbage.event\",\"foo\":\"bar\"}",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"b\"}",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}",
        ]);
        let stream = translate_responses_sse_to_gemini(upstream, "gemini-3-flash-preview".into());
        let got = collect_all(Box::pin(stream)).await;
        let chunks = parse_gemini_chunks(&got);

        let texts: Vec<String> = chunks
            .iter()
            .filter_map(|c| {
                c["candidates"][0]["content"]["parts"][0]
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(texts, vec!["a".to_string(), "b".to_string()]);

        let last = chunks.last().unwrap();
        assert_eq!(last["candidates"][0]["finishReason"], "STOP");
    }
}
