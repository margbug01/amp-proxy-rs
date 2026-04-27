//! Anthropic Messages SSE -> single JSON body collapser.
//!
//! Ported from `internal/customproxy/sse_messages_collapser.go`. The augment
//! upstream silently drops content blocks on non-streaming `/v1/messages`
//! requests; the customproxy Director upgrades the request to streaming and
//! we collapse the SSE stream back into a single JSON body so the
//! downstream client (Amp CLI's librarian sub-agent) sees the shape it
//! expects.

use std::io;
use std::pin::Pin;

use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use serde_json::{json, Map, Value};

/// Cap on accumulated SSE bytes during collapse. Mirrors the Go ceiling
/// (4 MiB); librarian replies rarely exceed a few hundred KiB.
pub const MAX_MESSAGES_SSE_BYTES: usize = 4 * 1024 * 1024;

/// Reads an Anthropic Messages SSE stream from `body` and returns a single
/// JSON body shaped like the non-streaming `/v1/messages` response.
///
/// Handles: `message_start`, `content_block_start`, `content_block_delta`
/// (text_delta / input_json_delta / thinking_delta), `content_block_stop`,
/// `message_delta`, `message_stop`. Unknown events are skipped. An `error`
/// event aborts the collapse so the caller can fall back.
pub async fn collapse_to_json<S>(stream: S) -> Result<Bytes, io::Error>
where
    S: Stream<Item = Result<Bytes, io::Error>> + Unpin,
{
    let mut accumulated: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut s = stream;
    while let Some(chunk) = s.next().await {
        let chunk = chunk?;
        total = total.saturating_add(chunk.len());
        if total > MAX_MESSAGES_SSE_BYTES {
            // Truncate: append only up to limit, then stop reading.
            let take = MAX_MESSAGES_SSE_BYTES.saturating_sub(accumulated.len());
            accumulated.extend_from_slice(&chunk[..take.min(chunk.len())]);
            break;
        }
        accumulated.extend_from_slice(&chunk);
    }
    collapse_bytes(&accumulated)
}

/// Synchronous helper that collapses an already-buffered SSE byte slice.
/// Tests use this; production code should prefer [`collapse_to_json`].
pub fn collapse_bytes(raw: &[u8]) -> Result<Bytes, io::Error> {
    let mut envelope: Option<Map<String, Value>> = None;
    let mut content: Vec<Value> = Vec::new();

    let mut current_block: Option<Map<String, Value>> = None;
    let mut current_text = String::new();
    let mut current_partial_json = String::new();

    let finalize_block = |current_block: &mut Option<Map<String, Value>>,
                          current_text: &mut String,
                          current_partial_json: &mut String,
                          content: &mut Vec<Value>| {
        let Some(mut block) = current_block.take() else {
            return;
        };
        let kind = block
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match kind.as_str() {
            "text" => {
                block.insert("text".into(), Value::String(std::mem::take(current_text)));
            }
            "tool_use" => {
                let input: Value = if current_partial_json.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(current_partial_json).unwrap_or_else(|_| json!({}))
                };
                block.insert("input".into(), input);
                current_partial_json.clear();
            }
            "thinking" => {
                block.insert(
                    "thinking".into(),
                    Value::String(std::mem::take(current_text)),
                );
            }
            _ => {}
        }
        current_text.clear();
        content.push(Value::Object(block));
    };

    for line in raw.split(|&b| b == b'\n') {
        // Strip a trailing CR if upstream sent CRLF.
        let line = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        };
        let payload = match line.strip_prefix(b"data: ") {
            Some(p) => p,
            None => continue,
        };
        if payload.is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let event_type = parsed
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match event_type.as_str() {
            "message_start" => {
                if let Some(msg) = parsed.get("message") {
                    if let Some(obj) = msg.as_object() {
                        envelope = Some(obj.clone());
                    }
                }
            }
            "content_block_start" => {
                finalize_block(
                    &mut current_block,
                    &mut current_text,
                    &mut current_partial_json,
                    &mut content,
                );
                if let Some(block) = parsed.get("content_block").and_then(|v| v.as_object()) {
                    current_block = Some(block.clone());
                    current_text.clear();
                    current_partial_json.clear();
                }
            }
            "content_block_delta" => {
                if current_block.is_none() {
                    continue;
                }
                let delta_type = parsed
                    .pointer("/delta/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        if let Some(s) = parsed.pointer("/delta/text").and_then(|v| v.as_str()) {
                            current_text.push_str(s);
                        }
                    }
                    "input_json_delta" => {
                        if let Some(s) =
                            parsed.pointer("/delta/partial_json").and_then(|v| v.as_str())
                        {
                            current_partial_json.push_str(s);
                        }
                    }
                    "thinking_delta" => {
                        if let Some(s) = parsed.pointer("/delta/thinking").and_then(|v| v.as_str())
                        {
                            current_text.push_str(s);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                finalize_block(
                    &mut current_block,
                    &mut current_text,
                    &mut current_partial_json,
                    &mut content,
                );
            }
            "message_delta" => {
                let env = envelope.get_or_insert_with(Map::new);
                if let Some(reason) = parsed.pointer("/delta/stop_reason") {
                    if let Some(s) = reason.as_str() {
                        if !s.is_empty() {
                            env.insert("stop_reason".into(), Value::String(s.to_string()));
                        }
                    }
                }
                if let Some(seq) = parsed.pointer("/delta/stop_sequence") {
                    if seq.is_null() {
                        env.insert("stop_sequence".into(), Value::Null);
                    } else if let Some(s) = seq.as_str() {
                        env.insert("stop_sequence".into(), Value::String(s.to_string()));
                    }
                }
                if let Some(usage) = parsed.get("usage") {
                    if let Some(incoming) = usage.as_object() {
                        let base = env
                            .entry("usage".to_string())
                            .or_insert_with(|| Value::Object(Map::new()));
                        if let Some(base_obj) = base.as_object_mut() {
                            for (k, v) in incoming {
                                base_obj.insert(k.clone(), v.clone());
                            }
                        } else {
                            *base = Value::Object(incoming.clone());
                        }
                    }
                }
            }
            "message_stop" => {
                // Graceful end. Keep scanning so trailing events (if any)
                // are still consumed.
            }
            "ping" | "" => {
                // SSE keepalives and blank event markers.
            }
            "error" => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "upstream stream error: {}",
                        std::str::from_utf8(payload).unwrap_or("<non-utf8>")
                    ),
                ));
            }
            _ => {}
        }
    }

    // Flush any in-flight block in case the stream ended without an
    // explicit content_block_stop.
    finalize_block(
        &mut current_block,
        &mut current_text,
        &mut current_partial_json,
        &mut content,
    );

    let mut env = envelope.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "collapse_to_json: no message_start event seen",
        )
    })?;
    env.insert("content".into(), Value::Array(content));
    let bytes = serde_json::to_vec(&Value::Object(env))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    Ok(Bytes::from(bytes))
}

