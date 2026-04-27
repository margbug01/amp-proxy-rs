//! OpenAI Responses ↔ chat/completions request/response translator.
//!
//! Ported from `internal/customproxy/responses_translator.go`. Used for
//! providers (DeepSeek, etc.) that only implement chat/completions, so the
//! Amp CLI's Responses API requests must be rewritten on the way out and
//! the chat/completions reply rewritten on the way back.
//!
//! Field-mapping summary (see Go file for the full doc-comment):
//!   * `model`, `parallel_tool_calls` pass through
//!   * `max_output_tokens` → `max_tokens`
//!   * `stream` defaults to `true` if absent (Amp deep mode)
//!   * Responses-only fields dropped: `include`, `store`, `stream_options`,
//!     `prompt_cache_key`
//!   * `input` array (system/message/reasoning/function_call/
//!     function_call_output) → flat `messages` array with role mapping
//!   * `tools` Responses-shape → chat-shape (`{type:"function", function:{…}}`)
//!   * `reasoning.effort` → top-level `reasoning_effort`

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

/// Per-request translator state carried from the request phase to the
/// response phase. The request phase produces it; the streaming or
/// non-streaming response translator consumes it.
#[derive(Debug, Clone, Default)]
pub struct ResponsesTranslateCtx {
    /// Model the client (Amp CLI) asked for. Echoed back in
    /// `response.model` so logs stay coherent.
    pub orig_model: String,
    /// Whether the client requested streaming. Mirrors `stream` from the
    /// incoming Responses request.
    pub stream: bool,
    /// Amp CLI's thread-scoped idempotency hint. Echoed back in
    /// `prompt_cache_key`; never forwarded upstream.
    pub prompt_cache_key: String,
}

/// Rewrites an OpenAI Responses API request body into an OpenAI
/// chat/completions request body. Returns the new body and a context
/// holding fields the response phase will need.
pub fn translate_responses_request_to_chat(
    body: &[u8],
) -> Result<(Vec<u8>, ResponsesTranslateCtx)> {
    let req: Value = serde_json::from_slice(body).context("parse responses request")?;
    let req_obj = req
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("responses request must be a JSON object"))?;

    let mut out = Map::new();

    if let Some(m) = req_obj.get("model").and_then(|v| v.as_str()) {
        out.insert("model".into(), Value::String(m.into()));
    }

    // Amp deep mode normally sets stream:true; default true to keep chat
    // upstreams streaming so the reply can be translated back to SSE.
    let stream = req_obj
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    out.insert("stream".into(), Value::Bool(stream));

    if let Some(v) = req_obj.get("max_output_tokens") {
        out.insert("max_tokens".into(), v.clone());
    }
    if let Some(v) = req_obj.get("parallel_tool_calls") {
        if v.is_boolean() {
            out.insert("parallel_tool_calls".into(), v.clone());
        }
    }

    // reasoning.effort → reasoning_effort + thinking:{type:"enabled"}.
    let mut thinking_enabled = false;
    if let Some(r) = req_obj.get("reasoning").and_then(|v| v.as_object()) {
        if let Some(eff) = r.get("effort").and_then(|v| v.as_str()) {
            if !eff.is_empty() {
                out.insert("reasoning_effort".into(), Value::String(eff.into()));
                out.insert("thinking".into(), json!({"type": "enabled"}));
                thinking_enabled = true;
            }
        }
    }

    // tools: unwrap Responses-style flat tool into chat/completions shape.
    if let Some(raw_tools) = req_obj.get("tools").and_then(|v| v.as_array()) {
        let mut chat_tools: Vec<Value> = Vec::with_capacity(raw_tools.len());
        for rt in raw_tools {
            let Some(t) = rt.as_object() else { continue };
            let ttype = t.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ttype != "function" {
                continue;
            }
            let mut fn_obj = Map::new();
            if let Some(name) = t.get("name").and_then(|v| v.as_str()) {
                if !name.is_empty() {
                    fn_obj.insert("name".into(), Value::String(name.into()));
                }
            }
            if let Some(desc) = t.get("description").and_then(|v| v.as_str()) {
                if !desc.is_empty() {
                    fn_obj.insert("description".into(), Value::String(desc.into()));
                }
            }
            if let Some(params) = t.get("parameters") {
                fn_obj.insert("parameters".into(), params.clone());
            }
            let mut chat_tool = Map::new();
            chat_tool.insert("type".into(), Value::String("function".into()));
            chat_tool.insert("function".into(), Value::Object(fn_obj));
            if let Some(strict) = t.get("strict").and_then(|v| v.as_bool()) {
                chat_tool.insert("strict".into(), Value::Bool(strict));
            }
            chat_tools.push(Value::Object(chat_tool));
        }
        if !chat_tools.is_empty() {
            out.insert("tools".into(), Value::Array(chat_tools));
        }
    }

    if let Some(tc) = req_obj.get("tool_choice") {
        out.insert("tool_choice".into(), tc.clone());
    }

    // input → messages (the bulk of the translation).
    let empty: Vec<Value> = Vec::new();
    let input = req_obj
        .get("input")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let messages = translate_input_to_messages(input, thinking_enabled);
    out.insert("messages".into(), Value::Array(messages));

    let orig_model = req_obj
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let prompt_cache_key = req_obj
        .get("prompt_cache_key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let ctx = ResponsesTranslateCtx {
        orig_model,
        stream,
        prompt_cache_key,
    };

    let new_body = serde_json::to_vec(&Value::Object(out)).context("marshal chat request")?;
    Ok((new_body, ctx))
}

