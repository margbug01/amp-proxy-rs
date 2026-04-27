//! Response post-processing helpers.
//!
//! Ported (selectively) from `internal/amp/response_rewriter.go`. The Go
//! implementation also wraps the gin ResponseWriter to support buffered +
//! streaming model-name rewriting. The Rust port keeps the *pure* helpers —
//! the JSON/SSE rewriting itself — and leaves the writer plumbing for the
//! routes layer that actually drives the upstream response.
//!
//! Public surface:
//!   * `rewrite_model_in_json(body, original_model)` — rewrite model fields
//!     in a non-streaming JSON envelope and inject empty signature fields
//!     so Amp CLI's TUI doesn't crash on `P.signature.length`.
//!   * `rewrite_sse_chunk(chunk, original_model)` — same but per-event for
//!     streaming Anthropic Messages / OpenAI Responses replies.
//!   * `sanitize_amp_request_body(body)` — strip thinking blocks with
//!     missing/invalid signatures from outgoing assistant messages.

use serde_json::{Map, Value};

const MODEL_FIELD_PATHS: &[&[&str]] = &[
    &["message", "model"],
    &["model"],
    &["modelVersion"],
    &["response", "model"],
    &["response", "modelVersion"],
];

/// Rewrite the upstream JSON body so the `model` field reflects the original
/// (pre-mapping) model name and so any `tool_use` / `thinking` content blocks
/// carry an empty `signature` field. Returns the rewritten bytes; on parse
/// failure returns the input unchanged.
pub fn rewrite_model_in_json(body: &[u8], original_model: &str) -> Vec<u8> {
    if body.is_empty() {
        return Vec::new();
    }
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };
    ensure_amp_signature(&mut v);
    if !original_model.is_empty() {
        rewrite_model_paths(&mut v, original_model);
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

/// Rewrite a single SSE chunk. Each `data: {...}\n` line is parsed as JSON,
/// rewritten via [`rewrite_model_in_json_value`], and re-serialised. Lines
/// that don't carry JSON (event names, blanks, comments) pass through.
pub fn rewrite_sse_chunk(chunk: &[u8], original_model: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(chunk.len());
    let mut first = true;
    for line in chunk.split(|&b| b == b'\n') {
        if !first {
            out.push(b'\n');
        }
        first = false;
        if let Some(rest) = line.strip_prefix(b"data: ") {
            if rest.starts_with(b"{") {
                if let Ok(mut v) = serde_json::from_slice::<Value>(rest) {
                    ensure_amp_signature(&mut v);
                    if !original_model.is_empty() {
                        rewrite_model_paths(&mut v, original_model);
                    }
                    out.extend_from_slice(b"data: ");
                    if let Ok(bytes) = serde_json::to_vec(&v) {
                        out.extend_from_slice(&bytes);
                    } else {
                        out.extend_from_slice(line);
                    }
                    continue;
                }
            }
        }
        out.extend_from_slice(line);
    }
    out
}

fn rewrite_model_paths(v: &mut Value, original_model: &str) {
    for path in MODEL_FIELD_PATHS {
        if let Some(node) = pointer_mut(v, path) {
            if node.is_string() {
                *node = Value::String(original_model.to_string());
            }
        }
    }
}

fn pointer_mut<'a>(v: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    let mut cur = v;
    for seg in path {
        cur = cur.as_object_mut()?.get_mut(*seg)?;
    }
    Some(cur)
}

/// Inject an empty `signature` field into any `tool_use` / `thinking` block
/// that lacks one. Mirrors Go `ensureAmpSignature`.
fn ensure_amp_signature(v: &mut Value) {
    if let Some(content) = v.get_mut("content").and_then(|c| c.as_array_mut()) {
        for block in content.iter_mut() {
            if let Some(obj) = block.as_object_mut() {
                let kind = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if (kind == "tool_use" || kind == "thinking") && !obj.contains_key("signature") {
                    obj.insert("signature".into(), Value::String(String::new()));
                }
            }
        }
    }
    // Streaming variant: top-level `content_block` object.
    if let Some(cb) = v.get_mut("content_block").and_then(|cb| cb.as_object_mut()) {
        let kind = cb.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if (kind == "tool_use" || kind == "thinking") && !cb.contains_key("signature") {
            cb.insert("signature".into(), Value::String(String::new()));
        }
    }
}

