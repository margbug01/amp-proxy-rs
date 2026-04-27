//! Gemini generateContent ↔ OpenAI Responses bridge.
//!
//! Two flavours, dispatched on the URL action:
//!
//!   * `:generateContent` — non-streaming. Request body is translated, sent
//!     upstream as SSE for parity with augment's preferred shape, and the
//!     full upstream reply is collapsed back into a single Gemini JSON body
//!     via [`crate::customproxy::gemini_translator::translate_gemini_response`].
//!   * `:streamGenerateContent` — streaming. Same request-side translation,
//!     but the upstream SSE stream is piped event-by-event through
//!     [`crate::customproxy::gemini_stream_translator::translate_responses_sse_to_gemini`]
//!     and surfaced to the client as Gemini-shape SSE chunks. Stops the
//!     `finder` sub-agent from leaking traffic to ampcode.com.
//!
//! Both paths share request-side wiring; the dispatch lives at the top of
//! [`forward_gemini_translated`].

use anyhow::{anyhow, Context};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use tracing::{info, warn};

use crate::customproxy::{retry_transport::RetryTransport, Provider};

/// Hard cap on non-streaming upstream response bodies that must be fully
/// buffered for Gemini translation.
const MAX_GEMINI_BRIDGE_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Forward a Gemini `:generateContent` or `:streamGenerateContent` request
/// to a custom provider via the Gemini ↔ OpenAI Responses translator pair.
///
/// `path` is the original incoming URL path (used to decide streaming vs.
/// non-streaming dispatch). `mapped_model` is the upstream-served name after
/// the FallbackHandler's mapping step. `original_model` is what Amp CLI
/// asked for; we echo it back into the translated reply's `modelVersion`
/// field so downstream Amp logs stay coherent.
pub async fn forward_gemini_translated(
    provider: &Provider,
    body: Bytes,
    mapped_model: &str,
    original_model: &str,
    path: &str,
    client: &reqwest::Client,
) -> anyhow::Result<Response> {
    let stream_mode = path.ends_with(":streamGenerateContent");

    // 1. Translate the Gemini body into an OpenAI Responses body.
    let mut translated_body = translate_request(&body, mapped_model)
        .context("translate gemini request to openai responses")?;

    // Pin the upstream `stream` flag explicitly to whichever path we're on
    // so it can't drift (the underlying translator defaults to stream=true
    // for parity with augment's preferred shape, but for `:generateContent`
    // we want a single non-streaming JSON body or `translate_gemini_response`
    // can't parse it).
    translated_body = ensure_stream_field(&translated_body, stream_mode);

    // 2. Build the upstream URL: provider.url + "/responses".
    let base = provider.url.trim_end_matches('/');
    let url = if base.ends_with("/v1") || base.ends_with("/v1beta") {
        format!("{base}/responses")
    } else {
        format!("{base}/v1/responses")
    };

    let in_bytes = body.len();
    info!(
        provider = %provider.name,
        original_model = %original_model,
        mapped_model = %mapped_model,
        url = %url,
        path = %path,
        stream = stream_mode,
        in_bytes,
        translated_bytes = translated_body.len(),
        "gemini-translate: forwarding"
    );

    // 3. Issue the upstream request. Accept must match the body's stream
    //    flag — augment serves SSE only when the client asked for it.
    let accept = if stream_mode {
        "text/event-stream"
    } else {
        "application/json"
    };
    let outbound_body = Bytes::from(translated_body);
    let retry = RetryTransport::new(client.clone());
    let upstream = match retry
        .send_with_retry(|| {
            let mut req = client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Accept", accept);
            let bearer = provider.api_key.trim();
            if !bearer.is_empty() {
                req = req.header("Authorization", format!("Bearer {bearer}"));
            }
            req.body(outbound_body.clone())
        })
        .await
    {
        Ok(resp) => {
            crate::customproxy::global().record_success(&provider.name);
            resp
        }
        Err(err) => {
            crate::customproxy::global().record_failure(&provider.name, err.to_string());
            return Err(err).with_context(|| format!("upstream request to {url}"));
        }
    };

    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    if stream_mode {
        // Streaming path: pipe upstream Responses SSE through the streaming
        // translator and return as Gemini-shape SSE.
        let upstream_stream = upstream
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let translated =
            crate::customproxy::gemini_stream_translator::translate_responses_sse_to_gemini(
                upstream_stream,
                original_model.to_string(),
            );
        let mut response = Response::builder().status(status_or_502(status));
        {
            let h = response
                .headers_mut()
                .ok_or_else(|| anyhow!("response builder lost its header map"))?;
            h.insert(
                "Content-Type",
                HeaderValue::from_static("text/event-stream"),
            );
            h.insert("Cache-Control", HeaderValue::from_static("no-cache"));
        }
        return response
            .body(Body::from_stream(translated))
            .context("build axum streaming response");
    }

    // Non-streaming path: read the full upstream body, translate to JSON.
    let upstream_bytes = read_limited(upstream, MAX_GEMINI_BRIDGE_RESPONSE_BYTES)
        .await
        .context("read upstream response body")?;

    let gemini_body = match translate_response(&upstream_bytes, original_model) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                provider = %provider.name,
                model = %mapped_model,
                error = %e,
                "gemini-translate: response translation failed; surfacing raw upstream body"
            );
            return Ok(passthrough_response(
                status,
                &upstream_headers,
                upstream_bytes,
            ));
        }
    };

    let mut response = Response::builder().status(status_or_502(status));
    {
        let h = response
            .headers_mut()
            .ok_or_else(|| anyhow!("response builder lost its header map"))?;
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        if let Ok(v) = HeaderValue::from_str(&gemini_body.len().to_string()) {
            h.insert("Content-Length", v);
        }
    }
    response
        .body(Body::from(gemini_body))
        .context("build axum response")
}