/// Walks the Responses `input` array and produces an OpenAI chat/completions
/// `messages` array. See the Go doc-comment for full merging rules.
fn translate_input_to_messages(input: &[Value], thinking_enabled: bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(input.len());

    // pendingAssistant buffers a not-yet-emitted assistant message so that
    // subsequent function_call items can attach to it.
    let mut pending_assistant: Option<Map<String, Value>> = None;
    let mut pending_reasoning: String = String::new();

    let flush = |out: &mut Vec<Value>, pa: &mut Option<Map<String, Value>>| {
        if let Some(asst) = pa.take() {
            out.push(Value::Object(asst));
        }
    };

    for raw in input {
        let Some(item) = raw.as_object() else { continue };
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("");

        // Helper that materializes pending assistant on demand. We can't
        // close over mutable state in Rust the way Go does, so we open it
        // locally on each branch below.
        if item_type.is_empty() && role == "system" {
            flush(&mut out, &mut pending_assistant);
            let content = item
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.push(json!({"role": "system", "content": content}));
            continue;
        }

        if item_type == "message" {
            let text = extract_message_text(item);
            match role {
                "user" => {
                    flush(&mut out, &mut pending_assistant);
                    out.push(json!({"role": "user", "content": text}));
                }
                "assistant" => {
                    flush(&mut out, &mut pending_assistant);
                    let mut asst = Map::new();
                    asst.insert("role".into(), Value::String("assistant".into()));
                    if !pending_reasoning.is_empty() {
                        asst.insert(
                            "reasoning_content".into(),
                            Value::String(std::mem::take(&mut pending_reasoning)),
                        );
                    } else if thinking_enabled {
                        asst.insert("reasoning_content".into(), Value::String(String::new()));
                    }
                    if !text.is_empty() {
                        asst.insert("content".into(), Value::String(text));
                    }
                    pending_assistant = Some(asst);
                }
                "system" => {
                    flush(&mut out, &mut pending_assistant);
                    out.push(json!({"role": "system", "content": text}));
                }
                _ => {}
            }
            continue;
        }

        if item_type == "function_call" {
            // Open assistant if needed.
            if pending_assistant.is_none() {
                let mut asst = Map::new();
                asst.insert("role".into(), Value::String("assistant".into()));
                if !pending_reasoning.is_empty() {
                    asst.insert(
                        "reasoning_content".into(),
                        Value::String(std::mem::take(&mut pending_reasoning)),
                    );
                } else if thinking_enabled {
                    asst.insert("reasoning_content".into(), Value::String(String::new()));
                }
                pending_assistant = Some(asst);
            }
            let asst = pending_assistant.as_mut().expect("just opened");
            // OpenAI: assistant with tool_calls and no content must use null.
            if !asst.contains_key("content") {
                asst.insert("content".into(), Value::Null);
            }
            let mut tcs = match asst.get("tool_calls") {
                Some(Value::Array(arr)) => arr.clone(),
                _ => Vec::new(),
            };
            let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
            tcs.push(json!({
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": args},
            }));
            asst.insert("tool_calls".into(), Value::Array(tcs));
            continue;
        }

        if item_type == "function_call_output" {
            flush(&mut out, &mut pending_assistant);
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output = item
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output,
            }));
            continue;
        }

        if item_type == "reasoning" {
            let r = extract_reasoning_text(item);
            if !r.is_empty() {
                pending_reasoning = r;
            }
            continue;
        }

        // Unknown type: drop defensively.
    }

    flush(&mut out, &mut pending_assistant);
    out
}