/// Strip thinking blocks with missing/empty signatures from assistant
/// messages, and remove the proxy-injected `signature` field from
/// `tool_use` blocks. Mirrors Go's `SanitizeAmpRequestBody`.
pub fn sanitize_amp_request_body(body: &[u8]) -> Vec<u8> {
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };

    let messages = match v.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return body.to_vec(),
    };

    for msg in messages.iter_mut() {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        if role != "assistant" {
            continue;
        }
        let content = match msg.get_mut("content").and_then(|c| c.as_array_mut()) {
            Some(c) => c,
            None => continue,
        };
        let mut kept: Vec<Value> = Vec::with_capacity(content.len());
        for block in content.drain(..) {
            let mut block = block;
            let kind = block
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if kind == "thinking" {
                let sig_ok = block
                    .get("signature")
                    .and_then(|s| s.as_str())
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                if !sig_ok {
                    continue;
                }
            } else if kind == "tool_use" {
                if let Some(obj) = block.as_object_mut() {
                    obj.remove("signature");
                }
            }
            kept.push(block);
        }
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("content".into(), Value::Array(kept));
        }
    }

    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

/// Filter a comma-separated `Anthropic-Beta` header, removing one feature.
pub fn filter_beta_features(header: &str, feature_to_remove: &str) -> String {
    header
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != feature_to_remove)
        .collect::<Vec<_>>()
        .join(",")
}

#[allow(dead_code)]
fn _ensure_object<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    map.entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("just inserted object")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rewrites_model_field() {
        let body = br#"{"model":"gpt-5.4-mini","content":[]}"#;
        let out = rewrite_model_in_json(body, "claude-opus-4.6");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "claude-opus-4.6");
    }

    #[test]
    fn injects_empty_signature() {
        let body = json!({
            "model": "x",
            "content": [
                {"type": "tool_use", "id": "t1", "name": "search", "input": {}},
                {"type": "thinking", "thinking": "..."}
            ]
        })
        .to_string();
        let out = rewrite_model_in_json(body.as_bytes(), "");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["content"][0]["signature"], "");
        assert_eq!(v["content"][1]["signature"], "");
    }

    #[test]
    fn sse_chunk_rewrites_data_lines() {
        let chunk = b"event: content_block_delta\n\
data: {\"type\":\"message_start\",\"message\":{\"model\":\"x\"}}\n\
\n";
        let out = rewrite_sse_chunk(chunk, "claude");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"model\":\"claude\""));
        assert!(s.starts_with("event: content_block_delta\n"));
    }

    #[test]
    fn sanitize_drops_unsigned_thinking() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "..."},
                        {"type": "text", "text": "ok"}
                    ]
                }
            ]
        })
        .to_string();
        let out = sanitize_amp_request_body(body.as_bytes());
        let v: Value = serde_json::from_slice(&out).unwrap();
        let blocks = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn sanitize_strips_signature_from_tool_use() {
        let body = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "t", "signature": "fake"}
                    ]
                }
            ]
        })
        .to_string();
        let out = sanitize_amp_request_body(body.as_bytes());
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v["messages"][0]["content"][0].get("signature").is_none());
    }

    #[test]
    fn filter_beta_features_removes_target() {
        let h = "feature-a, context-1m-2025-08-07,feature-b";
        assert_eq!(
            filter_beta_features(h, "context-1m-2025-08-07"),
            "feature-a,feature-b"
        );
        assert_eq!(filter_beta_features("only-one", "only-one"), "");
    }
}
