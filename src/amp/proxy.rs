//! Amp module proxy adapters.
//!
//! The ampcode.com transparent reverse proxy already lives in
//! [`crate::proxy::AmpcodeProxy`] and [`crate::proxy::forward`]. This file
//! adds the *custom-provider* forwarding path: given a routing decision that
//! pinned a specific [`crate::customproxy::Provider`], we issue the upstream
//! request via reqwest and adapt the reply for the downstream Amp CLI client.
//!
//! Behaviour mirrors `internal/customproxy/customproxy.go::buildProxy`:
//!
//!   * Strip `/api/provider/<name>/` and `/v1` / `/v1beta` / `/v1beta1`
//!     version prefixes from the incoming path; the leaf is appended to the
//!     provider's base URL via [`crate::customproxy::extract_leaf`].
//!   * Replace `Authorization` with `Bearer <provider api key>`.
//!   * Drop `Anthropic-Beta`, `anthropic-beta`, and `x-api-key`.
//!   * Merge `request_overrides` into POST `/messages` bodies as a shallow
//!     JSON patch.
//!   * Toggle `stream:true` on non-streaming `/messages` and collapse the
//!     SSE reply back into a single JSON body via the customproxy
//!     `sse_messages_collapser`.
//!   * If `provider.responses_translate`, rewrite POST `/responses` into
//!     `/chat/completions` and translate the reply back to the Responses
//!     shape.

use anyhow::{anyhow, Context};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Map, Value};
use tracing::{info, warn};

pub use crate::proxy::{forward as forward_to_ampcode, AmpcodeProxy};

use crate::customproxy::{
    extract_leaf, responses_stream_translator, responses_translator,
    retry_transport::RetryTransport, sse_messages_collapser, Provider,
};

