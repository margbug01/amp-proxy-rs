//! OpenAI Responses → Anthropic Messages request translator, and
//! Anthropic Messages response → Gemini response translator.
//!
//! Used by the Gemini bridge when the target provider only speaks the
//! Anthropic Messages API (e.g. DeepSeek's `https://api.deepseek.com/anthropic`).
//!
//! Flow: Gemini request → gemini_translator → OpenAI Responses body
//!       → **this module** → Anthropic Messages body → POST /v1/messages
//!       → Anthropic response → **this module** → Gemini response body.

use std::io;

use anyhow::{anyhow, Context, Result};
use bytes::{Bytes, BytesMut};
use futures::stream::{Stream, StreamExt};
use serde_json::{json, Map, Value};
use tracing::warn;

// ---------------------------------------------------------------------------
// Request translation: OpenAI Responses → Anthropic Messages
// ---------------------------------------------------------------------------

/// Converts an OpenAI Responses request body into an Anthropic Messages
/// request body suitable for `POST /v1/messages`.
pub fn translate_responses_to_messages(body: &[u8]) -> Result<Vec<u8>> {
    let req: Value = serde_json::from_slice(body).context("parse responses request")?;
    let req_obj = req
        .as_object()
        .ok_or_else(|| anyhow!("responses request must be a JSON object"))?;

    let mut out = Map::new();

    // model
    if let Some(m) = req_obj.get("model").and_then(|v| v.as_str()) {
        out.insert("model".into(), Value::String(m.into()));
    }

    // max_output_tokens → max_tokens (default 8192)
    let max_tokens = req_obj
        .get("max_output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(8192);
    out.insert("max_tokens".into(), Value::from(max_tokens));

    // stream
    let stream = req_obj
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    out.insert("stream".into(), Value::Bool(stream));

    // tools
    if let Some(tools) = req_obj.get("tools").and_then(|v| v.as_array()) {
        let translated: Vec<Value> = tools.iter().filter_map(translate_tool).collect();
        if !translated.is_empty() {
            out.insert("tools".into(), Value::Array(translated));
        }
    }

    // input → system + messages
    let empty: Vec<Value> = Vec::new();
    let input = req_obj
        .get("input")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();

    for item in input {
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if role == "system" {
            // Collect system text
            let text = extract_text_from_content(item);
            if !text.is_empty() {
                system_parts.push(text);
            }
            continue;
        }

        match item_type {
            "reasoning" => {} // skip — Anthropic has no equivalent input shape
            "function_call" => {
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args_str = item
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let args_val: Value =
                    serde_json::from_str(args_str).unwrap_or(Value::Object(Map::new()));
                messages.push(json!({
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": args_val,
                    }]
                }));
            }
            "function_call_output" => {
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                let output = item.get("output").and_then(|v| v.as_str()).unwrap_or("");
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": output,
                    }]
                }));
            }
            "message" => {
                // type: "message" with nested content array
                let msg_role = item
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("assistant");
                let text = extract_message_output_text(item);
                if !text.is_empty() {
                    messages.push(json!({
                        "role": msg_role,
                        "content": [{"type": "text", "text": text}]
                    }));
                }
            }
            _ => {
                // Plain role-based items (user / assistant)
                let msg_role = if role.is_empty() { "user" } else { role };
                let text = extract_text_from_content(item);
                if !text.is_empty() {
                    messages.push(json!({
                        "role": msg_role,
                        "content": [{"type": "text", "text": text}]
                    }));
                }
            }
        }
    }

    if !system_parts.is_empty() {
        out.insert("system".into(), Value::String(system_parts.join("\n")));
    }
    normalize_tool_results(&mut messages);

    if messages.is_empty() {
        let fallback = system_parts
            .last()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.as_str())
            .unwrap_or("Continue.");
        messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": fallback}]
        }));
    }
    out.insert("messages".into(), Value::Array(messages));

    serde_json::to_vec(&Value::Object(out)).context("marshal messages request")
}

