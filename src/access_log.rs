//! Optional access-log middleware with body model peek.
//!
//! Ported from `internal/server/access_log.go` (Gin). Emits a single INFO
//! `tracing` event per request with `method`, `path`, `status`, `elapsed_ms`.
//! When `DebugConfig.access_log_model_peek` is enabled and the request looks
//! like a JSON POST/PUT, also peeks `model` and `stream` from the body and
//! attaches them as fields. The peek is capped at 256 KiB so an oversized
//! upload can't hold the request in memory.
//!
//! The middleware is intended to coexist with `amp::routes::handle`'s own
//! `amp router: request` / `amp router: response` logs. To avoid confusion in
//! a one-line scan of run.log this middleware uses the message `"request log"`
//! (mirroring the Go version).

use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures::{stream, StreamExt};
use tracing::info;

/// Maximum body size to buffer for the model/stream peek. Larger bodies are
/// forwarded untouched and skip the peek.
const PEEK_LIMIT: usize = 256 * 1024;

/// Axum middleware function — wire via
/// `axum::middleware::from_fn_with_state(cfg, access_log_layer)`.
pub async fn access_log_layer(
    State(cfg): State<crate::config::DebugConfig>,
    req: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Decide whether we want to peek before consuming the body, so the
    // common path stays zero-copy.
    let want_peek = cfg.access_log_model_peek
        && (method == Method::POST || method == Method::PUT)
        && is_json_content_type(req.headers());

    let (req, peeked) = if want_peek {
        peek_model_and_stream(req).await
    } else {
        (req, None)
    };

    let response = next.run(req).await;
    let status = response.status().as_u16();
    let elapsed_ms = started.elapsed().as_millis() as u64;

    match peeked {
        Some((model, stream)) => {
            info!(
                method = %method,
                path = %path,
                status = status,
                elapsed_ms = elapsed_ms,
                model = %model.as_deref().unwrap_or("-"),
                stream = stream.map(|b| b.to_string()).as_deref().unwrap_or("-"),
                "request log"
            );
        }
        None => {
            info!(
                method = %method,
                path = %path,
                status = status,
                elapsed_ms = elapsed_ms,
                "request log"
            );
        }
    }

    response
}

/// Returns `true` when the request's `Content-Type` starts with
/// `application/json` (case-insensitive). Used to gate the peek.
fn is_json_content_type(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let lower = s.trim().to_ascii_lowercase();
            lower.starts_with("application/json") || lower.contains("+json")
        })
        .unwrap_or(false)
}

/// Read at most `PEEK_LIMIT` bytes from the front of a request body and
/// rebuild a body that yields those bytes followed by the unread tail.
///
/// `Ok((body, Some(prefix)))` means the original body ended within the limit,
/// so `prefix` contains the complete body and may be parsed. `Ok((body, None))`
/// means the body exceeded the limit; it is still reconstructed losslessly, but
/// callers should skip parsing because only a prefix was inspected.
async fn peek_body_prefix(body: Body) -> Result<(Body, Option<Bytes>), axum::Error> {
    let mut body_stream = body.into_data_stream();
    let mut prefix = BytesMut::new();

    while prefix.len() < PEEK_LIMIT {
        let Some(chunk) = body_stream.next().await else {
            let bytes = prefix.freeze();
            return Ok((Body::from(bytes.clone()), Some(bytes)));
        };
        let chunk = chunk?;
        if prefix.len() + chunk.len() <= PEEK_LIMIT {
            prefix.extend_from_slice(&chunk);
        } else {
            let prefix_len = PEEK_LIMIT - prefix.len();
            prefix.extend_from_slice(&chunk[..prefix_len]);
            let rest = chunk.slice(prefix_len..);
            let rebuilt = stream::once(async move { Ok::<_, axum::Error>(prefix.freeze()) })
                .chain(stream::once(async move { Ok::<_, axum::Error>(rest) }))
                .chain(body_stream);
            return Ok((Body::from_stream(rebuilt), None));
        }
    }

    let Some(chunk) = body_stream.next().await else {
        let bytes = prefix.freeze();
        return Ok((Body::from(bytes.clone()), Some(bytes)));
    };
    let chunk = chunk?;
    let rebuilt = stream::once(async move { Ok::<_, axum::Error>(prefix.freeze()) })
        .chain(stream::once(async move { Ok::<_, axum::Error>(chunk) }))
        .chain(body_stream);
    Ok((Body::from_stream(rebuilt), None))
}

/// Buffer the request body up to `PEEK_LIMIT` bytes, parse it as JSON, and
/// extract `model` / `stream`. Always returns a fully-reconstructed request
/// so the downstream handler still sees the original bytes. If the body is
/// larger than the cap we silently skip the peek.
async fn peek_model_and_stream(req: Request) -> (Request, Option<(Option<String>, Option<bool>)>) {
    let (parts, body) = req.into_parts();
    let (body, complete) = match peek_body_prefix(body).await {
        Ok(peeked) => peeked,
        Err(_) => {
            // On a real body read error, keep the existing conservative
            // behavior: the consumed prefix cannot be recovered.
            let req = Request::from_parts(parts, Body::empty());
            return (req, None);
        }
    };

    let peeked = complete.map(|bytes| parse_model_and_stream(&bytes));
    let req = Request::from_parts(parts, body);
    (req, peeked)
}

/// Parse the JSON body and pull out `model` (string) and `stream` (bool).
/// Returns `(None, None)` for non-objects or invalid JSON; partial extraction
/// is fine.
fn parse_model_and_stream(body: &Bytes) -> (Option<String>, Option<bool>) {
    if body.is_empty() {
        return (None, None);
    }
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return (None, None),
    };
    let model = obj
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    let stream = obj.get("stream").and_then(|s| s.as_bool());
    (model, stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request as HttpRequest};

    #[tokio::test]
    async fn peek_handles_empty_body_without_panic() {
        let req: Request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();

        let (req, peeked) = peek_model_and_stream(req).await;
        assert_eq!(req.method(), Method::POST);
        let (model, stream) = peeked.expect("peek attempted");
        assert!(model.is_none());
        assert!(stream.is_none());
    }

    #[tokio::test]
    async fn peek_extracts_model_and_stream_fields() {
        let body = br#"{"model":"gpt-5","stream":true,"messages":[]}"#;
        let req: Request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(&body[..]))
            .unwrap();

        let (rebuilt, peeked) = peek_model_and_stream(req).await;
        let (model, stream) = peeked.unwrap();
        assert_eq!(model.as_deref(), Some("gpt-5"));
        assert_eq!(stream, Some(true));

        // The body must still be readable downstream.
        let bytes = axum::body::to_bytes(rebuilt.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&bytes[..], &body[..]);
    }

    #[tokio::test]
    async fn peek_skips_oversized_body_but_rebuilds_it_intact() {
        let body: Vec<u8> = (0..PEEK_LIMIT + 17).map(|i| (i % 251) as u8).collect();
        let req: Request = HttpRequest::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.clone()))
            .unwrap();

        let (rebuilt, peeked) = peek_model_and_stream(req).await;
        assert!(peeked.is_none());

        let bytes = axum::body::to_bytes(rebuilt.into_body(), PEEK_LIMIT + 32)
            .await
            .unwrap();
        assert_eq!(&bytes[..], &body[..]);
    }

    #[test]
    fn json_content_type_detection() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert!(is_json_content_type(&h));

        h.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.api+json"),
        );
        assert!(is_json_content_type(&h));

        h.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        assert!(!is_json_content_type(&h));
    }
}