/// Collapses a Responses message `content` array into a single string.
fn extract_message_text(item: &Map<String, Value>) -> String {
    if let Some(s) = item.get("content").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    let Some(parts) = item.get("content").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut buf = String::new();
    for rp in parts {
        let Some(p) = rp.as_object() else { continue };
        let t = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(t, "input_text" | "output_text" | "text") {
            if let Some(s) = p.get("text").and_then(|v| v.as_str()) {
                buf.push_str(s);
            }
        }
    }
    buf
}

/// Extracts plaintext reasoning from a reasoning input item, preferring
/// `encrypted_content` unless it looks like an opaque GPT-5.x token,
/// falling back to `reasoning_content` and finally to the `summary` array.
fn extract_reasoning_text(item: &Map<String, Value>) -> String {
    if let Some(s) = item.get("encrypted_content").and_then(|v| v.as_str()) {
        if !s.is_empty() && !s.starts_with("gAAAAAB") {
            return s.to_string();
        }
    }
    if let Some(s) = item.get("reasoning_content").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    let Some(summary) = item.get("summary").and_then(|v| v.as_array()) else {
        return String::new();
    };
    let mut buf = String::new();
    for raw in summary {
        let Some(part) = raw.as_object() else { continue };
        if let Some(s) = part.get("text").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                buf.push_str(s);
                continue;
            }
        }
        if let Some(s) = part.get("summary_text").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                buf.push_str(s);
            }
        }
    }
    buf
}

/// Rewrites a non-streaming chat/completions JSON response into a
/// non-streaming Responses JSON response. Returns `(body, true)` on
/// success. Returns `(_, false)` for bodies that do not look like a
/// chat completion (e.g. upstream error payloads); callers pass those
/// through unchanged.
pub fn translate_chat_completion_to_responses(
    body: &[u8],
    ctx: &ResponsesTranslateCtx,
) -> Result<(Vec<u8>, bool)> {
    let chat: Value = serde_json::from_slice(body).context("parse chat response")?;
    let chat_obj = match chat.as_object() {
        Some(o) => o,
        None => return Ok((Vec::new(), false)),
    };
    let Some(choices) = chat_obj.get("choices").and_then(|v| v.as_array()) else {
        return Ok((Vec::new(), false));
    };
    if choices.is_empty() {
        return Ok((Vec::new(), false));
    }
    let Some(choice) = choices[0].as_object() else {
        return Ok((Vec::new(), false));
    };
    let Some(message) = choice.get("message").and_then(|v| v.as_object()) else {
        return Ok((Vec::new(), false));
    };

    let mut output: Vec<Value> = Vec::with_capacity(3);
    let reasoning = message
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !reasoning.is_empty() {
        output.push(json!({
            "id": synth_item_id("rs"),
            "type": "reasoning",
            "status": "completed",
            "encrypted_content": reasoning,
            "summary": [{"type": "summary_text", "text": reasoning}],
        }));
    }
    let content = chat_message_content(message.get("content"));
    if !content.is_empty() {
        output.push(json!({
            "id": synth_item_id("msg"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "annotations": [],
                "logprobs": [],
                "text": content,
            }],
        }));
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for raw in tool_calls {
            let Some(tc) = raw.as_object() else { continue };
            let fn_obj = tc.get("function").and_then(|v| v.as_object());
            output.push(json!({
                "id": synth_item_id("fc"),
                "type": "function_call",
                "status": "completed",
                "arguments": fn_obj.and_then(|f| f.get("arguments")).and_then(|v| v.as_str()).unwrap_or(""),
                "call_id": tc.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "name": fn_obj.and_then(|f| f.get("name")).and_then(|v| v.as_str()).unwrap_or(""),
            }));
        }
    }

    let model = if !ctx.orig_model.is_empty() {
        ctx.orig_model.clone()
    } else {
        chat_obj
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mut created_at = chat_obj
        .get("created")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if created_at == 0 {
        created_at = chrono::Utc::now().timestamp();
    }

    let mut resp = Map::new();
    resp.insert("id".into(), Value::String(synth_response_id()));
    resp.insert("object".into(), Value::String("response".into()));
    resp.insert("created_at".into(), Value::from(created_at));
    resp.insert("status".into(), Value::String("completed".into()));
    resp.insert("background".into(), Value::Bool(false));
    resp.insert("error".into(), Value::Null);
    resp.insert("incomplete_details".into(), Value::Null);
    resp.insert("instructions".into(), Value::Null);
    resp.insert("max_output_tokens".into(), Value::Null);
    resp.insert("max_tool_calls".into(), Value::Null);
    resp.insert("model".into(), Value::String(model));
    resp.insert("output".into(), Value::Array(output));
    resp.insert("parallel_tool_calls".into(), Value::Bool(true));
    resp.insert("previous_response_id".into(), Value::Null);
    resp.insert(
        "reasoning".into(),
        json!({"effort": "auto", "summary": "auto"}),
    );
    resp.insert("store".into(), Value::Bool(false));
    resp.insert("temperature".into(), Value::from(1.0));
    resp.insert("top_p".into(), Value::from(1.0));
    resp.insert("usage".into(), translate_chat_usage(chat_obj.get("usage")));
    resp.insert("completed_at".into(), Value::from(chrono::Utc::now().timestamp()));
    if !ctx.prompt_cache_key.is_empty() {
        resp.insert(
            "prompt_cache_key".into(),
            Value::String(ctx.prompt_cache_key.clone()),
        );
    }

    let out = serde_json::to_vec(&Value::Object(resp)).context("marshal responses response")?;
    Ok((out, true))
}