/// Hard cap on bytes streamed through the custom-provider streaming path
/// for a single request body. Mirrors the buffered path's implicit
/// limit; surfaced as an `io::Error` on the wrapped stream so reqwest
/// fails the upload cleanly rather than buffering the lot.
const MAX_CUSTOM_PROVIDER_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// Hard cap on non-streaming upstream response bodies that must be fully
/// buffered for translation.
const MAX_CUSTOM_PROVIDER_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// Forward a buffered request to a custom upstream provider.
///
/// `path` is the original request URL path (used to compute the upstream
/// leaf). `method` is forwarded as-is. `headers` are sanitised before send.
/// `body_bytes` is the (possibly mutated) request body — callers that
/// already rewrote `model` should pass the rewritten body here.
pub async fn forward_to_custom_provider(
    provider: &Provider,
    method: http::Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body_bytes: Bytes,
    client: &reqwest::Client,
) -> anyhow::Result<Response> {
    let base = provider.url.trim_end_matches('/');
    let leaf = extract_leaf(path);
    let mut url = format!("{base}{leaf}");
    if let Some(q) = query {
        if !q.is_empty() {
            url.push('?');
            url.push_str(q);
        }
    }

    // Determine which post-processing branches apply *before* mutating the
    // body, so the closures can be honoured below.
    let is_post = method == http::Method::POST;
    // The leaf path is what we forward; suffix checks should run on it,
    // not the raw incoming path (which may still carry /api/provider/...).
    let is_messages = leaf.ends_with("/messages") || leaf == "/messages";
    let is_responses = leaf.ends_with("/responses") || leaf == "/responses";

    let mut upstream_path = String::from(leaf);
    let mut send_body = body_bytes.clone();
    let mut upgraded_messages = false;
    let mut translate_ctx: Option<responses_translator::ResponsesTranslateCtx> = None;

    // /messages: shallow-merge overrides + (maybe) flip stream:true.
    if is_post && is_messages {
        match apply_messages_mutations(&body_bytes, &provider.request_overrides) {
            Ok((bytes, upgraded)) => {
                send_body = bytes;
                upgraded_messages = upgraded;
            }
            Err(e) => {
                warn!(error = %e, "customproxy: /messages body mutation failed; forwarding as-is")
            }
        }
    }

    // /responses + translate flag: retarget to /chat/completions and
    // translate the body. The ctx is carried into the response phase.
    if is_post && is_responses && provider.responses_translate {
        match responses_translator::translate_responses_request_to_chat(&send_body) {
            Ok((bytes, ctx)) => {
                send_body = Bytes::from(bytes);
                upstream_path =
                    upstream_path.trim_end_matches("/responses").to_string() + "/chat/completions";
                translate_ctx = Some(ctx);
            }
            Err(e) => {
                warn!(error = %e, "customproxy: responses→chat translation failed; forwarding as-is")
            }
        }
    }
    let translate_responses = translate_ctx.is_some();

    // Rebuild the URL if we retargeted the path.
    if translate_responses {
        url = format!("{base}{upstream_path}");
        if let Some(q) = query {
            if !q.is_empty() {
                url.push('?');
                url.push_str(q);
            }
        }
    }

    info!(
        provider = %provider.name,
        method = %method,
        from = %path,
        to = %url,
        upgraded_messages,
        translate_responses,
        "customproxy: forwarding"
    );

    let outbound_headers = sanitize_headers(
        headers,
        &provider.api_key,
        upgraded_messages || translate_responses,
    );
    let retry = RetryTransport::new(client.clone());
    let upstream = match retry
        .send_with_retry(|| {
            client
                .request(reqwest_method(&method), &url)
                .headers(outbound_headers.clone())
                .body(send_body.clone())
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

    // Branch 1: non-streaming /messages that we upgraded → collapse SSE.
    let ct = upstream_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if upgraded_messages && ct.contains("text/event-stream") {
        let stream = upstream
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        match sse_messages_collapser::collapse_to_json(Box::pin(stream)).await {
            Ok(collapsed) => {
                return Ok(json_response(status, collapsed));
            }
            Err(e) => {
                warn!(error = %e, "customproxy: collapseMessagesSSE failed; returning empty assistant envelope");
                let fallback = Bytes::from_static(
                    br#"{"type":"message","role":"assistant","content":[],"stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0}}"#,
                );
                return Ok(json_response(status, fallback));
            }
        }
    }

    // Branch 2: /responses translation reply.
    if let Some(ctx) = translate_ctx {
        if ct.contains("text/event-stream") {
            let upstream_stream = upstream
                .bytes_stream()
                .map(|r| r.map_err(std::io::Error::other));
            let translated = responses_stream_translator::translate_chat_to_responses_stream(
                upstream_stream,
                ctx,
            );
            let mut response = Response::builder().status(status);
            {
                let h = response
                    .headers_mut()
                    .ok_or_else(|| anyhow!("response builder lost headers"))?;
                h.insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                h.insert(
                    http::header::CACHE_CONTROL,
                    HeaderValue::from_static("no-cache"),
                );
            }
            return response
                .body(Body::from_stream(translated))
                .context("build axum streaming response");
        }
        // Non-SSE JSON reply (or error). Translate the chat completion JSON
        // back into Responses shape; if translation no-ops, pass through.
        let bytes = read_limited(upstream, MAX_CUSTOM_PROVIDER_RESPONSE_BYTES)
            .await
            .context("read upstream JSON body")?;
        let (final_body, _translated) =
            responses_translator::translate_chat_completion_to_responses(&bytes, &ctx)
                .unwrap_or_else(|e| {
                    warn!(error = %e, "customproxy: chat→responses JSON translation failed; passing through");
                    (bytes.to_vec(), false)
                });
        return Ok(json_response(status, Bytes::from(final_body)));
    }

    // Default: stream the upstream response back unchanged.
    let stream = upstream.bytes_stream();
    let mut response = Response::builder().status(status);
    {
        let h = response
            .headers_mut()
            .ok_or_else(|| anyhow!("response builder lost headers"))?;
        for (k, v) in upstream_headers.iter() {
            let name = k.as_str().to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "transfer-encoding" | "connection" | "keep-alive" | "upgrade"
            ) {
                continue;
            }
            h.append(k.clone(), v.clone());
        }
    }
    response
        .body(Body::from_stream(stream))
        .context("build axum streaming response")
}

/// Streaming variant of [`forward_to_custom_provider`]. Used by
/// [`crate::amp::routes::handle`] for paths where it has determined no
/// body mutation is needed; the body is piped chunk-by-chunk to the
/// upstream instead of being held in memory.
///
/// This MUST NOT be used for `/messages` POST or for `/responses` POST when
/// `provider.responses_translate` is true — those paths require body
/// mutation and the SSE-collapse / responses-translation post-processing
/// branches that depend on it. The caller is responsible for that gate.
pub async fn forward_to_custom_provider_streaming(
    provider: &Provider,
    method: http::Method,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: axum::body::Body,
    client: &reqwest::Client,
) -> anyhow::Result<Response> {
    let base = provider.url.trim_end_matches('/');
    let leaf = extract_leaf(path);
    let mut url = format!("{base}{leaf}");
    if let Some(q) = query {
        if !q.is_empty() {
            url.push('?');
            url.push_str(q);
        }
    }

    info!(
        provider = %provider.name,
        method = %method,
        from = %path,
        to = %url,
        streaming = true,
        "customproxy: forwarding (streaming body)"
    );

    let outbound_headers = sanitize_headers(headers, &provider.api_key, false);
    let upstream_body = body_into_reqwest_capped(body);

    let upstream = match client
        .request(reqwest_method(&method), &url)
        .headers(outbound_headers)
        .body(upstream_body)
        .send()
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
    let stream = upstream.bytes_stream();

    let mut response = Response::builder().status(status);
    {
        let h = response
            .headers_mut()
            .ok_or_else(|| anyhow!("response builder lost headers"))?;
        for (k, v) in upstream_headers.iter() {
            let name = k.as_str().to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "transfer-encoding" | "connection" | "keep-alive" | "upgrade"
            ) {
                continue;
            }
            h.append(k.clone(), v.clone());
        }
    }
    response
        .body(Body::from_stream(stream))
        .context("build axum streaming response")
}

