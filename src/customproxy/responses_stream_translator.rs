//! OpenAI Responses streaming SSE translator.
//!
//! Ported from `internal/customproxy/responses_stream_translator.go`.
//! Wraps an upstream chat/completions SSE byte stream and re-emits
//! equivalent OpenAI Responses API SSE events to the downstream Amp CLI
//! client.
//!
//! Output event order (per response):
//!   1. `response.created` then `response.in_progress`
//!   2. zero or more `output_item.added` / `output_text.delta` /
//!      `output_item.done` triples (one per reasoning / message /
//!      function_call output item, in upstream order)
//!   3. `response.completed`

use std::io;

use bytes::{Bytes, BytesMut};
use futures::stream::{Stream, StreamExt};
use serde_json::{json, Map, Value};

use super::responses_translator::{
    synth_item_id, synth_response_id, translate_chat_usage, ResponsesTranslateCtx,
};

/// Translates an upstream chat/completions SSE byte stream into an OpenAI
/// Responses SSE byte stream.
///
/// The returned stream produces newly-framed `event: ...\ndata: ...\n\n`
/// chunks. It is guaranteed to emit a single `response.completed` event,
/// even if the upstream stream is truncated or never produces a
/// `finish_reason`.
pub fn translate_chat_to_responses_stream<S>(
    upstream: S,
    ctx: ResponsesTranslateCtx,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut state = TranslatorState::new(ctx);
        let mut buf = BytesMut::new();

        let mut upstream = Box::pin(upstream);
        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => {
                    // Surface upstream IO error to caller after closing items.
                    state.finish_if_pending();
                    if let Some(b) = state.take_out() {
                        yield Ok(b);
                    }
                    yield Err(e);
                    return;
                }
            };
            buf.extend_from_slice(&chunk);

            // Drain any complete lines from buf.
            while let Some(pos) = memchr_newline(&buf) {
                let line_bytes = buf.split_to(pos + 1);
                // Strip trailing \n and optional \r.
                let line = trim_line(&line_bytes);
                if line.is_empty() {
                    continue;
                }
                state.process_upstream_line(line);
                if state.done {
                    break;
                }
                if let Some(b) = state.take_out() {
                    yield Ok(b);
                }
            }

            if let Some(b) = state.take_out() {
                yield Ok(b);
            }
            if state.done {
                break;
            }
        }

        // End-of-stream: handle any line still in the buffer (no trailing \n).
        if !state.done && !buf.is_empty() {
            let line = trim_line(&buf);
            if !line.is_empty() {
                state.process_upstream_line(line);
            }
            buf.clear();
        }

        // Synthesize closing events if upstream ended without finish_reason.
        state.finish_if_pending();
        if let Some(b) = state.take_out() {
            yield Ok(b);
        }
    }
}

/// Per-response state machine. All emitted SSE bytes are buffered into
/// `out` and flushed by `take_out()` between processing steps.
struct TranslatorState {
    ctx: ResponsesTranslateCtx,
    out: BytesMut,
    done: bool,

    final_usage: Option<Value>,
    resp_id: String,
    created_at: i64,

    sequence: u64,
    output_index: u64,

    reasoning_buf: String,
    reasoning_open: bool,
    reasoning_item_id: String,
    reasoning_index: u64,

    message_buf: String,
    message_open: bool,
    message_item_id: String,
    message_index: u64,

    tool_calls: std::collections::BTreeMap<i64, ToolCallState>,

    output_items: Vec<Value>,

    emitted_created: bool,
    emitted_completed: bool,

    final_model: String,
}

struct ToolCallState {
    output_index: u64,
    item_id: String,
    call_id: String,
    name: String,
    args_buf: String,
    opened: bool,
    closed: bool,
}

impl TranslatorState {
    fn new(ctx: ResponsesTranslateCtx) -> Self {
        Self {
            ctx,
            out: BytesMut::new(),
            done: false,
            final_usage: None,
            resp_id: synth_response_id(),
            created_at: chrono::Utc::now().timestamp(),
            sequence: 0,
            output_index: 0,
            reasoning_buf: String::new(),
            reasoning_open: false,
            reasoning_item_id: String::new(),
            reasoning_index: 0,
            message_buf: String::new(),
            message_open: false,
            message_item_id: String::new(),
            message_index: 0,
            tool_calls: std::collections::BTreeMap::new(),
            output_items: Vec::new(),
            emitted_created: false,
            emitted_completed: false,
            final_model: String::new(),
        }
    }