/// Tolerates both string-valued and array-of-parts content shapes. Returns
/// the concatenated text.
fn chat_message_content(raw: Option<&Value>) -> String {
    match raw {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut buf = String::new();
            for part in arr {
                let Some(p) = part.as_object() else { continue };
                if let Some(s) = p.get("text").and_then(|v| v.as_str()) {
                    buf.push_str(s);
                }
            }
            buf
        }
        _ => String::new(),
    }
}

/// Re-keys a chat/completions usage object into Responses-shape usage.
pub(crate) fn translate_chat_usage(raw: Option<&Value>) -> Value {
    let Some(usage) = raw.and_then(|v| v.as_object()) else {
        return Value::Null;
    };
    let mut out = Map::new();
    let copy = |out: &mut Map<String, Value>, dst: &str, src: &str| {
        if let Some(v) = usage.get(src) {
            out.insert(dst.into(), v.clone());
        }
    };
    copy(&mut out, "input_tokens", "prompt_tokens");
    copy(&mut out, "output_tokens", "completion_tokens");
    copy(&mut out, "total_tokens", "total_tokens");
    copy(&mut out, "input_tokens_details", "prompt_tokens_details");
    copy(&mut out, "output_tokens_details", "completion_tokens_details");
    if out.is_empty() {
        return Value::Object(usage.clone());
    }
    Value::Object(out)
}

/// Returns a "resp_<hex>" id that mirrors OpenAI's opaque correlator shape.
pub(crate) fn synth_response_id() -> String {
    format!("resp_{}", rand_hex(24))
}

/// Returns a "<prefix>_<hex>" id for output items.
pub(crate) fn synth_item_id(prefix: &str) -> String {
    format!("{}_{}", prefix, rand_hex(24))
}