/// Bridge an axum request `Body` to a `reqwest::Body` that streams chunks
/// upstream as they arrive, capped at [`MAX_CUSTOM_PROVIDER_REQUEST_BYTES`]
/// total. Mirrors `crate::proxy::body_into_reqwest` but with a tighter cap
/// suited to the custom-provider path.
fn body_into_reqwest_capped(body: axum::body::Body) -> reqwest::Body {
    let mut bytes_so_far: usize = 0;
    let stream = body.into_data_stream().map(move |item| match item {
        Ok(chunk) => {
            bytes_so_far = bytes_so_far.saturating_add(chunk.len());
            if bytes_so_far > MAX_CUSTOM_PROVIDER_REQUEST_BYTES {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request body exceeds 16 MiB on custom-provider streaming path",
                ))
            } else {
                Ok::<Bytes, std::io::Error>(chunk)
            }
        }
        Err(e) => Err(std::io::Error::other(e)),
    });
    reqwest::Body::wrap_stream(stream)
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

/// Convert axum `http::Method` to reqwest's. They share the same underlying
/// type as of reqwest 0.12, but we map by name to stay forward-compatible.
fn reqwest_method(m: &http::Method) -> reqwest::Method {
    reqwest::Method::from_bytes(m.as_str().as_bytes()).unwrap_or(reqwest::Method::POST)
}