fn passthrough_response(status: StatusCode, headers: &HeaderMap, bytes: Bytes) -> Response {
    let mut b = Response::builder().status(status);
    {
        let h = b.headers_mut().expect("fresh builder has headers");
        for (k, v) in headers.iter() {
            let name = k.as_str().to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "content-length" | "transfer-encoding" | "connection" | "keep-alive" | "upgrade"
            ) {
                continue;
            }
            h.append(k.clone(), v.clone());
        }
    }
    b.body(Body::from(bytes))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn read_limited(upstream: reqwest::Response, limit: usize) -> anyhow::Result<Bytes> {
    if upstream
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err(anyhow!("upstream response body exceeds {limit} bytes"));
    }

    let mut stream = upstream.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read upstream response chunk")?;
        if buf.len().saturating_add(chunk.len()) > limit {
            return Err(anyhow!("upstream response body exceeds {limit} bytes"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

fn status_or_502(s: StatusCode) -> StatusCode {
    if s.is_informational() {
        StatusCode::BAD_GATEWAY
    } else {
        s
    }
}

/// Force-set or clear the top-level `stream` field on a JSON request body.
/// Used by the streaming path to guarantee the upstream returns SSE.
/// Falls back to the original bytes if the body is not parseable JSON.
fn ensure_stream_field(body: &[u8], stream: bool) -> Vec<u8> {
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert("stream".into(), Value::Bool(stream));
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

fn translate_request(body: &[u8], mapped_model: &str) -> anyhow::Result<Vec<u8>> {
    crate::customproxy::gemini_translator::translate_gemini_request_to_openai(body, mapped_model)
}

fn translate_response(body: &[u8], original_model: &str) -> anyhow::Result<Vec<u8>> {
    crate::customproxy::gemini_translator::translate_gemini_response(body, original_model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_request_calls_real_translator() {
        let body = br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
        let out = translate_request(body, "gpt-5.4-mini").expect("translate succeeds");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpt-5.4-mini");
        assert!(v["input"].is_array());
    }

    #[test]
    fn status_demotes_informational() {
        assert_eq!(status_or_502(StatusCode::CONTINUE), StatusCode::BAD_GATEWAY);
        assert_eq!(status_or_502(StatusCode::OK), StatusCode::OK);
    }

    #[test]
    fn ensure_stream_field_sets_true() {
        let body = br#"{"model":"x","messages":[]}"#;
        let out = ensure_stream_field(body, true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn ensure_stream_field_overwrites_existing() {
        let body = br#"{"stream":false,"model":"x"}"#;
        let out = ensure_stream_field(body, true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], true);
    }
}