fn rand_hex(n_bytes: usize) -> String {
    // uuid::Uuid::new_v4() is 16 bytes of high-quality randomness; we just
    // need an opaque correlator, so concatenate enough uuid hex to cover.
    let mut buf = String::with_capacity(n_bytes * 2);
    while buf.len() < n_bytes * 2 {
        let u = uuid::Uuid::new_v4().simple().to_string();
        buf.push_str(&u);
    }
    buf.truncate(n_bytes * 2);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(body: Value) -> (Value, ResponsesTranslateCtx) {
        let bytes = serde_json::to_vec(&body).unwrap();
        let (out, ctx) = translate_responses_request_to_chat(&bytes).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        (v, ctx)
    }

    #[test]
    fn basic_field_mapping_max_output_tokens_to_max_tokens() {
        let (out, ctx) = translate(json!({
            "model": "gpt-5.4",
            "stream": true,
            "input": [
                {"role": "system", "content": "You are amp."},
                {"type": "message", "role": "user",
                 "content": [
                     {"type": "input_text", "text": "hi "},
                     {"type": "input_text", "text": "there"}
                 ]},
            ],
            "reasoning": {"effort": "high", "summary": "auto"},
            "max_output_tokens": 1024,
        }));
        assert_eq!(out["model"], "gpt-5.4");
        assert_eq!(out["stream"], true);
        assert_eq!(out["max_tokens"], 1024);
        assert_eq!(out["reasoning_effort"], "high");
        assert_eq!(out["thinking"]["type"], "enabled");
        // Responses-only reasoning.summary must be dropped.
        assert!(out.get("reasoning").is_none());

        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are amp.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi there", "input_text parts joined");
        assert_eq!(ctx.orig_model, "gpt-5.4");
        assert!(ctx.stream);
    }

    #[test]
    fn tool_translation_unwraps_to_chat_shape() {
        let (out, _) = translate(json!({
            "model": "gpt-5.4",
            "input": [{"role": "system", "content": "sys"}],
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "description": "Run a shell command.",
                "parameters": {"type": "object"},
                "strict": false,
            }],
        }));
        let ts = out["tools"].as_array().unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0]["type"], "function");
        assert_eq!(ts[0]["function"]["name"], "shell_command");
        assert_eq!(ts[0]["function"]["description"], "Run a shell command.");
        assert!(ts[0]["function"]["parameters"]["type"].is_string());
        // Top-level name must be removed (moved into function).
        assert!(ts[0].get("name").is_none());
    }

    #[test]
    fn function_call_output_becomes_role_tool_message() {
        let (out, _) = translate(json!({
            "model": "gpt-5.4",
            "input": [
                {"role": "system", "content": "sys"},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "do stuff"}]},
                {"type": "reasoning", "id": "rs_x", "encrypted_content": "blob"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "ok working"}]},
                {"type": "function_call", "name": "shell_command",
                 "call_id": "call_1", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call", "name": "read_file",
                 "call_id": "call_2", "arguments": "{\"path\":\"README\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a\nb"},
                {"type": "function_call_output", "call_id": "call_2",
                 "output": "readme contents"},
            ],
        }));
        let msgs = out["messages"].as_array().unwrap();
        // system, user, assistant(text + 2 tool_calls), tool, tool
        assert_eq!(msgs.len(), 5, "msgs={:#}", out["messages"]);
        let asst = &msgs[2];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["content"], "ok working");
        assert_eq!(asst["reasoning_content"], "blob");
        let tcs = asst["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0]["id"], "call_1");
        assert_eq!(tcs[0]["function"]["name"], "shell_command");
        assert_eq!(tcs[1]["id"], "call_2");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[3]["content"], "a\nb");
        assert_eq!(msgs[4]["role"], "tool");
        assert_eq!(msgs[4]["tool_call_id"], "call_2");
    }

    #[test]
    fn function_calls_without_prior_assistant_text_emit_null_content() {
        let (out, _) = translate(json!({
            "model": "gpt-5.4",
            "input": [
                {"role": "system", "content": "sys"},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "run ls"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "need tool"}]},
                {"type": "function_call", "name": "shell_command",
                 "call_id": "call_z", "arguments": "{\"cmd\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_z", "output": "files"},
            ],
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4);
        let asst = &msgs[2];
        assert_eq!(asst["role"], "assistant");
        assert!(asst["content"].is_null(), "content should be null");
        assert_eq!(asst["reasoning_content"], "need tool");
        assert_eq!(asst["tool_calls"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn responses_only_fields_dropped_and_ctx_carries_prompt_cache_key() {
        let (out, ctx) = translate(json!({
            "model": "gpt-5.4",
            "input": [{"role": "system", "content": "sys"}],
            "include": ["reasoning.encrypted_content"],
            "store": false,
            "stream_options": {"include_obfuscation": false},
            "prompt_cache_key": "T-abc",
        }));
        for k in ["include", "store", "stream_options", "prompt_cache_key"] {
            assert!(out.get(k).is_none(), "{k} should be dropped");
        }
        assert_eq!(ctx.prompt_cache_key, "T-abc");
    }

    #[test]
    fn chat_completion_translates_back_to_responses() {
        let body = br#"{"id":"chatcmpl_1","object":"chat.completion","created":123,"model":"deepseek-v4-pro","choices":[{"message":{"role":"assistant","reasoning_content":"think","content":"Hello world","tool_calls":[{"id":"call_1","type":"function","function":{"name":"shell_command","arguments":"{\"cmd\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let ctx = ResponsesTranslateCtx {
            orig_model: "gpt-5.4".into(),
            stream: false,
            prompt_cache_key: "T-abc".into(),
        };
        let (out, ok) = translate_chat_completion_to_responses(body, &ctx).unwrap();
        assert!(ok);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-5.4");
        assert_eq!(v["prompt_cache_key"], "T-abc");
        let items = v["output"].as_array().unwrap();
        assert_eq!(items.len(), 3, "reasoning + msg + tool");
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[0]["summary"][0]["text"], "think");
        assert_eq!(items[1]["content"][0]["text"], "Hello world");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["output_tokens"], 5);
    }

    #[test]
    fn chat_completion_passes_through_error_shape() {
        let (_, ok) = translate_chat_completion_to_responses(
            br#"{"error":{"message":"bad"}}"#,
            &ResponsesTranslateCtx::default(),
        )
        .unwrap();
        assert!(!ok, "error payload must not translate");
    }
}
