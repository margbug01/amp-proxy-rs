//! OpenAI Responses SSE event rewriter.
//!
//! Ported from `internal/customproxy/sse_rewriter.go`. The augment upstream
//! emits a final `response.completed` event whose `response.output` array
//! is empty, even though the stream already delivered full
//! `response.output_item.done` events. Amp CLI's Stainless SDK reads
//! `response.output` from the completed event as the authoritative final
//! state, sees an empty array, and discards the streamed message.
//!
//! Fix: accumulate every `response.output_item.done` item, and when the
//! `response.completed` event arrives, inject the accumulated list into
//! `response.output` before forwarding to the client.

use std::io;
use std::pin::Pin;

use bytes::{Bytes, BytesMut};
use futures::Stream;
use futures::StreamExt;
use serde_json::Value;
use tracing::{error, warn};

/// Wraps a byte stream of OpenAI Responses SSE events and patches the final
/// `response.completed` event so the Stainless SDK sees the streamed items.
///
/// Returns a stream that yields the rewritten body chunks line-by-line so
/// downstream clients still see incremental deltas as they arrive.
pub fn rewrite_stream<S>(stream: S) -> Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>
where
    S: Stream<Item = Result<Bytes, io::Error>> + Send + Unpin + 'static,
{
    Box::pin(async_stream::try_stream! {
        let mut state = RewriterState::default();
        let mut s = stream;
        let mut buffer = BytesMut::new();
        while let Some(chunk) = s.next().await {
            let chunk = chunk?;
            buffer.extend_from_slice(&chunk);
            // Emit any complete lines (delimited by '\n').
            while let Some(idx) = buffer.iter().position(|&b| b == b'\n') {
                let raw_line = buffer.split_to(idx + 1); // includes '\n'
                // Strip the trailing newline (and possible CR) for parsing
                // but preserve the original framing in output.
                let len = raw_line.len();
                let body = &raw_line[..len - 1];
                let body = if body.last() == Some(&b'\r') {
                    &body[..body.len() - 1]
                } else {
                    body
                };
                let out = state.process_line(body);
                yield out;
            }
        }
        // Flush any trailing partial line (no newline at end).
        if !buffer.is_empty() {
            let body = if buffer.last() == Some(&b'\r') {
                &buffer[..buffer.len() - 1]
            } else {
                &buffer[..]
            };
            // For framing parity with the line path, append '\n' only if
            // the original ended with one. Trailing partial without '\n'
            // is forwarded verbatim.
            let processed = state.process_line_no_newline(body);
            yield processed;
        }
    })
}

#[derive(Default)]
struct RewriterState {
    items: Vec<Value>,
}

impl RewriterState {
    /// Processes a line that ended with '\n' in the upstream stream.
    /// Returns the rewritten line, including the trailing '\n'.
    fn process_line(&mut self, body: &[u8]) -> Bytes {
        let patched = self.transform(body);
        let mut out = BytesMut::with_capacity(patched.len() + 1);
        out.extend_from_slice(&patched);
        out.extend_from_slice(b"\n");
        out.freeze()
    }

    /// Processes a trailing partial line that did NOT end with '\n'.
    fn process_line_no_newline(&mut self, body: &[u8]) -> Bytes {
        self.transform(body)
    }

    fn transform(&mut self, line: &[u8]) -> Bytes {
        let payload = match line.strip_prefix(b"data: ") {
            Some(p) => p,
            None => return Bytes::copy_from_slice(line),
        };
        let parsed: Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(_) => return Bytes::copy_from_slice(line),
        };
        let event_type = parsed
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match event_type.as_str() {
            "response.output_item.done" => {
                if let Some(item) = parsed.get("item") {
                    self.items.push(item.clone());
                }
                Bytes::copy_from_slice(line)
            }
            "response.completed" => {
                let already_filled = parsed
                    .pointer("/response/output")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                if already_filled || self.items.is_empty() {
                    return Bytes::copy_from_slice(line);
                }
                let mut patched = parsed;
                if let Some(response) = patched.get_mut("response").and_then(|v| v.as_object_mut())
                {
                    response.insert(
                        "output".into(),
                        Value::Array(std::mem::take(&mut self.items)),
                    );
                } else {
                    warn!("customproxy: response.completed has no response object");
                    return Bytes::copy_from_slice(line);
                }
                let new_payload = match serde_json::to_vec(&patched) {
                    Ok(b) => b,
                    Err(e) => {
                        error!(error = %e, "customproxy: serialize patched response.completed");
                        return Bytes::copy_from_slice(line);
                    }
                };
                let mut out = BytesMut::with_capacity(new_payload.len() + 6);
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(&new_payload);
                out.freeze()
            }
            _ => Bytes::copy_from_slice(line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    async fn drive(fixture: &str) -> Vec<u8> {
        let chunks: Vec<Result<Bytes, io::Error>> =
            vec![Ok(Bytes::copy_from_slice(fixture.as_bytes()))];
        let s = stream::iter(chunks);
        let mut rw = rewrite_stream(s);
        let mut out: Vec<u8> = Vec::new();
        while let Some(chunk) = rw.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    fn extract_completed(out: &[u8]) -> Option<Value> {
        for line in out.split(|&b| b == b'\n') {
            if let Some(payload) = line.strip_prefix(b"data: ") {
                let v: Value = match serde_json::from_slice(payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v.get("type").and_then(|x| x.as_str()) == Some("response.completed") {
                    return Some(v);
                }
            }
        }
        None
    }

    #[tokio::test]
    async fn patches_empty_output_array() {
        let fixture = "event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"...\"}]},\"output_index\":0,\"sequence_number\":1}\n\
\n\
event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]},\"output_index\":1,\"sequence_number\":2}\n\
\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[]}}\n";
        let out = drive(fixture).await;
        let payload = extract_completed(&out).expect("completed event");
        let arr = payload["response"]["output"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "rs_1");
        assert_eq!(arr[1]["id"], "msg_1");
    }

    #[tokio::test]
    async fn idempotent_on_non_empty_output() {
        let fixture = "event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"msg_ignored\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ignored\"}]},\"output_index\":0,\"sequence_number\":1}\n\
\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"id\":\"msg_real\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"real\"}]}]}}\n";
        let out = drive(fixture).await;
        let payload = extract_completed(&out).expect("completed event");
        let arr = payload["response"]["output"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "msg_real");
    }

    #[tokio::test]
    async fn non_data_lines_pass_through() {
        let fixture = "event: ping\n\n: heartbeat\nevent: custom\ndata: {\"type\":\"noop\"}\n";
        let out = drive(fixture).await;
        assert_eq!(out, fixture.as_bytes());
    }

    #[tokio::test]
    async fn multiple_items_in_order() {
        let fixture = "event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\"},\"output_index\":0,\"sequence_number\":1}\n\
\n\
event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"first\"}]},\"output_index\":1,\"sequence_number\":2}\n\
\n\
event: response.output_item.done\n\
data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"second\"}]},\"output_index\":2,\"sequence_number\":3}\n\
\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[]}}\n";
        let out = drive(fixture).await;
        let payload = extract_completed(&out).expect("completed event");
        let arr = payload["response"]["output"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"], "rs_1");
        assert_eq!(arr[1]["id"], "msg_1");
        assert_eq!(arr[2]["id"], "msg_2");
    }
}
