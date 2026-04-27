//! Axum router for the amp module.
//!
//! Ported from `internal/amp/routes.go`. The Go version registers a fan-out of
//! per-provider aliases (`/api/provider/openai/v1/chat/completions`, etc.) and
//! a Gemini bridge; in the Rust port the catch-all fallback in
//! [`crate::server::build_app`] already handles the ampcode.com forwarding,
//! so this module focuses on the *amp-only* routing brain: extract the model,
//! consult [`super::fallback_handlers::FallbackHandler`], and dispatch to
//! either a custom provider, the Gemini translator bridge, or fall through to
//! the existing ampcode.com proxy.
//!
//! The router this builder produces is intended to be merged into a parent
//! axum app via `Router::merge`. It does *not* register the auth middleware —
//! callers should layer that on top.
//!
//! # Hybrid streaming
//!
//! Inbound bodies are no longer buffered to RAM unconditionally. The handler
//! peeks the first [`PEEK_LIMIT`] bytes (enough to JSON-parse the `model`
//! field for any realistic input), takes a routing decision against that
//! prefix, and — when the destination doesn't require body mutation —
//! streams the rest of the body to the upstream chunk-by-chunk via
//! [`super::prefixed_body::PrefixedBody`]. Paths that do need mutation
//! (Gemini translation, model-mapping rewrite, /messages SSE upgrade,
//! /responses translation) fall back to the legacy "buffer fully, sanitise,
//! forward" path.

use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use tracing::{info, warn};

use super::fallback_handlers::{AmpRouteType, RouteDecision};
use super::gemini_bridge::forward_gemini_translated;
use super::prefixed_body::PrefixedBody;
use super::proxy::{forward_to_custom_provider, forward_to_custom_provider_streaming};
use crate::amp::AmpModule;
use crate::customproxy::{self, Provider};

/// How many bytes of an inbound body we read to extract the `model` field
/// for routing. 16 KiB easily covers every realistic Amp CLI / API client
/// payload — the `model` field is at the top of the JSON object and tools /
/// messages arrays come after it.
const PEEK_LIMIT: usize = 16 * 1024;

/// Hard cap on the buffered request body. Mirrors the legacy 16 MiB cap.
const MAX_BUFFERED_BYTES: usize = 16 * 1024 * 1024;

/// Shared state passed to every amp-routed handler.
#[derive(Clone)]
pub struct AmpState {
    pub module: Arc<AmpModule>,
    pub client: Client,
}

impl AmpState {
    /// Build a new amp state with a freshly-configured reqwest client.
    pub fn new(module: Arc<AmpModule>) -> Self {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("build reqwest client");
        Self { module, client }
    }
}

/// Build the amp module router. Mounts all known amp route shapes onto a
/// single catch-all that funnels through the fallback brain.
pub fn build_router(state: AmpState) -> Router {
    Router::new()
        // Provider-aliased OpenAI / Anthropic endpoints.
        .route("/api/provider/:provider/v1/chat/completions", any(handle))
        .route("/api/provider/:provider/v1/completions", any(handle))
        .route("/api/provider/:provider/v1/responses", any(handle))
        .route("/api/provider/:provider/v1/messages", any(handle))
        .route(
            "/api/provider/:provider/v1/messages/count_tokens",
            any(handle),
        )
        .route("/api/provider/:provider/chat/completions", any(handle))
        .route("/api/provider/:provider/completions", any(handle))
        .route("/api/provider/:provider/responses", any(handle))
        // Gemini native: /v1beta/models/<model>:<action>
        .route("/api/provider/:provider/v1beta/models/*action", any(handle))
        .route(
            "/api/provider/:provider/v1beta1/models/*action",
            any(handle),
        )
        .route("/v1beta/models/*action", any(handle))
        .route("/v1beta1/models/*action", any(handle))
        // Gemini Vertex-AI / AMP-CLI variant: includes a `publishers/google/`
        // segment between the api version and `models/`. Observed shape:
        //   /api/provider/google/v1beta1/publishers/google/models/<model>:<action>
        // Without these routes the request misses the amp router entirely
        // and falls through to ampcode.com (BILLABLE).
        .route(
            "/api/provider/:provider/v1beta/publishers/google/models/*action",
            any(handle),
        )
        .route(
            "/api/provider/:provider/v1beta1/publishers/google/models/*action",
            any(handle),
        )
        .route("/v1beta/publishers/google/models/*action", any(handle))
        .route("/v1beta1/publishers/google/models/*action", any(handle))
        // Bare provider-less alias paths.
        .route("/v1/chat/completions", any(handle))
        .route("/v1/completions", any(handle))
        .route("/v1/responses", any(handle))
        .route("/v1/messages", any(handle))
        .route("/v1/messages/count_tokens", any(handle))
        .with_state(state)
}