/// Extract text from a Responses input item. Handles both string content
/// and array-of-parts content (with `input_text` / `output_text` / `text`).
fn extract_text_from_content(item: &Value) -> String {
    if let Some(s) = item.get("content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(parts) = item.get("content").and_then(|v| v.as_array()) {
        let mut buf = String::new();
        for part in parts {
            let t = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let text = match t {
                "input_text" | "output_text" | "text" => {
                    part.get("text").and_then(|v| v.as_str()).unwrap_or("")
                }
                _ => "",
            };
            if !text.is_empty() {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(text);
            }
        }
        return buf;
    }
    String::new()
}

/// Extract text from a `type: "message"` output item's content array.
fn extract_message_output_text(item: &Value) -> String {
    let empty: Vec<Value> = Vec::new();
    let parts = item
        .get("content")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let mut buf = String::new();
    for part in parts {
        let t = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == "output_text" || t == "text" {
            let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !text.is_empty() {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(text);
            }
        }
    }
    buf
}

/// Convert a Responses tool to Anthropic tool shape.
fn translate_tool(tool: &Value) -> Option<Value> {
    let obj = tool.as_object()?;
    let name = obj.get("name").and_then(|v| v.as_str())?;
    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let parameters = obj.get("parameters").cloned().unwrap_or(json!({}));
    Some(json!({
        "name": name,
        "description": description,
        "input_schema": parameters,
    }))
}

fn normalize_tool_results(messages: &mut Vec<Value>) {
    let mut out = Vec::with_capacity(messages.len());
    let mut i = 0;

    while i < messages.len() {
        let msg = messages[i].clone();
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let tool_ids = if role == "assistant" {
            tool_use_ids(&msg)
        } else {
            Vec::new()
        };

        if tool_ids.is_empty() {
            out.push(convert_orphan_tool_results(msg));
            i += 1;
            continue;
        }

        out.push(msg);
        let mut tool_results = Vec::new();
        let mut orphan_results = Vec::new();
        let mut j = i + 1;

        while j < messages.len() {
            if messages[j].get("role").and_then(|v| v.as_str()) != Some("user") {
                break;
            }
            let blocks = messages[j]
                .get("content")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if blocks
                .iter()
                .all(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
            {
                for block in blocks {
                    let id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if tool_ids.iter().any(|known| known == id) {
                        tool_results.push(block);
                    } else {
                        orphan_results.push(block);
                    }
                }
                j += 1;
            } else {
                break;
            }
        }

        for id in tool_ids {
            if !tool_results
                .iter()
                .any(|b| b.get("tool_use_id").and_then(|v| v.as_str()) == Some(id.as_str()))
            {
                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": ""
                }));
            }
        }

        out.push(json!({"role": "user", "content": tool_results}));
        if !orphan_results.is_empty() {
            out.push(tool_results_as_text_message(&orphan_results));
        }
        i = j;
    }

    *messages = out;
}

fn tool_use_ids(msg: &Value) -> Vec<String> {
    msg.get("content")
        .and_then(|v| v.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                .filter_map(|b| b.get("id").and_then(|v| v.as_str()))
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn convert_orphan_tool_results(msg: Value) -> Value {
    if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
        return msg;
    }
    let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) else {
        return msg;
    };
    if !blocks
        .iter()
        .any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result"))
    {
        return msg;
    }
    tool_results_as_text_message(blocks)
}

fn tool_results_as_text_message(blocks: &[Value]) -> Value {
    let mut text_parts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
            let id = block
                .get("tool_use_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let content = block_to_text(block.get("content")).unwrap_or_default();
            text_parts.push(format!("Tool result {id}: {content}"));
        } else if let Some(text) = block_to_text(Some(block)) {
            text_parts.push(text);
        }
    }

    let text = if text_parts.is_empty() {
        "Tool result.".to_string()
    } else {
        text_parts.join("\n")
    };
    json!({"role": "user", "content": [{"type": "text", "text": text}]})
}