    fn take_out(&mut self) -> Option<Bytes> {
        if self.out.is_empty() {
            None
        } else {
            Some(self.out.split().freeze())
        }
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.sequence;
        self.sequence += 1;
        s
    }

    fn process_upstream_line(&mut self, line: &str) {
        if !line.starts_with("data: ") {
            return;
        }
        let payload = &line[6..];
        if payload == "[DONE]" {
            self.finish_if_pending();
            self.done = true;
            return;
        }

        let v: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => return,
        };

        if !self.emitted_created {
            if let Some(m) = v.get("model").and_then(|v| v.as_str()) {
                self.final_model = m.to_string();
            }
            self.emit_created();
        }

        let finish_reason = v
            .pointer("/choices/0/finish_reason")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(u) = v.get("usage") {
            if !u.is_null() {
                self.final_usage = Some(u.clone());
            }
        }

        if let Some(rc) = v
            .pointer("/choices/0/delta/reasoning_content")
            .and_then(|x| x.as_str())
        {
            if !rc.is_empty() {
                self.handle_reasoning_delta(rc);
            }
        }
        if let Some(c) = v
            .pointer("/choices/0/delta/content")
            .and_then(|x| x.as_str())
        {
            if !c.is_empty() {
                self.handle_content_delta(c);
            }
        }
        if let Some(tcs) = v
            .pointer("/choices/0/delta/tool_calls")
            .and_then(|x| x.as_array())
        {
            for tc in tcs {
                self.handle_tool_call_delta(tc);
            }
        }