/// Single dispatch handler for every amp route. Peeks the body for routing
/// metadata, asks the fallback brain where to send it, then dispatches via
/// either the streaming or buffered path. Emits structured INFO logs at
/// request start (with the routing decision and which path was chosen) and
/// at request end (with status + duration), so a one-line scan of run.log
/// shows what every Amp CLI call did.
async fn handle(State(state): State<AmpState>, req: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let method = parts.method.clone();

    // Phase 1: peek up to PEEK_LIMIT bytes off the body so we can route
    // without buffering huge payloads.
    let (prefix, tail) = match split_prefix(body, PEEK_LIMIT).await {
        Ok(p) => p,
        Err(e) => {
            warn!(method = %method, path = %path, error = %e, "amp router: failed to peek request body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Wrap the tail in an Option so the borrow checker can see that it's
    // consumed in exactly one of the three paths below (drain-on-terminal,
    // drain-on-buffered-dispatch, or stream-on-streaming-dispatch).
    let mut tail_opt: Option<Body> = Some(tail);

    let fallback = state.module.fallback.load();
    let mut decision = fallback.decide(&path, &prefix);

    // Phase 2: if the prefix didn't yield a model, drain the tail and retry.
    // The legacy path also relied on having the whole body in memory before
    // deciding, so this preserves identical behaviour for those payloads.
    let mut buffered_full: Option<Bytes> = None;
    let prefix_was_terminal = decision.requested_model.is_empty();
    if prefix_was_terminal {
        let tail_owned = tail_opt
            .take()
            .expect("tail not yet consumed at phase 2 entry");
        match drain_tail(tail_owned, MAX_BUFFERED_BYTES.saturating_sub(prefix.len())).await {
            Ok(rest) => {
                let mut combined = Vec::with_capacity(prefix.len() + rest.len());
                combined.extend_from_slice(&prefix);
                combined.extend_from_slice(&rest);
                let combined = Bytes::from(combined);
                decision = fallback.decide(&path, &combined);
                buffered_full = Some(combined);
            }
            Err(BufferError::TooLarge) => {
                warn!(method = %method, path = %path, "amp router: request body exceeds 16 MiB");
                return StatusCode::PAYLOAD_TOO_LARGE.into_response();
            }
            Err(BufferError::Read(e)) => {
                warn!(method = %method, path = %path, error = %e, "amp router: failed to read request body tail");
                return StatusCode::BAD_REQUEST.into_response();
            }
        }
    }

    // Decide upfront whether streaming dispatch is allowed. We need to know
    // this before we move `tail` so the request log can record it.
    let provider_for_decision: Option<Arc<Provider>> = match decision.route_type {
        AmpRouteType::CustomProvider | AmpRouteType::ModelMapping => {
            customproxy::global().provider_for_model(&decision.resolved_model)
        }
        _ => None,
    };
    let streaming = buffered_full.is_none()
        && provider_for_decision
            .as_deref()
            .map(|p| can_stream(&decision, p, &path, &method))
            .unwrap_or(false);

    // Per-request entry log — one line covers what the brain decided and
    // which dispatch path we'll take.
    info!(
        method = %method,
        path = %path,
        prefix_bytes = prefix.len(),
        buffered = buffered_full.is_some(),
        streaming = streaming,
        route = ?decision.route_type,
        requested_model = %decision.requested_model,
        resolved_model = %decision.resolved_model,
        provider = decision.provider_name.as_deref().unwrap_or("-"),
        gemini_translate = decision.gemini_translate,
        "amp router: request"
    );

    let response = match decision.route_type {
        AmpRouteType::CustomProvider | AmpRouteType::ModelMapping => {
            if streaming {
                // Streaming path: no body mutation needed, hand the upstream
                // a Body that emits the peek prefix first, then the tail
                // chunks as they arrive.
                let provider = provider_for_decision
                    .clone()
                    .expect("streaming gate already checked provider exists");
                let tail_owned = tail_opt
                    .take()
                    .expect("streaming branch invariant: tail still present");
                let combined = PrefixedBody::build(prefix.clone(), tail_owned);
                dispatch_streaming(&state.client, &provider, &decision, &parts, combined).await
            } else {
                // Buffered path: we already buffered if the prefix was
                // terminal, otherwise we need to drain now.
                let body_bytes = match buffered_full {
                    Some(b) => b,
                    None => {
                        let tail_owned = tail_opt.take().expect(
                            "buffered branch: tail still present when buffered_full is None",
                        );
                        match drain_tail(
                            tail_owned,
                            MAX_BUFFERED_BYTES.saturating_sub(prefix.len()),
                        )
                        .await
                        {
                            Ok(rest) => {
                                let mut combined = Vec::with_capacity(prefix.len() + rest.len());
                                combined.extend_from_slice(&prefix);
                                combined.extend_from_slice(&rest);
                                Bytes::from(combined)
                            }
                            Err(BufferError::TooLarge) => {
                                warn!(method = %method, path = %path, "amp router: request body exceeds 16 MiB");
                                return StatusCode::PAYLOAD_TOO_LARGE.into_response();
                            }
                            Err(BufferError::Read(e)) => {
                                warn!(method = %method, path = %path, error = %e, "amp router: failed to read request body tail");
                                return StatusCode::BAD_REQUEST.into_response();
                            }
                        }
                    }
                };
                // Sanitise (remove unsigned thinking blocks etc.) on the
                // buffered branch only — it requires the full body.
                let sanitised = super::response_rewriter::sanitize_amp_request_body(&body_bytes);
                dispatch_custom(
                    state.client.clone(),
                    &decision,
                    &parts,
                    Bytes::from(sanitised),
                    provider_for_decision.clone(),
                )
                .await
            }
        }
        AmpRouteType::AmpCredits | AmpRouteType::LocalProvider | AmpRouteType::NoProvider => {
            // The amp router intentionally doesn't own the ampcode.com
            // forwarder. Returning 404 lets the parent Router fallback
            // (`crate::proxy::forward`) take over — that handler itself
            // logs a BILLABLE warning so operators can spot credit drain.
            // Note: the body has already been (partially) consumed by the
            // peek, so the parent fallback will see an empty body. This
            // matches the prior behaviour because the prior handler also
            // 404'd here without forwarding.
            warn!(
                method = %method,
                path = %path,
                requested_model = %decision.requested_model,
                resolved_model = %decision.resolved_model,
                "amp router: no custom provider matched; falling through to ampcode.com (BILLABLE)"
            );
            StatusCode::NOT_FOUND.into_response()
        }
    };

    let elapsed_ms = started.elapsed().as_millis();
    let status = response.status();
    info!(
        method = %method,
        path = %path,
        status = status.as_u16(),
        elapsed_ms = elapsed_ms as u64,
        provider = decision.provider_name.as_deref().unwrap_or("-"),
        streaming = streaming,
        "amp router: response"
    );
    response
}

/// Should this request use the streaming dispatch path? Streaming is only
/// safe when no body mutation is needed — Gemini translation, model-name
/// rewrites, /messages SSE upgrades, and /responses-to-chat translation all
/// require holding the full body in memory.
fn can_stream(
    decision: &RouteDecision,
    provider: &Provider,
    path: &str,
    method: &http::Method,
) -> bool {
    if decision.gemini_translate {
        return false;
    }
    if decision.route_type == AmpRouteType::ModelMapping {
        // ModelMapping rewrites the body's `model` field before forwarding.
        return false;
    }
    if *method == http::Method::POST && path.ends_with("/messages") {
        // /messages requires `apply_messages_mutations` (override merge +
        // stream:true upgrade) which needs the full body.
        return false;
    }
    if *method == http::Method::POST && path.ends_with("/responses") && provider.responses_translate
    {
        return false;
    }
    if provider.model_aliases.is_empty() && decision.route_type != AmpRouteType::ModelMapping {
        return true;
    }
    false
}

/// Read up to `limit` bytes off `body` into a contiguous `Bytes` and return
/// the peeked prefix together with the remaining (possibly empty) tail Body.
/// The tail can be re-wrapped with [`PrefixedBody`] to reconstitute the full
/// payload for an upstream forward.
async fn split_prefix(body: Body, limit: usize) -> Result<(Bytes, Body), axum::Error> {
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(limit.min(8 * 1024));
    while buf.len() < limit {
        match stream.next().await {
            None => break,
            Some(Ok(chunk)) => {
                let remaining = limit - buf.len();
                if chunk.len() <= remaining {
                    buf.extend_from_slice(&chunk);
                } else {
                    // Hit the cap mid-chunk: take what fits and prepend the
                    // leftover slice back onto the tail stream.
                    buf.extend_from_slice(&chunk[..remaining]);
                    let leftover = chunk.slice(remaining..);
                    let head =
                        futures::stream::once(async move { Ok::<Bytes, axum::Error>(leftover) });
                    let tail = Body::from_stream(head.chain(stream));
                    return Ok((Bytes::from(buf), tail));
                }
            }
            Some(Err(e)) => return Err(e),
        }
    }
    let tail = Body::from_stream(stream);
    Ok((Bytes::from(buf), tail))
}

/// Errors that can occur while draining the tail of a peeked body into a
/// contiguous buffer.
#[derive(Debug)]
enum BufferError {
    /// The remaining body would exceed the configured cap.
    TooLarge,
    /// The underlying stream returned an error.
    Read(axum::Error),
}

/// Drain a tail Body into a `Bytes`, capped at `limit` bytes.
async fn drain_tail(body: Body, limit: usize) -> Result<Bytes, BufferError> {
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(BufferError::Read)?;
        if buf.len().saturating_add(chunk.len()) > limit {
            return Err(BufferError::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buf))
}

/// Dispatch a request via the buffered custom-provider path. Used when the
/// destination needs body mutation (Gemini translate, model-mapping rewrite,
/// /messages SSE upgrade, /responses translation).
async fn dispatch_custom(
    client: Client,
    decision: &RouteDecision,
    parts: &http::request::Parts,
    body: Bytes,
    provider_for_decision: Option<Arc<Provider>>,
) -> Response {
    let provider_name = match &decision.provider_name {
        Some(n) => n.clone(),
        None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Re-resolve the provider via the registry so we get the full Provider
    // (with request_overrides etc) rather than just the URL/key from the
    // decision.
    let provider = match provider_for_decision
        .or_else(|| customproxy::global().provider_for_model(&decision.resolved_model))
    {
        Some(p) => p,
        None => {
            warn!(model = %decision.resolved_model, "custom provider vanished between decide and dispatch");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let upstream_model = provider.upstream_model_for(&decision.resolved_model);

    if decision.gemini_translate {
        return match forward_gemini_translated(
            &provider,
            body,
            &upstream_model,
            &decision.requested_model,
            parts.uri.path(),
            &client,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(provider = %provider_name, error = %e, "gemini bridge failed");
                bad_gateway(format!("gemini bridge: {e}"))
            }
        };
    }

    // Standard custom-provider forward. If the body's model field needed rewriting
    // (mapping or provider alias case), do it here so we forward the upstream name.
    let body = if decision.route_type == AmpRouteType::ModelMapping
        || upstream_model != decision.resolved_model.trim()
    {
        rewrite_model_in_body(&body, &upstream_model)
    } else {
        body
    };

    let query = parts.uri.query();
    match forward_to_custom_provider(
        &provider,
        parts.method.clone(),
        parts.uri.path(),
        query,
        &parts.headers,
        body,
        &client,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(provider = %provider_name, error = %e, "custom provider forward failed");
            bad_gateway(format!("custom provider {provider_name}: {e}"))
        }
    }
}

/// Dispatch a request via the streaming custom-provider path. Used when the
/// router has determined that no body mutation is needed and the body can
/// be piped chunk-by-chunk to the upstream.
async fn dispatch_streaming(
    client: &Client,
    provider: &Provider,
    decision: &RouteDecision,
    parts: &http::request::Parts,
    body: Body,
) -> Response {
    let provider_name = decision
        .provider_name
        .clone()
        .unwrap_or_else(|| provider.name.clone());
    let query = parts.uri.query();
    match forward_to_custom_provider_streaming(
        provider,
        parts.method.clone(),
        parts.uri.path(),
        query,
        &parts.headers,
        body,
        client,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(provider = %provider_name, error = %e, "custom provider streaming forward failed");
            bad_gateway(format!("custom provider {provider_name}: {e}"))
        }
    }
}

/// Rewrite the `model` field on a JSON body in place, leaving non-JSON or
/// model-less bodies untouched.
fn rewrite_model_in_body(body: &Bytes, new_model: &str) -> Bytes {
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.clone(),
    };
    if let Some(obj) = v.as_object_mut() {
        if obj.contains_key("model") {
            obj.insert(
                "model".into(),
                serde_json::Value::String(new_model.to_string()),
            );
        }
    }
    Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec()))
}