fn block_to_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let texts: Vec<String> = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string)
                        .or_else(|| part.as_str().map(ToString::to_string))
                })
                .collect();
            if texts.is_empty() {
                serde_json::to_string(parts).ok()
            } else {
                Some(texts.join("\n"))
            }
        }
        other => serde_json::to_string(other).ok(),
    }
}

// ---------------------------------------------------------------------------
// Response translation: Anthropic Messages → Gemini
// ---------------------------------------------------------------------------

/// Converts a non-streaming Anthropic Messages JSON response into a Gemini
/// `generateContent` JSON response.
pub fn translate_messages_response_to_gemini(body: &[u8], original_model: &str) -> Result<Vec<u8>> {
    if body.is_empty() {
        return build_empty_gemini_response(original_model);
    }
    let v: Value = serde_json::from_slice(body).context("parse messages response")?;
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow!("messages response must be a JSON object"))?;

    // Verify it looks like an Anthropic Messages response.
    let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if msg_type != "message" {
        return Err(anyhow!(
            "not an Anthropic Messages response (type={msg_type:?})"
        ));
    }

    let empty: Vec<Value> = Vec::new();
    let content = obj
        .get("content")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let mut parts: Vec<Value> = Vec::new();
    for block in content {
        let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    parts.push(json!({"text": text}));
                }
            }
            "tool_use" => {
                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = block.get("input").cloned().unwrap_or(json!({}));
                parts.push(json!({"functionCall": {"name": name, "args": args}}));
            }
            _ => {}
        }
    }

    let stop_reason = obj
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    let finish_reason = match stop_reason {
        "end_turn" | "stop" => "STOP",
        "tool_use" => "STOP",
        "max_tokens" => "MAX_TOKENS",
        _ => "STOP",
    };

    let usage_in = v
        .pointer("/usage/input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let usage_out = v
        .pointer("/usage/output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let resp = json!({
        "candidates": [{
            "content": {
                "parts": parts,
                "role": "model",
            },
            "finishReason": finish_reason,
            "index": 0,
        }],
        "usageMetadata": {
            "promptTokenCount": usage_in,
            "candidatesTokenCount": usage_out,
            "totalTokenCount": usage_in + usage_out,
        },
        "modelVersion": original_model,
    });
    serde_json::to_vec(&resp).context("marshal gemini response")
}

fn build_empty_gemini_response(original_model: &str) -> Result<Vec<u8>> {
    let resp = json!({
        "candidates": [{
            "content": {"parts": [], "role": "model"},
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": {
            "promptTokenCount": 0,
            "candidatesTokenCount": 0,
            "totalTokenCount": 0,
        },
        "modelVersion": original_model,
    });
    serde_json::to_vec(&resp).context("marshal empty gemini response")
}

// ---------------------------------------------------------------------------
// Streaming: Anthropic Messages SSE → Gemini SSE
// ---------------------------------------------------------------------------

/// Translates an Anthropic Messages SSE stream into a Gemini-shape SSE
/// stream for `:streamGenerateContent`.
pub fn translate_messages_sse_to_gemini<S>(
    upstream: S,
    original_model: String,
) -> impl Stream<Item = Result<Bytes, io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut state = MessagesStreamState::new(original_model);
        let mut buf: Vec<u8> = Vec::new();

        let mut upstream = Box::pin(upstream);
        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => {
                    if !state.emitted_terminal {
                        if let Some(b) = state.build_terminal_chunk() {
                            yield Ok(b);
                        }
                    }
                    yield Err(e);
                    return;
                }
            };
            buf.extend_from_slice(&chunk);

            while let Some(end) = find_event_boundary(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..end).collect();
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

        if !state.emitted_terminal && !buf.is_empty() {
            let event_text = String::from_utf8_lossy(&buf).to_string();
            buf.clear();
            if !event_text.trim().is_empty() {
                for out in state.process_event(&event_text) {
                    yield Ok(out);
                }
            }
        }

        if !state.emitted_terminal {
            if let Some(b) = state.build_terminal_chunk() {
                yield Ok(b);
            }
        }
    }
}