/// Wraps a byte stream into one that yields a single collapsed JSON body
/// once the upstream stream ends. Useful as a `reqwest::Response` body
/// adapter inside the customproxy ModifyResponse path.
pub fn collapse_stream<S>(
    stream: S,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + Unpin + 'static,
{
    Box::pin(async_stream::try_stream! {
        let collapsed = collapse_to_json(stream).await?;
        yield collapsed;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn collapse(fixture: &str) -> Result<Value, io::Error> {
        let bytes = collapse_bytes(fixture.as_bytes())?;
        Ok(serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn simple_text() {
        let fixture = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"resp_abc\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"gpt-5.4-mini\",\"stop_sequence\":null,\"usage\":{\"input_tokens\":0,\"output_tokens\":0},\"content\":[],\"stop_reason\":null}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":12,\"output_tokens\":32}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";
        let v = collapse(fixture).unwrap();
        assert_eq!(v["id"], "resp_abc");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"].as_array().unwrap().len(), 1);
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "Hi");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 12);
        assert_eq!(v["usage"]["output_tokens"], 32);
    }

    #[test]
    fn multiple_text_deltas_concatenate() {
        let fixture = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", \"}}\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\
data: {\"type\":\"message_stop\"}\n";
        let v = collapse(fixture).unwrap();
        assert_eq!(v["content"][0]["text"], "Hello, world");
    }

    #[test]
    fn tool_use() {
        let fixture = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"web_search\",\"input\":{}}}\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"objective\\\":\"}}\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"find AI projects\\\"}\"}}\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":10}}\n\
data: {\"type\":\"message_stop\"}\n";
        let v = collapse(fixture).unwrap();
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["name"], "web_search");
        assert_eq!(v["content"][0]["id"], "toolu_1");
        assert_eq!(v["content"][0]["input"]["objective"], "find AI projects");
        assert_eq!(v["stop_reason"], "tool_use");
    }

    #[test]
    fn empty_content() {
        // message_start, no blocks, message_stop. The collapser must still
        // produce a well-formed envelope with content:[].
        let fixture = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m_empty\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":0}}\n\
data: {\"type\":\"message_stop\"}\n";
        let v = collapse(fixture).unwrap();
        assert_eq!(v["id"], "m_empty");
        assert_eq!(v["content"].as_array().unwrap().len(), 0);
        assert_eq!(v["stop_reason"], "end_turn");
    }

    #[test]
    fn error_event_aborts() {
        let fixture = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m4\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded\",\"message\":\"try again\"}}\n";
        let err = collapse(fixture).unwrap_err();
        assert!(err.to_string().contains("upstream stream error"));
    }

    #[test]
    fn no_message_start_rejected() {
        let fixture = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n";
        let err = collapse(fixture).unwrap_err();
        assert!(err.to_string().contains("no message_start"));
    }
}