/// Build a 502 Bad Gateway response with a JSON error envelope.
fn bad_gateway(msg: String) -> Response {
    let body = serde_json::json!({"error": "amp_router_error", "message": msg});
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amp::fallback_handlers::{extract_model_from_request, FallbackHandler};
    use crate::config::{AmpCode, CustomProvider, ModelAlias, ModelMapping};
    use bytes::Bytes;
    use serde_json::Map;
    use std::sync::Mutex;

    /// All registry-touching tests must serialise so they don't race the
    /// process-global customproxy registry.
    static SERIALISE_TESTS: Mutex<()> = Mutex::new(());

    fn provider(name: &str, models: &[&str]) -> CustomProvider {
        CustomProvider {
            name: name.into(),
            url: format!("https://{name}.example.com"),
            api_key: format!("key-{name}"),
            models: models.iter().map(|s| s.to_string()).collect(),
            model_aliases: Vec::new(),
            request_overrides: Map::new(),
            responses_translate: false,
            messages_translate: false,
        }
    }

    fn provider_with_alias(name: &str, alias: &str, upstream: &str) -> CustomProvider {
        CustomProvider {
            name: name.into(),
            url: format!("https://{name}.example.com"),
            api_key: format!("key-{name}"),
            models: Vec::new(),
            model_aliases: vec![ModelAlias {
                alias: alias.into(),
                upstream: upstream.into(),
            }],
            request_overrides: Map::new(),
            responses_translate: false,
            messages_translate: false,
        }
    }

    fn cfg(providers: Vec<CustomProvider>, mappings: Vec<ModelMapping>) -> AmpCode {
        AmpCode {
            upstream_url: "https://ampcode.com".into(),
            upstream_api_key: "amp-key".into(),
            model_mappings: mappings,
            force_model_mappings: false,
            custom_providers: providers,
            gemini_route_mode: String::new(),
            restrict_management_to_localhost: true,
        }
    }

    #[test]
    fn rewrite_model_in_body_replaces_field() {
        let body = Bytes::from_static(br#"{"model":"old","x":1}"#);
        let out = rewrite_model_in_body(&body, "new");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "new");
        assert_eq!(v["x"], 1);
    }

    #[test]
    fn provider_alias_routes_as_alias_but_rewrites_to_upstream_model() {
        let cfg = cfg(
            vec![provider_with_alias(
                "gw",
                "opus-deepseek-anthropic",
                "deepseek-v4-pro",
            )],
            vec![],
        );
        let _g = SERIALISE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        customproxy::global()
            .configure(&cfg.custom_providers)
            .expect("configure registry");
        let h = FallbackHandler::new(&cfg).expect("new handler");

        let body = Bytes::from_static(br#"{"model":"opus-deepseek-anthropic","messages":[]}"#);
        let d = h.decide("/v1/messages", &body);
        assert_eq!(d.route_type, AmpRouteType::CustomProvider);
        assert_eq!(d.resolved_model, "opus-deepseek-anthropic");

        let p = customproxy::global()
            .provider_for_model(&d.resolved_model)
            .expect("provider");
        let upstream_model = p.upstream_model_for(&d.resolved_model);
        let out = rewrite_model_in_body(&body, &upstream_model);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "deepseek-v4-pro");
    }

    #[test]
    fn rewrite_model_passes_through_invalid_json() {
        let body = Bytes::from_static(b"not json");
        let out = rewrite_model_in_body(&body, "new");
        assert_eq!(&out[..], b"not json");
    }

    #[tokio::test]
    async fn split_prefix_returns_full_body_when_under_limit() {
        let body = Body::from(Bytes::from_static(b"abc"));
        let (prefix, tail) = split_prefix(body, PEEK_LIMIT).await.unwrap();
        assert_eq!(&prefix[..], b"abc");
        let rest = drain_tail(tail, MAX_BUFFERED_BYTES)
            .await
            .unwrap_or_default();
        assert_eq!(rest.len(), 0);
    }

    #[tokio::test]
    async fn split_prefix_caps_at_limit_and_preserves_tail() {
        let payload = (b'a'..=b'z').cycle().take(40).collect::<Vec<u8>>();
        let body = Body::from(Bytes::from(payload.clone()));
        let (prefix, tail) = split_prefix(body, 10).await.unwrap();
        assert_eq!(prefix.len(), 10);
        assert_eq!(&prefix[..], &payload[..10]);
        let rest = drain_tail(tail, 1024).await.expect("drain ok");
        assert_eq!(rest.len(), 30);
        assert_eq!(&rest[..], &payload[10..]);
    }

    #[tokio::test]
    async fn peek_prefix_finds_model_in_first_chunk() {
        let h = {
            let _g = SERIALISE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            let cfg = cfg(vec![provider("gw", &["gpt-5.4"])], vec![]);
            customproxy::global()
                .configure(&cfg.custom_providers)
                .expect("configure registry");
            FallbackHandler::new(&cfg).expect("new handler")
        };

        let body_bytes = Bytes::from_static(
            br#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hi"}]}"#,
        );
        let body = Body::from(body_bytes.clone());

        let (prefix, _tail) = split_prefix(body, PEEK_LIMIT).await.unwrap();
        assert!(
            std::str::from_utf8(&prefix).unwrap().contains("model"),
            "prefix should contain `model` field"
        );
        let model =
            extract_model_from_request(&prefix, "/v1/chat/completions").expect("model in prefix");
        assert_eq!(model, "gpt-5.4");
        let decision = h.decide("/v1/chat/completions", &prefix);
        assert_eq!(decision.route_type, AmpRouteType::CustomProvider);
        assert_eq!(decision.resolved_model, "gpt-5.4");
    }

    #[tokio::test]
    async fn peek_prefix_falls_back_to_full_buffer_when_model_at_end() {
        let h = {
            let _g = SERIALISE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            let cfg = cfg(vec![provider("gw", &["gpt-5.4"])], vec![]);
            customproxy::global()
                .configure(&cfg.custom_providers)
                .expect("configure registry");
            FallbackHandler::new(&cfg).expect("new handler")
        };

        // Build a body whose `model` field lives well past PEEK_LIMIT. We
        // use a very long string field preceding the model field. The JSON
        // parser only sees a complete document with the full body, so the
        // prefix-only decision should yield empty model.
        let mut padding = String::with_capacity(PEEK_LIMIT * 2);
        padding.push('"');
        for _ in 0..(PEEK_LIMIT + 4096) {
            padding.push('x');
        }
        padding.push('"');
        let body_str = format!(r#"{{"junk":{padding},"model":"gpt-5.4"}}"#);
        let body_bytes = Bytes::from(body_str.into_bytes());
        assert!(body_bytes.len() > PEEK_LIMIT);
        let body = Body::from(body_bytes.clone());

        // Prefix-only: model isn't found because the JSON object is
        // truncated mid-string; extract_model_from_request returns None.
        let (prefix, tail) = split_prefix(body, PEEK_LIMIT).await.unwrap();
        assert_eq!(prefix.len(), PEEK_LIMIT);
        let prefix_decision = h.decide("/v1/chat/completions", &prefix);
        assert_eq!(
            prefix_decision.route_type,
            AmpRouteType::AmpCredits,
            "truncated prefix has no parseable model"
        );

        // Buffered fallback: drain the tail, glue, retry. Now we resolve.
        let rest = drain_tail(tail, MAX_BUFFERED_BYTES).await.unwrap();
        let mut combined = Vec::with_capacity(prefix.len() + rest.len());
        combined.extend_from_slice(&prefix);
        combined.extend_from_slice(&rest);
        let combined = Bytes::from(combined);
        assert_eq!(combined.len(), body_bytes.len());
        let full_decision = h.decide("/v1/chat/completions", &combined);
        assert_eq!(full_decision.route_type, AmpRouteType::CustomProvider);
        assert_eq!(full_decision.resolved_model, "gpt-5.4");
    }

    #[test]
    fn can_stream_blocks_messages_post() {
        let p = Provider {
            name: "p".into(),
            url: "http://x".into(),
            api_key: "k".into(),
            models: vec!["m".into()],
            model_aliases: std::collections::HashMap::new(),
            request_overrides: Map::new(),
            responses_translate: false,
            messages_translate: false,
        };
        let d = RouteDecision {
            route_type: AmpRouteType::CustomProvider,
            requested_model: "m".into(),
            resolved_model: "m".into(),
            provider_name: Some("p".into()),
            provider_url: Some("http://x".into()),
            provider_api_key: Some("k".into()),
            gemini_translate: false,
        };
        assert!(!can_stream(&d, &p, "/v1/messages", &http::Method::POST));
        assert!(can_stream(
            &d,
            &p,
            "/v1/chat/completions",
            &http::Method::POST
        ));
    }

    #[test]
    fn can_stream_blocks_model_mapping() {
        let p = Provider {
            name: "p".into(),
            url: "http://x".into(),
            api_key: "k".into(),
            models: vec!["m".into()],
            model_aliases: std::collections::HashMap::new(),
            request_overrides: Map::new(),
            responses_translate: false,
            messages_translate: false,
        };
        let d = RouteDecision {
            route_type: AmpRouteType::ModelMapping,
            requested_model: "a".into(),
            resolved_model: "m".into(),
            provider_name: Some("p".into()),
            provider_url: Some("http://x".into()),
            provider_api_key: Some("k".into()),
            gemini_translate: false,
        };
        assert!(!can_stream(
            &d,
            &p,
            "/v1/chat/completions",
            &http::Method::POST
        ));
    }

    #[test]
    fn can_stream_blocks_responses_translate() {
        let p = Provider {
            name: "p".into(),
            url: "http://x".into(),
            api_key: "k".into(),
            models: vec!["m".into()],
            model_aliases: std::collections::HashMap::new(),
            request_overrides: Map::new(),
            responses_translate: true,
            messages_translate: false,
        };
        let d = RouteDecision {
            route_type: AmpRouteType::CustomProvider,
            requested_model: "m".into(),
            resolved_model: "m".into(),
            provider_name: Some("p".into()),
            provider_url: Some("http://x".into()),
            provider_api_key: Some("k".into()),
            gemini_translate: false,
        };
        assert!(!can_stream(&d, &p, "/v1/responses", &http::Method::POST));
    }
}