        if !finish_reason.is_empty() {
            self.finish_all(&finish_reason);
        }
    }

    fn emit_created(&mut self) {
        if self.emitted_created {
            return;
        }
        self.emitted_created = true;
        let resp = self.build_response_envelope("in_progress", &[]);
        let seq = self.next_seq();
        self.write_sse(
            "response.created",
            &json!({
                "type": "response.created",
                "response": resp,
                "sequence_number": seq,
            }),
        );
        let resp = self.build_response_envelope("in_progress", &[]);
        let seq = self.next_seq();
        self.write_sse(
            "response.in_progress",
            &json!({
                "type": "response.in_progress",
                "response": resp,
                "sequence_number": seq,
            }),
        );
    }

    fn handle_reasoning_delta(&mut self, text: &str) {
        if !self.reasoning_open {
            self.reasoning_open = true;
            self.reasoning_item_id = synth_item_id("rs");
            self.reasoning_index = self.output_index;
            self.output_index += 1;
            let item = self.reasoning_item_at("in_progress");
            let seq = self.next_seq();
            let idx = self.reasoning_index;
            self.write_sse(
                "response.output_item.added",
                &json!({
                    "type": "response.output_item.added",
                    "output_index": idx,
                    "item": item,
                    "sequence_number": seq,
                }),
            );
        }
        self.reasoning_buf.push_str(text);
    }

    fn flush_reasoning(&mut self) {
        if !self.reasoning_open {
            return;
        }
        self.reasoning_open = false;
        let item = self.reasoning_item_at("completed");
        let seq = self.next_seq();
        let idx = self.reasoning_index;
        self.write_sse(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": idx,
                "item": item.clone(),
                "sequence_number": seq,
            }),
        );
        self.output_items.push(item);
    }

    fn reasoning_item_at(&self, status: &str) -> Value {
        let txt = self.reasoning_buf.clone();
        json!({
            "id": self.reasoning_item_id,
            "type": "reasoning",
            "status": status,
            "encrypted_content": txt,
            "summary": [{"type": "summary_text", "text": txt}],
        })
    }

    fn handle_content_delta(&mut self, text: &str) {
        // Reasoning closes before content (OpenAI ordering).
        if self.reasoning_open {
            self.flush_reasoning();
        }
        if !self.message_open {
            self.message_open = true;
            self.message_item_id = synth_item_id("msg");
            self.message_index = self.output_index;
            self.output_index += 1;
            let item = self.message_item_base("in_progress", None);
            let seq = self.next_seq();
            let idx = self.message_index;
            self.write_sse(
                "response.output_item.added",
                &json!({
                    "type": "response.output_item.added",
                    "output_index": idx,
                    "item": item,
                    "sequence_number": seq,
                }),
            );
            let seq = self.next_seq();
            let item_id = self.message_item_id.clone();
            self.write_sse(
                "response.content_part.added",
                &json!({
                    "type": "response.content_part.added",
                    "content_index": 0,
                    "item_id": item_id,
                    "output_index": idx,
                    "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""},
                    "sequence_number": seq,
                }),
            );
        }
        self.message_buf.push_str(text);
        let seq = self.next_seq();
        let item_id = self.message_item_id.clone();
        let idx = self.message_index;
        self.write_sse(
            "response.output_text.delta",
            &json!({
                "type": "response.output_text.delta",
                "content_index": 0,
                "delta": text,
                "item_id": item_id,
                "logprobs": [],
                "output_index": idx,
                "sequence_number": seq,
            }),
        );
    }

    fn flush_message(&mut self) {
        if !self.message_open {
            return;
        }
        self.message_open = false;
        let full = self.message_buf.clone();
        let seq1 = self.next_seq();
        let item_id = self.message_item_id.clone();
        let idx = self.message_index;
        self.write_sse(
            "response.output_text.done",
            &json!({
                "type": "response.output_text.done",
                "content_index": 0,
                "item_id": item_id,
                "logprobs": [],
                "output_index": idx,
                "text": full,
                "sequence_number": seq1,
            }),
        );
        let seq2 = self.next_seq();
        let item_id = self.message_item_id.clone();
        self.write_sse(
            "response.content_part.done",
            &json!({
                "type": "response.content_part.done",
                "content_index": 0,
                "item_id": item_id,
                "output_index": idx,
                "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": full},
                "sequence_number": seq2,
            }),
        );
        let content_part = json!({
            "type": "output_text",
            "annotations": [],
            "logprobs": [],
            "text": full,
        });
        let item = self.message_item_base("completed", Some(vec![content_part]));
        let seq3 = self.next_seq();
        self.write_sse(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": idx,
                "item": item.clone(),
                "sequence_number": seq3,
            }),
        );
        self.output_items.push(item);
    }

    fn message_item_base(&self, status: &str, content: Option<Vec<Value>>) -> Value {
        let c = content.unwrap_or_default();
        json!({
            "id": self.message_item_id,
            "type": "message",
            "status": status,
            "role": "assistant",
            "content": c,
        })
    }

    fn handle_tool_call_delta(&mut self, tc: &Value) {
        let idx = tc.get("index").and_then(|v| v.as_i64()).unwrap_or(0);

        // Insert state if not seen before.
        let entry = self.tool_calls.entry(idx).or_insert_with(|| ToolCallState {
            output_index: 0,
            item_id: String::new(),
            call_id: String::new(),
            name: String::new(),
            args_buf: String::new(),
            opened: false,
            closed: false,
        });

        if !entry.opened {
            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    entry.call_id = id.to_string();
                }
            }
            if let Some(n) = tc.pointer("/function/name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    entry.name = n.to_string();
                }
            }
            if entry.call_id.is_empty() || entry.name.is_empty() {
                if let Some(a) = tc.pointer("/function/arguments").and_then(|v| v.as_str()) {
                    if !a.is_empty() {
                        entry.args_buf.push_str(a);
                    }
                }
                return;
            }

            // Defer assignment of output_index + ID until prior items closed.
            // We need to drop the entry borrow first so we can call &mut self
            // methods on flush.
            let call_id = entry.call_id.clone();
            let name = entry.name.clone();
            let mut args_buf_take = std::mem::take(&mut entry.args_buf);
            // Mark opened to avoid re-entering this branch on the same delta.
            entry.opened = true;
            // Now flush prior open items.
            self.flush_reasoning();
            self.flush_message();
            // Re-acquire entry to set the index/id (state machine needs current
            // self.output_index).
            let item_id = synth_item_id("fc");
            let oi = self.output_index;
            self.output_index += 1;
            {
                let entry = self.tool_calls.get_mut(&idx).expect("just inserted");
                entry.output_index = oi;
                entry.item_id = item_id.clone();
            }
            // Append args delta from this very chunk if any present.
            if let Some(a) = tc.pointer("/function/arguments").and_then(|v| v.as_str()) {
                if !a.is_empty() {
                    args_buf_take.push_str(a);
                }
            }
            // Save back the args_buf.
            {
                let entry = self.tool_calls.get_mut(&idx).expect("just inserted");
                entry.args_buf = args_buf_take.clone();
            }
            // Emit output_item.added with current args_buf.
            let item = json!({
                "id": item_id,
                "type": "function_call",
                "status": "in_progress",
                "arguments": args_buf_take.clone(),
                "call_id": call_id,
                "name": name,
            });
            let seq = self.next_seq();
            self.write_sse(
                "response.output_item.added",
                &json!({
                    "type": "response.output_item.added",
                    "output_index": oi,
                    "item": item,
                    "sequence_number": seq,
                }),
            );
            // Emit args delta event for this chunk's argument fragment, if any.
            if !args_buf_take.is_empty() {
                let seq = self.next_seq();
                let item_id_c = item_id;
                self.write_sse(
                    "response.function_call_arguments.delta",
                    &json!({
                        "type": "response.function_call_arguments.delta",
                        "delta": args_buf_take,
                        "item_id": item_id_c,
                        "output_index": oi,
                        "sequence_number": seq,
                    }),
                );
            }
            return;
        }

        // Already opened — just pump arg fragments.
        let mut delta_to_emit: Option<(String, String, u64)> = None;
        if let Some(a) = tc.pointer("/function/arguments").and_then(|v| v.as_str()) {
            if !a.is_empty() {
                let entry = self.tool_calls.get_mut(&idx).expect("present");
                entry.args_buf.push_str(a);
                delta_to_emit = Some((a.to_string(), entry.item_id.clone(), entry.output_index));
            }
        }
        if let Some((delta, item_id, oi)) = delta_to_emit {
            let seq = self.next_seq();
            self.write_sse(
                "response.function_call_arguments.delta",
                &json!({
                    "type": "response.function_call_arguments.delta",
                    "delta": delta,
                    "item_id": item_id,
                    "output_index": oi,
                    "sequence_number": seq,
                }),
            );
        }
    }

    fn flush_tool_calls(&mut self) {
        if self.tool_calls.is_empty() {
            return;
        }
        // BTreeMap iterates in sorted-key order, matching upstream tool_call
        // delta index ordering.
        let keys: Vec<i64> = self.tool_calls.keys().copied().collect();
        for k in keys {
            let already_closed = matches!(self.tool_calls.get(&k), Some(s) if s.closed);
            if already_closed {
                continue;
            }
            self.close_tool_call(k);
        }
    }

    fn close_tool_call(&mut self, idx: i64) {
        let Some(st) = self.tool_calls.get(&idx) else {
            return;
        };
        if !st.opened {
            return;
        }
        let item_id = st.item_id.clone();
        let call_id = st.call_id.clone();
        let name = st.name.clone();
        let args = st.args_buf.clone();
        let oi = st.output_index;
        let seq = self.next_seq();
        self.write_sse(
            "response.function_call_arguments.done",
            &json!({
                "type": "response.function_call_arguments.done",
                "arguments": args,
                "item_id": item_id,
                "output_index": oi,
                "sequence_number": seq,
            }),
        );
        let item = json!({
            "id": item_id,
            "type": "function_call",
            "status": "completed",
            "arguments": args,
            "call_id": call_id,
            "name": name,
        });
        let seq = self.next_seq();
        self.write_sse(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": oi,
                "item": item.clone(),
                "sequence_number": seq,
            }),
        );
        self.output_items.push(item);
        if let Some(st) = self.tool_calls.get_mut(&idx) {
            st.closed = true;
        }
    }

    fn finish_all(&mut self, finish_reason: &str) {
        if !self.emitted_created {
            self.emit_created();
        }
        self.flush_reasoning();
        if finish_reason == "tool_calls" {
            self.flush_message();
            self.flush_tool_calls();
        } else {
            self.flush_message();
        }
        self.emit_completed();
    }

    fn finish_if_pending(&mut self) {
        if !self.emitted_created {
            self.emit_created();
        }
        self.flush_reasoning();
        self.flush_message();
        self.flush_tool_calls();
        self.emit_completed();
    }

    fn emit_completed(&mut self) {
        if self.emitted_completed {
            return;
        }
        self.emitted_completed = true;
        let items_owned: Vec<Value> = self.output_items.clone();
        let resp = self.build_response_envelope("completed", &items_owned);
        let seq = self.next_seq();
        self.write_sse(
            "response.completed",
            &json!({
                "type": "response.completed",
                "response": resp,
                "sequence_number": seq,
            }),
        );
    }

    fn build_response_envelope(&self, status: &str, output: &[Value]) -> Value {
        let model = if !self.ctx.orig_model.is_empty() {
            self.ctx.orig_model.clone()
        } else {
            self.final_model.clone()
        };
        let mut env = Map::new();
        env.insert("id".into(), Value::String(self.resp_id.clone()));
        env.insert("object".into(), Value::String("response".into()));
        env.insert("created_at".into(), Value::from(self.created_at));
        env.insert("status".into(), Value::String(status.into()));
        env.insert("background".into(), Value::Bool(false));
        env.insert("error".into(), Value::Null);
        env.insert("incomplete_details".into(), Value::Null);
        env.insert("instructions".into(), Value::Null);
        env.insert("max_output_tokens".into(), Value::Null);
        env.insert("max_tool_calls".into(), Value::Null);
        env.insert("model".into(), Value::String(model));
        env.insert("output".into(), Value::Array(output.to_vec()));
        env.insert("parallel_tool_calls".into(), Value::Bool(true));
        env.insert("previous_response_id".into(), Value::Null);
        env.insert(
            "reasoning".into(),
            json!({"effort": "auto", "summary": "auto"}),
        );
        env.insert("store".into(), Value::Bool(false));
        env.insert("temperature".into(), Value::from(1.0));
        env.insert("top_p".into(), Value::from(1.0));
        env.insert(
            "usage".into(),
            translate_chat_usage(self.final_usage.as_ref()),
        );
        if status == "completed" {
            env.insert(
                "completed_at".into(),
                Value::from(chrono::Utc::now().timestamp()),
            );
        } else {
            env.insert("completed_at".into(), Value::Null);
        }
        if !self.ctx.prompt_cache_key.is_empty() {
            env.insert(
                "prompt_cache_key".into(),
                Value::String(self.ctx.prompt_cache_key.clone()),
            );
        }
        Value::Object(env)
    }

    fn write_sse(&mut self, event_name: &str, payload: &Value) {
        let body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
        self.out.extend_from_slice(b"event: ");
        self.out.extend_from_slice(event_name.as_bytes());
        self.out.extend_from_slice(b"\ndata: ");
        self.out.extend_from_slice(body.as_bytes());
        self.out.extend_from_slice(b"\n\n");
    }
}

