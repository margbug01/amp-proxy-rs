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
//!
//! When the matched provider sets `responses_translate` (chat/completions only)
//! or `messages_translate` (Anthropic Messages only), the bridge performs a
//! second translation step so the upstream receives the format it speaks:
//!
//!   * `responses_translate`: Gemini → Responses → chat/completions → provider
//!   * `messages_translate`:  Gemini → Responses → Anthropic Messages → provider

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

    // 2. Dispatch to the appropriate upstream format.
    if provider.messages_translate {
        return forward_via_messages(
            provider,
            &translated_body,
            mapped_model,
            original_model,
            path,
            stream_mode,
            client,
        )
        .await;
    }

    if provider.responses_translate {
        return forward_via_chat_completions(
            provider,
            &translated_body,
            mapped_model,
            original_model,
            path,
            stream_mode,
            client,
        )
        .await;
    }

    // Default: provider speaks OpenAI Responses natively.
    forward_via_responses(
        provider,
        translated_body,
        mapped_model,
        original_model,
        path,
        stream_mode,
        body.len(),
        client,
    )
    .await
}

// ---------------------------------------------------------------------------
// Path A: native OpenAI Responses (existing behaviour)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn forward_via_responses(
    provider: &Provider,
    translated_body: Vec<u8>,
    mapped_model: &str,
    original_model: &str,
    path: &str,
    stream_mode: bool,
    in_bytes: usize,
    client: &reqwest::Client,
) -> anyhow::Result<Response> {
    let base = provider.url.trim_end_matches('/');
    let url = if base.ends_with("/v1") || base.ends_with("/v1beta") {
        format!("{base}/responses")
    } else {
        format!("{base}/v1/responses")
    };

    info!(
        provider = %provider.name,
        original_model = %original_model,
        mapped_model = %mapped_model,
        url = %url,
        path = %path,
        stream = stream_mode,
        in_bytes,
        translated_bytes = translated_body.len(),
        "gemini-translate: forwarding (responses)"
    );

    let accept = if stream_mode {
        "text/event-stream"
    } else {
        "application/json"
    };
    let outbound_body = Bytes::from(translated_body);
    let upstream = send_upstream(provider, client, &url, accept, outbound_body, &[]).await?;
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    if stream_mode {
        let upstream_stream = upstream
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let translated =
            crate::customproxy::gemini_stream_translator::translate_responses_sse_to_gemini(
                upstream_stream,
                original_model.to_string(),
            );
        return build_sse_response(status, translated);
    }

    let upstream_bytes = read_limited(upstream, MAX_GEMINI_BRIDGE_RESPONSE_BYTES)
        .await
        .context("read upstream response body")?;

    match translate_gemini_response(&upstream_bytes, original_model) {
        Ok(b) => build_json_response(status, b),
        Err(e) => {
            warn!(
                provider = %provider.name,
                model = %mapped_model,
                error = %e,
                "gemini-translate: response translation failed; surfacing raw upstream body"
            );
            Ok(passthrough_response(
                status,
                &upstream_headers,
                upstream_bytes,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Path B: responses-translate → chat/completions
// ---------------------------------------------------------------------------

async fn forward_via_chat_completions(
    provider: &Provider,
    responses_body: &[u8],
    mapped_model: &str,
    original_model: &str,
    path: &str,
    stream_mode: bool,
    client: &reqwest::Client,
) -> anyhow::Result<Response> {
    use crate::customproxy::responses_translator;

    // Strip reasoning fields before the Responses→chat translation so the
    // upstream provider does not enter thinking mode. Gemini's wire format
    // cannot carry `reasoning_content` in multi-turn conversations, which
    // causes DeepSeek (and similar providers) to reject the second request
    // with "reasoning_content must be passed back to the API".
    let sanitised = strip_reasoning_fields(responses_body);

    // Responses → chat/completions
    let (chat_body, ctx) = responses_translator::translate_responses_request_to_chat(&sanitised)
        .context("gemini bridge: responses→chat translation")?;

    // Explicitly disable thinking mode. DeepSeek v4-flash defaults to
    // thinking-on; the Gemini wire format cannot round-trip
    // reasoning_content so multi-turn conversations break.
    let chat_body = force_thinking_disabled(&chat_body);

    let base = provider.url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    };

    info!(
        provider = %provider.name,
        original_model = %original_model,
        mapped_model = %mapped_model,
        url = %url,
        path = %path,
        stream = stream_mode,
        "gemini-translate: forwarding (chat/completions via responses-translate)"
    );

    let accept = if stream_mode {
        "text/event-stream"
    } else {
        "application/json"
    };
    let outbound_body = Bytes::from(chat_body);
    let upstream = send_upstream(provider, client, &url, accept, outbound_body, &[]).await?;
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    if stream_mode {
        // chat SSE → Responses SSE → Gemini SSE (chained translators)
        let chat_stream = upstream
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let responses_stream =
            crate::customproxy::responses_stream_translator::translate_chat_to_responses_stream(
                chat_stream,
                ctx,
            );
        // Wrap in a Box::pin so it satisfies Send + 'static for the second
        // translator which also expects Stream<Item = Result<Bytes, io::Error>>.
        let pinned = Box::pin(responses_stream);
        let gemini_stream =
            crate::customproxy::gemini_stream_translator::translate_responses_sse_to_gemini(
                pinned,
                original_model.to_string(),
            );
        return build_sse_response(status, gemini_stream);
    }

    // Non-streaming: chat JSON → Responses JSON → Gemini JSON
    let upstream_bytes = read_limited(upstream, MAX_GEMINI_BRIDGE_RESPONSE_BYTES)
        .await
        .context("read upstream chat/completions response")?;

    // Step 1: chat → Responses
    let responses_body =
        match responses_translator::translate_chat_completion_to_responses(&upstream_bytes, &ctx) {
            Ok((body, true)) => body,
            Ok((_, false)) | Err(_) => {
                warn!(
                    provider = %provider.name,
                    "gemini bridge: chat→responses translation no-op; surfacing raw body"
                );
                return Ok(passthrough_response(
                    status,
                    &upstream_headers,
                    upstream_bytes,
                ));
            }
        };

    // Step 2: Responses → Gemini
    match translate_gemini_response(&responses_body, original_model) {
        Ok(b) => build_json_response(status, b),
        Err(e) => {
            warn!(
                provider = %provider.name,
                error = %e,
                "gemini bridge: responses→gemini translation failed; surfacing raw body"
            );
            Ok(passthrough_response(
                status,
                &upstream_headers,
                upstream_bytes,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Path C: messages-translate → Anthropic Messages
// ---------------------------------------------------------------------------

async fn forward_via_messages(
    provider: &Provider,
    responses_body: &[u8],
    mapped_model: &str,
    original_model: &str,
    path: &str,
    stream_mode: bool,
    client: &reqwest::Client,
) -> anyhow::Result<Response> {
    use crate::customproxy::messages_translator;

    // Strip reasoning — same rationale as the chat/completions path above.
    let sanitised = strip_reasoning_fields(responses_body);

    // Responses → Anthropic Messages
    let messages_body = messages_translator::translate_responses_to_messages(&sanitised)
        .context("gemini bridge: responses→messages translation")?;

    // Explicitly disable thinking mode for Anthropic-compatible upstreams
    // that support DeepSeek's extension. Gemini cannot round-trip thinking
    // content/signatures, so leaving provider defaults enabled can break
    // multi-turn conversations.
    let messages_body = force_thinking_disabled(&messages_body);

    let base = provider.url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };

    info!(
        provider = %provider.name,
        original_model = %original_model,
        mapped_model = %mapped_model,
        url = %url,
        path = %path,
        stream = stream_mode,
        "gemini-translate: forwarding (anthropic messages via messages-translate)"
    );

    let accept = if stream_mode {
        "text/event-stream"
    } else {
        "application/json"
    };
    let outbound_body = Bytes::from(messages_body);
    let upstream = send_upstream(
        provider,
        client,
        &url,
        accept,
        outbound_body,
        &[("anthropic-version", "2023-06-01")],
    )
    .await?;
    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    if stream_mode {
        // Anthropic Messages SSE → Gemini SSE
        let upstream_stream = upstream
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let gemini_stream = messages_translator::translate_messages_sse_to_gemini(
            upstream_stream,
            original_model.to_string(),
        );
        return build_sse_response(status, gemini_stream);
    }

    // Non-streaming: Anthropic Messages JSON → Gemini JSON
    let upstream_bytes = read_limited(upstream, MAX_GEMINI_BRIDGE_RESPONSE_BYTES)
        .await
        .context("read upstream messages response")?;

    match messages_translator::translate_messages_response_to_gemini(
        &upstream_bytes,
        original_model,
    ) {
        Ok(b) => build_json_response(status, b),
        Err(e) => {
            warn!(
                provider = %provider.name,
                error = %e,
                "gemini bridge: messages→gemini translation failed; surfacing raw body"
            );
            Ok(passthrough_response(
                status,
                &upstream_headers,
                upstream_bytes,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

async fn send_upstream(
    provider: &Provider,
    client: &reqwest::Client,
    url: &str,
    accept: &str,
    body: Bytes,
    extra_headers: &[(&str, &str)],
) -> anyhow::Result<reqwest::Response> {
    let retry = RetryTransport::new(client.clone());
    match retry
        .send_with_retry(|| {
            let mut req = client
                .post(url)
                .header("Content-Type", "application/json")
                .header("Accept", accept);
            let bearer = provider.api_key.trim();
            if !bearer.is_empty() {
                req = req.header("Authorization", format!("Bearer {bearer}"));
            }
            for (name, value) in extra_headers {
                req = req.header(*name, *value);
            }
            req.body(body.clone())
        })
        .await
    {
        Ok(resp) => {
            crate::customproxy::global().record_success(&provider.name);
            Ok(resp)
        }
        Err(err) => {
            crate::customproxy::global().record_failure(&provider.name, err.to_string());
            Err(err).with_context(|| format!("upstream request to {url}"))
        }
    }
}

fn build_sse_response<S>(status: StatusCode, stream: S) -> anyhow::Result<Response>
where
    S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
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
    response
        .body(Body::from_stream(stream))
        .context("build axum streaming response")
}

fn build_json_response(status: StatusCode, body: Vec<u8>) -> anyhow::Result<Response> {
    let mut response = Response::builder().status(status_or_502(status));
    {
        let h = response
            .headers_mut()
            .ok_or_else(|| anyhow!("response builder lost its header map"))?;
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        if let Ok(v) = HeaderValue::from_str(&body.len().to_string()) {
            h.insert("Content-Length", v);
        }
    }
    response
        .body(Body::from(body))
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

/// Force-set `thinking: {type: "disabled"}` and remove `reasoning_effort`
/// on a chat/completions body. DeepSeek v4-flash defaults to thinking-on;
/// the Gemini bridge cannot round-trip reasoning_content.
fn force_thinking_disabled(body: &[u8]) -> Vec<u8> {
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert("thinking".into(), serde_json::json!({"type": "disabled"}));
        obj.remove("reasoning_effort");
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

/// Remove `reasoning` and reasoning-related `input` items from a Responses
/// body so downstream providers don't enter thinking mode. The Gemini wire
/// format cannot round-trip `reasoning_content`, which causes DeepSeek to
/// reject multi-turn requests.
fn strip_reasoning_fields(body: &[u8]) -> Vec<u8> {
    let mut v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };
    if let Some(obj) = v.as_object_mut() {
        obj.remove("reasoning");
        // Also strip `include` which may request `reasoning.encrypted_content`.
        obj.remove("include");
        // Remove reasoning-type items from `input` so they don't confuse
        // the downstream translator.
        if let Some(input) = obj.get_mut("input").and_then(|v| v.as_array_mut()) {
            input.retain(|item| {
                item.get("type")
                    .and_then(|v| v.as_str())
                    .map(|t| t != "reasoning")
                    .unwrap_or(true)
            });
        }
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

fn translate_request(body: &[u8], mapped_model: &str) -> anyhow::Result<Vec<u8>> {
    crate::customproxy::gemini_translator::translate_gemini_request_to_openai(body, mapped_model)
}

fn translate_gemini_response(body: &[u8], original_model: &str) -> anyhow::Result<Vec<u8>> {
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
    fn strip_reasoning_removes_fields() {
        let body = serde_json::to_vec(&serde_json::json!({
            "model": "x",
            "reasoning": {"effort": "high", "summary": "auto"},
            "include": ["reasoning.encrypted_content"],
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "reasoning", "id": "r1", "encrypted_content": "abc"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]},
            ]
        }))
        .unwrap();
        let out = strip_reasoning_fields(&body);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("reasoning").is_none());
        assert!(v.get("include").is_none());
        let input = v["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert!(input.iter().all(|i| i["type"] != "reasoning"));
    }

    #[test]
    fn ensure_stream_field_overwrites_existing() {
        let body = br#"{"stream":false,"model":"x"}"#;
        let out = ensure_stream_field(body, true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], true);
    }

    #[test]
    fn force_thinking_disabled_sets_disabled_and_removes_reasoning_effort() {
        let body = br#"{"model":"x","messages":[],"reasoning_effort":"high","thinking":{"type":"enabled"}}"#;
        let out = force_thinking_disabled(body);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["thinking"]["type"], "disabled");
        assert!(v.get("reasoning_effort").is_none());
    }
}
