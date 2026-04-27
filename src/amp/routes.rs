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
use reqwest::Client;
use tracing::{info, warn};

use super::fallback_handlers::{AmpRouteType, RouteDecision};
use super::gemini_bridge::forward_gemini_translated;
use super::proxy::forward_to_custom_provider;
use crate::amp::AmpModule;
use crate::customproxy;

/// Shared state passed to every amp-routed handler.
#[derive(Clone)]
pub struct AmpState {
    pub module: Arc<AmpModule>,
    pub client: Client,
}

impl AmpState {
    pub fn new(module: Arc<AmpModule>) -> Self {
        let client = Client::builder()
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
        .route(
            "/api/provider/:provider/v1beta/models/*action",
            any(handle),
        )
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
        .route(
            "/v1beta/publishers/google/models/*action",
            any(handle),
        )
        .route(
            "/v1beta1/publishers/google/models/*action",
            any(handle),
        )
        // Bare provider-less alias paths.
        .route("/v1/chat/completions", any(handle))
        .route("/v1/completions", any(handle))
        .route("/v1/responses", any(handle))
        .route("/v1/messages", any(handle))
        .route("/v1/messages/count_tokens", any(handle))
        .with_state(state)
}

/// Single dispatch handler for every amp route. Reads the body, asks the
/// fallback brain where to send it, then forwards. Emits structured INFO
/// logs at request start (with the routing decision) and at request end
/// (with status + duration), so a one-line scan of run.log shows what every
/// Amp CLI call did.
async fn handle(State(state): State<AmpState>, req: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let method = parts.method.clone();

    // Buffer the body so we can both inspect it (for routing) and forward
    // it. 16 MiB matches the Go cap.
    let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            warn!(method = %method, path = %path, "amp router: request body exceeds 16 MiB");
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
    };

    // Sanitise the body before inspecting / forwarding so unsigned thinking
    // blocks don't trigger upstream 400s.
    let sanitised = super::response_rewriter::sanitize_amp_request_body(&body_bytes);
    let body_bytes = Bytes::from(sanitised);

    let fallback = state.module.fallback.load();
    let decision = fallback.decide(&path, &body_bytes);
    let body_len = body_bytes.len();

    // Per-request entry log — one line covers what the brain decided.
    info!(
        method = %method,
        path = %path,
        body_bytes = body_len,
        route = ?decision.route_type,
        requested_model = %decision.requested_model,
        resolved_model = %decision.resolved_model,
        provider = decision.provider_name.as_deref().unwrap_or("-"),
        gemini_translate = decision.gemini_translate,
        "amp router: request"
    );

    let response = match decision.route_type {
        AmpRouteType::CustomProvider | AmpRouteType::ModelMapping => {
            dispatch_custom(state.client.clone(), &decision, &parts, body_bytes).await
        }
        AmpRouteType::AmpCredits | AmpRouteType::LocalProvider | AmpRouteType::NoProvider => {
            // The amp router intentionally doesn't own the ampcode.com
            // forwarder. Returning 404 lets the parent Router fallback
            // (`crate::proxy::forward`) take over — that handler itself
            // logs a BILLABLE warning so operators can spot credit drain.
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
        "amp router: response"
    );
    response
}

async fn dispatch_custom(
    client: Client,
    decision: &RouteDecision,
    parts: &http::request::Parts,
    body: Bytes,
) -> Response {
    let provider_name = match &decision.provider_name {
        Some(n) => n.clone(),
        None => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Re-resolve the provider via the registry so we get the full Provider
    // (with request_overrides etc) rather than just the URL/key from the
    // decision.
    let provider = match customproxy::global().provider_for_model(&decision.resolved_model) {
        Some(p) => p,
        None => {
            warn!(model = %decision.resolved_model, "custom provider vanished between decide and dispatch");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    if decision.gemini_translate {
        return match forward_gemini_translated(
            &provider,
            body,
            &decision.resolved_model,
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

    // Standard custom-provider forward. If the body's model field needed
    // rewriting (mapping case), do it here so we forward the resolved name.
    let body = if decision.route_type == AmpRouteType::ModelMapping {
        rewrite_model_in_body(&body, &decision.resolved_model)
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
    use bytes::Bytes;

    #[test]
    fn rewrite_model_in_body_replaces_field() {
        let body = Bytes::from_static(br#"{"model":"old","x":1}"#);
        let out = rewrite_model_in_body(&body, "new");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "new");
        assert_eq!(v["x"], 1);
    }

    #[test]
    fn rewrite_model_passes_through_invalid_json() {
        let body = Bytes::from_static(b"not json");
        let out = rewrite_model_in_body(&body, "new");
        assert_eq!(&out[..], b"not json");
    }
}