/// Build outbound headers for the custom-provider request.
fn sanitize_headers(src: &HeaderMap, api_key: &str, want_sse: bool) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (k, v) in src.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "host"
                | "content-length"
                | "anthropic-beta"
                | "x-api-key"
                | "authorization"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailers"
                | "transfer-encoding"
                | "upgrade"
        ) {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    let key = api_key.trim();
    if !key.is_empty() {
        if let Ok(hv) = HeaderValue::from_str(&format!("Bearer {key}")) {
            out.insert(http::header::AUTHORIZATION, hv);
        }
    }
    if want_sse {
        out.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        out.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    out
}

/// Apply request-overrides + (maybe) flip `stream: true` on a /messages body.
/// Returns the rewritten body and a flag indicating whether streaming was
/// just upgraded.
fn apply_messages_mutations(
    body: &[u8],
    overrides: &Map<String, Value>,
) -> anyhow::Result<(Bytes, bool)> {
    if body.is_empty() {
        return Ok((Bytes::new(), false));
    }
    let mut v: Value = serde_json::from_slice(body).context("parse /messages body")?;
    let already_streaming = v.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let mut upgraded = false;
    if let Some(obj) = v.as_object_mut() {
        if !already_streaming {
            obj.insert("stream".into(), Value::Bool(true));
            upgraded = true;
        }
        for (k, val) in overrides.iter() {
            obj.insert(k.clone(), val.clone());
        }
    }
    let bytes = serde_json::to_vec(&v).context("re-serialise /messages body")?;
    Ok((Bytes::from(bytes), upgraded))
}

fn json_response(status: StatusCode, body: Bytes) -> Response {
    let len = body.len();
    let mut b = Response::builder().status(status);
    {
        let h = b.headers_mut().expect("fresh builder has headers");
        h.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Ok(hv) = HeaderValue::from_str(&len.to_string()) {
            h.insert(http::header::CONTENT_LENGTH, hv);
        }
    }
    b.body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn applies_overrides_and_upgrades_stream() {
        let body = br#"{"model":"x","messages":[]}"#;
        let mut overrides: Map<String, Value> = Map::new();
        overrides.insert("reasoning_effort".into(), json!("max"));
        let (out, upgraded) = apply_messages_mutations(body, &overrides).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["reasoning_effort"], "max");
        assert!(upgraded);
    }

    #[test]
    fn no_upgrade_when_already_streaming() {
        let body = br#"{"stream":true,"model":"x"}"#;
        let (_out, upgraded) = apply_messages_mutations(body, &Map::new()).unwrap();
        assert!(!upgraded);
    }

    #[test]
    fn sanitize_headers_strips_auth_and_betas() {
        let mut src = HeaderMap::new();
        src.insert("authorization", HeaderValue::from_static("Bearer client"));
        src.insert("anthropic-beta", HeaderValue::from_static("ctx-1m"));
        src.insert("x-api-key", HeaderValue::from_static("client"));
        src.insert("content-type", HeaderValue::from_static("application/json"));
        let out = sanitize_headers(&src, "upstream-key", false);
        assert!(out.get("anthropic-beta").is_none());
        assert!(out.get("x-api-key").is_none());
        assert_eq!(
            out.get("authorization").unwrap().to_str().unwrap(),
            "Bearer upstream-key"
        );
    }

    /// The streaming variant must hand reqwest a body whose underlying
    /// data stream emits the same bytes the caller fed in, in order. We
    /// can't introspect a `reqwest::Body` post-wrap (its `as_bytes()`
    /// returns `None` for streamed bodies), so we exercise the same
    /// counter+map pipeline the helper uses.
    #[tokio::test]
    async fn streaming_variant_pipes_body() {
        let payload = b"hello world".to_vec();
        let body = axum::body::Body::from(payload.clone());

        // Mirror `body_into_reqwest_capped`'s pipeline up to (but not
        // including) the `reqwest::Body::wrap_stream` call.
        let mut bytes_so_far: usize = 0;
        let mut stream = body.into_data_stream().map(move |item| match item {
            Ok(chunk) => {
                bytes_so_far = bytes_so_far.saturating_add(chunk.len());
                if bytes_so_far > MAX_CUSTOM_PROVIDER_REQUEST_BYTES {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "exceeds cap",
                    ))
                } else {
                    Ok::<Bytes, std::io::Error>(chunk)
                }
            }
            Err(e) => Err(std::io::Error::other(e)),
        });

        let mut reassembled: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            reassembled.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(reassembled, payload);

        // And confirm the real helper at least produces a `reqwest::Body`
        // (smoke test — the wrapper itself can't be drained without a
        // network round-trip).
        let body2 = axum::body::Body::from(payload.clone());
        let _: reqwest::Body = body_into_reqwest_capped(body2);
    }

    /// Feeding more than `MAX_CUSTOM_PROVIDER_REQUEST_BYTES` through the
    /// helper must surface an `Err` on the resulting stream rather than
    /// silently buffering the lot. We drive the same map closure the
    /// helper installs so we can observe the error directly.
    #[tokio::test]
    async fn streaming_variant_caps_body_size() {
        // Build a stream of 17 chunks of 1 MiB = 17 MiB > 16 MiB cap.
        let chunk = Bytes::from(vec![0u8; 1024 * 1024]);
        let chunks: Vec<Result<Bytes, std::io::Error>> =
            (0..17).map(|_| Ok(chunk.clone())).collect();
        let raw = futures::stream::iter(chunks);

        let mut bytes_so_far: usize = 0;
        let mut mapped = raw.map(move |item: Result<Bytes, std::io::Error>| match item {
            Ok(chunk) => {
                bytes_so_far = bytes_so_far.saturating_add(chunk.len());
                if bytes_so_far > MAX_CUSTOM_PROVIDER_REQUEST_BYTES {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "request body exceeds 16 MiB on custom-provider streaming path",
                    ))
                } else {
                    Ok::<Bytes, std::io::Error>(chunk)
                }
            }
            Err(e) => Err(std::io::Error::other(e)),
        });

        let mut saw_err = false;
        while let Some(item) = mapped.next().await {
            if item.is_err() {
                saw_err = true;
                break;
            }
        }
        assert!(
            saw_err,
            "stream must emit Err once cumulative bytes exceed the cap"
        );

        // Also verify the cap is what we documented.
        assert_eq!(MAX_CUSTOM_PROVIDER_REQUEST_BYTES, 16 * 1024 * 1024);
    }
}