fn memchr_newline(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|b| *b == b'\n')
}

fn trim_line(b: &[u8]) -> &str {
    let mut end = b.len();
    while end > 0 && (b[end - 1] == b'\n' || b[end - 1] == b'\r') {
        end -= 1;
    }
    std::str::from_utf8(&b[..end]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::{self, StreamExt};

    fn upstream_from_events(events: &[&str]) -> impl Stream<Item = Result<Bytes, io::Error>> {
        let mut s = String::new();
        for e in events {
            s.push_str("data: ");
            s.push_str(e);
            s.push_str("\n\n");
        }
        s.push_str("data: [DONE]\n\n");
        // Single chunk; the translator's line-buffering still exercises.
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

    #[derive(Debug)]
    struct SseEvent {
        name: String,
        data: String,
    }

    fn parse_events(s: &str) -> Vec<SseEvent> {
        let mut out = Vec::new();
        let mut cur_name = String::new();
        for line in s.split('\n') {
            if let Some(rest) = line.strip_prefix("event: ") {
                cur_name = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                out.push(SseEvent {
                    name: std::mem::take(&mut cur_name),
                    data: rest.to_string(),
                });
            }
        }
        out
    }

    fn contains_in_order(haystack: &[String], needles: &[&str]) -> bool {
        let mut i = 0;
        for h in haystack {
            if i < needles.len() && h == needles[i] {
                i += 1;
            }
        }
        i == needles.len()
    }

    #[tokio::test]
    async fn single_text_chunk_emits_correct_event_order() {
        let upstream = upstream_from_events(&[
            r#"{"choices":[{"delta":{"role":"assistant"}}],"model":"deepseek-v4-pro"}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"let me "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"think."}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hello"}}]}"#,
            r#"{"choices":[{"delta":{"content":" world"}}]}"#,
            r#"{"choices":[{"finish_reason":"stop","delta":{}}]}"#,
        ]);
        let ctx = ResponsesTranslateCtx {
            orig_model: "gpt-5.4".into(),
            stream: true,
            prompt_cache_key: String::new(),
        };
        let stream = translate_chat_to_responses_stream(upstream, ctx);
        let got = collect_all(Box::pin(stream)).await;
        let events = parse_events(&got);
        assert!(!events.is_empty(), "no events produced. got={got}");

        let names: Vec<String> = events.iter().map(|e| e.name.clone()).collect();
        let expected = [
            "response.created",
            "response.in_progress",
            "response.output_item.added", // reasoning
            "response.output_item.done",  // reasoning
            "response.output_item.added", // message
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done", // message
            "response.completed",
        ];
        assert!(
            contains_in_order(&names, &expected),
            "events out of order.\ngot: {names:?}"
        );

        let completed = events
            .iter()
            .find(|e| e.name == "response.completed")
            .expect("response.completed");
        let v: Value = serde_json::from_str(&completed.data).unwrap();
        assert_eq!(v["response"]["model"], "gpt-5.4", "origModel must win");
        let out = v["response"]["output"].as_array().unwrap();
        assert_eq!(out.len(), 2, "reasoning + message");
        assert_eq!(out[0]["type"], "reasoning");
        assert!(out[0]["summary"][0]["text"]
            .as_str()
            .unwrap()
            .contains("let me think."));
        assert_eq!(out[1]["type"], "message");
        assert_eq!(out[1]["content"][0]["text"], "Hello world");

        // sequence_numbers strictly increasing.
        let mut prev: i64 = -1;
        for e in &events {
            let v: Value = serde_json::from_str(&e.data).unwrap();
            if let Some(seq) = v.get("sequence_number").and_then(|x| x.as_i64()) {
                assert!(seq > prev, "non-monotonic sequence at {}", e.name);
                prev = seq;
            }
        }

        // Exactly one completed event.
        let completed_count = events
            .iter()
            .filter(|e| e.name == "response.completed")
            .count();
        assert_eq!(completed_count, 1);
    }

    #[tokio::test]
    async fn function_call_chunks_translate() {
        let upstream = upstream_from_events(&[
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"must call tool"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_42","type":"function","function":{"name":"shell_command","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]}}]}"#,
            r#"{"choices":[{"finish_reason":"tool_calls","delta":{}}]}"#,
        ]);
        let ctx = ResponsesTranslateCtx {
            orig_model: "gpt-5.4".into(),
            stream: true,
            prompt_cache_key: String::new(),
        };
        let stream = translate_chat_to_responses_stream(upstream, ctx);
        let got = collect_all(Box::pin(stream)).await;
        let events = parse_events(&got);

        let mut args_delta = 0;
        let mut args_done = 0;
        for e in &events {
            match e.name.as_str() {
                "response.function_call_arguments.delta" => args_delta += 1,
                "response.function_call_arguments.done" => args_done += 1,
                _ => {}
            }
        }
        assert!(args_delta > 0, "expected args.delta events; got {got}");
        assert_eq!(args_done, 1);

        let completed = events
            .iter()
            .find(|e| e.name == "response.completed")
            .expect("response.completed");
        let v: Value = serde_json::from_str(&completed.data).unwrap();
        let output = v["response"]["output"].as_array().unwrap();
        let fc = output
            .iter()
            .find(|it| it["type"] == "function_call")
            .expect("function_call item");
        assert_eq!(fc["call_id"], "call_42");
        assert_eq!(fc["name"], "shell_command");
        assert_eq!(fc["arguments"], r#"{"cmd":"ls"}"#);
    }

    #[tokio::test]
    async fn maps_usage_from_final_chunk() {
        let upstream = upstream_from_events(&[
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"{"choices":[{"finish_reason":"stop","delta":{}}],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}"#,
        ]);
        let stream = translate_chat_to_responses_stream(upstream, ResponsesTranslateCtx::default());
        let got = collect_all(Box::pin(stream)).await;
        let events = parse_events(&got);
        let completed = events
            .iter()
            .find(|e| e.name == "response.completed")
            .expect("response.completed");
        let v: Value = serde_json::from_str(&completed.data).unwrap();
        assert_eq!(v["response"]["usage"]["input_tokens"], 7);
        assert_eq!(v["response"]["usage"]["output_tokens"], 2);
    }
}