struct MessagesStreamState {
    original_model: String,
    usage_in: i64,
    usage_out: i64,
    emitted_terminal: bool,
    finish_reason: String,
    // current content block state
    current_block_type: String,
    tool_name: String,
    tool_id: String,
    tool_args_buf: String,
}

impl MessagesStreamState {
    fn new(original_model: String) -> Self {
        Self {
            original_model,
            usage_in: 0,
            usage_out: 0,
            emitted_terminal: false,
            finish_reason: "STOP".into(),
            current_block_type: String::new(),
            tool_name: String::new(),
            tool_id: String::new(),
            tool_args_buf: String::new(),
        }
    }

    fn process_event(&mut self, event_text: &str) -> Vec<Bytes> {
        let mut out: Vec<Bytes> = Vec::new();

        let mut event_type = String::new();
        let mut data_payload = String::new();
        for line in event_text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("event: ") {
                event_type = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("event:") {
                event_type = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
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
        }

        if data_payload.is_empty() {
            return out;
        }

        let v: Value = match serde_json::from_str(&data_payload) {
            Ok(v) => v,
            Err(_) => return out,
        };

        match event_type.as_str() {
            "message_start" => {
                // Extract usage from the initial message envelope.
                if let Some(u) = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(|x| x.as_i64())
                {
                    self.usage_in = u;
                }
            }
            "content_block_start" => {
                let block_type = v
                    .pointer("/content_block/type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                self.current_block_type = block_type.to_string();
                if block_type == "tool_use" {
                    self.tool_name = v
                        .pointer("/content_block/name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.tool_id = v
                        .pointer("/content_block/id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.tool_args_buf.clear();
                }
            }
            "content_block_delta" => {
                let delta_type = v
                    .pointer("/delta/type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        let text = v
                            .pointer("/delta/text")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        if !text.is_empty() {
                            out.push(self.gemini_text_chunk(text));
                        }
                    }
                    "input_json_delta" => {
                        let partial = v
                            .pointer("/delta/partial_json")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        self.tool_args_buf.push_str(partial);
                    }
                    "thinking_delta" => {} // skip thinking content
                    _ => {}
                }
            }
            "content_block_stop" => {
                if self.current_block_type == "tool_use" {
                    let args: Value = serde_json::from_str(&self.tool_args_buf)
                        .unwrap_or(Value::Object(Map::new()));
                    out.push(self.gemini_function_call_chunk(&self.tool_name.clone(), &args));
                    self.finish_reason = "STOP".into();
                }
                self.current_block_type.clear();
            }
            "message_delta" => {
                if let Some(sr) = v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                    self.finish_reason = match sr {
                        "max_tokens" => "MAX_TOKENS".into(),
                        _ => "STOP".into(),
                    };
                }
                if let Some(u) = v.pointer("/usage/output_tokens").and_then(|x| x.as_i64()) {
                    self.usage_out = u;
                }
            }
            "message_stop" => {
                if let Some(b) = self.build_terminal_chunk() {
                    out.push(b);
                }
            }
            "ping" | "error" => {
                if event_type == "error" {
                    warn!(payload = %data_payload, "messages→gemini stream: upstream error event");
                    if let Some(b) = self.build_terminal_chunk() {
                        out.push(b);
                    }
                }
            }
            _ => {}
        }

        out
    }

    fn gemini_text_chunk(&self, text: &str) -> Bytes {
        let chunk = json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": text}],
                    "role": "model",
                },
                "index": 0,
            }],
            "modelVersion": self.original_model,
        });
        format_sse_data(&chunk)
    }

    fn gemini_function_call_chunk(&self, name: &str, args: &Value) -> Bytes {
        let chunk = json!({
            "candidates": [{
                "content": {
                    "parts": [{"functionCall": {"name": name, "args": args}}],
                    "role": "model",
                },
                "index": 0,
            }],
            "modelVersion": self.original_model,
        });
        format_sse_data(&chunk)
    }

    fn build_terminal_chunk(&mut self) -> Option<Bytes> {
        if self.emitted_terminal {
            return None;
        }
        self.emitted_terminal = true;
        let chunk = json!({
            "candidates": [{
                "content": {"parts": [], "role": "model"},
                "finishReason": self.finish_reason,
                "index": 0,
            }],
            "usageMetadata": {
                "promptTokenCount": self.usage_in,
                "candidatesTokenCount": self.usage_out,
                "totalTokenCount": self.usage_in + self.usage_out,
            },
            "modelVersion": self.original_model,
        });
        Some(format_sse_data(&chunk))
    }
}

fn format_sse_data(v: &Value) -> Bytes {
    let json_str = serde_json::to_string(v).unwrap_or_default();
    let mut buf = BytesMut::with_capacity(6 + json_str.len() + 2);
    buf.extend_from_slice(b"data: ");
    buf.extend_from_slice(json_str.as_bytes());
    buf.extend_from_slice(b"\n\n");
    buf.freeze()
}

/// Find the position of a blank-line boundary ("\n\n" or "\r\n\r\n") that
/// signals the end of an SSE event.
fn find_event_boundary(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some(i);
        }
    }
    None
}

fn blank_line_len(buf: &[u8]) -> usize {
    if buf.starts_with(b"\r\n\r\n") {
        4
    } else if buf.starts_with(b"\n\n") {
        2
    } else if buf.starts_with(b"\r\n") || buf.starts_with(b"\n") {
        // Single line ending (either \r\n or \n) — consume the delimiter
        // so the next event parse starts cleanly.
        if buf.starts_with(b"\r\n") {
            2
        } else {
            1
        }
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_request_basic_with_system() {
        let body = serde_json::to_vec(&json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": [{"type": "input_text", "text": "Hi"}]},
            ],
            "stream": true,
            "max_output_tokens": 4096,
        }))
        .unwrap();

        let out = translate_responses_to_messages(&body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["model"], "deepseek-v4-flash");
        assert_eq!(v["system"], "You are helpful.");
        assert_eq!(v["max_tokens"], 4096);
        assert_eq!(v["stream"], true);

        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "Hi");
    }

    #[test]
    fn translate_request_system_only_adds_fallback_user_message() {
        let body = serde_json::to_vec(&json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"role": "system", "content": "You are helpful."},
            ],
            "stream": false,
        }))
        .unwrap();

        let out = translate_responses_to_messages(&body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["system"], "You are helpful.");
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "You are helpful.");
    }

    #[test]
    fn translate_request_empty_input_adds_default_user_message() {
        let body = serde_json::to_vec(&json!({
            "model": "deepseek-v4-flash",
            "input": [],
            "stream": false,
        }))
        .unwrap();

        let out = translate_responses_to_messages(&body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();

        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["text"], "Continue.");
    }

    #[test]
    fn translate_request_with_tool_calls() {
        let body = serde_json::to_vec(&json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"role": "user", "content": "call the tool"},
                {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{\"city\":\"NYC\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "sunny"},
            ],
            "tools": [{"type": "function", "name": "get_weather", "description": "Get weather", "parameters": {"type": "object"}}],
        }))
        .unwrap();

        let out = translate_responses_to_messages(&body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();

        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["name"], "get_weather");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "c1");

        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "get_weather");
        assert!(tools[0].get("input_schema").is_some());
    }

    #[test]
    fn translate_request_adds_missing_tool_result_immediately_after_tool_use() {
        let body = serde_json::to_vec(&json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"role": "user", "content": "call the tool"},
                {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{}"},
                {"role": "assistant", "content": [{"type": "output_text", "text": "later text"}]},
            ],
        }))
        .unwrap();

        let out = translate_responses_to_messages(&body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();

        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "c1");
        assert_eq!(msgs[3]["role"], "assistant");
    }

    #[test]
    fn translate_request_keeps_tool_results_immediately_after_tool_use() {
        let body = serde_json::to_vec(&json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"role": "user", "content": "call the tool"},
                {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "sunny"},
                {"role": "assistant", "content": [{"type": "output_text", "text": "It is sunny."}]},
            ],
        }))
        .unwrap();

        let out = translate_responses_to_messages(&body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();

        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["content"], "sunny");
        assert_eq!(msgs[3]["content"][0]["text"], "It is sunny.");
    }

    #[test]
    fn translate_request_converts_orphan_tool_result_to_text() {
        let body = serde_json::to_vec(&json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"type": "function_call_output", "call_id": "call_gf_0", "output": "done"},
            ],
        }))
        .unwrap();

        let out = translate_responses_to_messages(&body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert!(msgs[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("call_gf_0"));
    }

    #[test]
    fn translate_request_converts_unmatched_tool_result_after_tool_use_to_text() {
        let body = serde_json::to_vec(&json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_gf_0", "output": "orphan"},
            ],
        }))
        .unwrap();

        let out = translate_responses_to_messages(&body).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();

        assert_eq!(msgs[0]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[1]["content"][0]["tool_use_id"], "c1");
        assert_eq!(msgs[2]["content"][0]["type"], "text");
        assert!(msgs[2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("call_gf_0"));
    }

    #[test]
    fn translate_response_basic_text() {
        let body = serde_json::to_vec(&json!({
            "id": "msg_123",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello!"}],
            "model": "deepseek-v4-flash",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5},
        }))
        .unwrap();

        let out = translate_messages_response_to_gemini(&body, "gemini-3-flash-preview").unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["modelVersion"], "gemini-3-flash-preview");
        assert_eq!(v["candidates"][0]["finishReason"], "STOP");
        assert_eq!(v["candidates"][0]["content"]["parts"][0]["text"], "Hello!");
        assert_eq!(v["usageMetadata"]["promptTokenCount"], 10);
        assert_eq!(v["usageMetadata"]["candidatesTokenCount"], 5);
        assert_eq!(v["usageMetadata"]["totalTokenCount"], 15);
    }

    #[test]
    fn translate_response_with_tool_use() {
        let body = serde_json::to_vec(&json!({
            "id": "msg_456",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Let me check."},
                {"type": "tool_use", "id": "t1", "name": "search", "input": {"q": "rust"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "output_tokens": 15},
        }))
        .unwrap();

        let out = translate_messages_response_to_gemini(&body, "gemini-3-flash-preview").unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();

        let parts = v["candidates"][0]["content"]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "Let me check.");
        assert_eq!(parts[1]["functionCall"]["name"], "search");
        assert_eq!(parts[1]["functionCall"]["args"]["q"], "rust");
    }

    #[test]
    fn translate_response_rejects_non_message() {
        let body = br#"{"type":"error","error":{"type":"invalid","message":"bad"}}"#;
        assert!(translate_messages_response_to_gemini(body, "m").is_err());
    }

    #[test]
    fn streaming_text_delta_produces_gemini_chunk() {
        let sse = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":5}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

        let stream =
            futures::stream::once(async { Ok::<Bytes, io::Error>(Bytes::from_static(sse)) });
        let mut translated = Box::pin(translate_messages_sse_to_gemini(
            stream,
            "gemini-3-flash-preview".into(),
        ));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let chunks: Vec<Bytes> = rt.block_on(async {
            let mut out = Vec::new();
            while let Some(item) = translated.next().await {
                out.push(item.unwrap());
            }
            out
        });

        // Should have: text chunk "Hi", terminal chunk
        assert!(chunks.len() >= 2, "got {} chunks", chunks.len());

        let first = std::str::from_utf8(&chunks[0]).unwrap();
        assert!(first.contains("\"text\":\"Hi\""), "first chunk: {first}");

        let last = std::str::from_utf8(chunks.last().unwrap()).unwrap();
        assert!(last.contains("\"finishReason\""), "last chunk: {last}");
        assert!(last.contains("\"totalTokenCount\""));
    }
}
