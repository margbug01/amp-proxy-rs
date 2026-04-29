use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::json;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::access_log::access_log_layer;
use crate::amp::routes::{build_router as build_amp_router, AmpState};
use crate::amp::AmpModule;
use crate::auth::{auth_middleware, ApiKeyValidator};
use crate::body_capture::{body_capture_layer, CaptureConfig};
use crate::config::Config;
use crate::customproxy;
use crate::error::{AppError, Result};
use crate::metrics::{metrics_middleware, Metrics, PROMETHEUS_CONTENT_TYPE};
use crate::proxy::{forward, AmpcodeProxy};

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub validator: ApiKeyValidator,
    pub ampcode: Option<AmpcodeProxy>,
    pub amp_module: Arc<AmpModule>,
    pub metrics: Arc<Metrics>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    status: &'static str,
    models: Vec<String>,
    providers: Vec<ProviderStatus>,
    metrics: crate::metrics::MetricsSnapshot,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderStatus {
    name: String,
    url: String,
    healthy: bool,
    consecutive_failures: u32,
    last_error: Option<String>,
}

pub fn build_app(cfg: &Config) -> Result<(Router, SharedState)> {
    let ampcode = if cfg.ampcode.upstream_url.trim().is_empty() {
        None
    } else {
        Some(
            AmpcodeProxy::new(&cfg.ampcode.upstream_url, &cfg.ampcode.upstream_api_key)
                .map_err(|e| AppError::Config(format!("ampcode.upstream-url: {e}")))?,
        )
    };

    let amp_module = Arc::new(
        AmpModule::new(&cfg.ampcode)
            .map_err(|e| AppError::Config(format!("amp module init: {e}")))?,
    );
    let metrics = Arc::new(Metrics::new());

    let state: SharedState = Arc::new(AppState {
        validator: ApiKeyValidator::new(cfg.api_keys.clone()),
        ampcode,
        amp_module: amp_module.clone(),
        metrics: metrics.clone(),
    });

    // Amp routes (model routing brain) handle every recognised provider /
    // OpenAI / Anthropic / Gemini path. Anything they don't claim falls
    // through to the ampcode.com transparent proxy.
    let amp_router: Router<()> = build_amp_router(AmpState::new(amp_module.clone()));

    let core: Router<()> = Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics_handler))
        .route("/admin/status", get(status_handler))
        .route("/admin/providers", get(providers_handler))
        .route(
            "/admin/providers/:name/recover",
            post(provider_recover_handler),
        )
        .fallback(forward)
        .with_state(state.clone());

    let mut app = core.merge(amp_router).layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ));

    app = app.layer(middleware::from_fn_with_state(
        metrics.clone(),
        metrics_middleware,
    ));

    // Optional access-log middleware with body model peek. Reads
    // `debug.access-log-model-peek` from config; when false the layer
    // still installs but its peek block is a no-op.
    app = app.layer(middleware::from_fn_with_state(
        cfg.debug.clone(),
        access_log_layer,
    ));

    // Optional body-capture middleware. Disabled when
    // `debug.capture-path-substring` is empty (the layer pre-checks and
    // skips work).
    let capture_substring = cfg.debug.capture_path_substring.trim().to_string();
    if !capture_substring.is_empty() {
        let dir = if cfg.debug.capture_dir.trim().is_empty() {
            PathBuf::from("./capture")
        } else {
            PathBuf::from(cfg.debug.capture_dir.trim())
        };
        info!(
            substring = %capture_substring,
            dir = %dir.display(),
            "body capture enabled"
        );
        app = app.layer(middleware::from_fn_with_state(
            CaptureConfig {
                path_substring: capture_substring,
                dir,
            },
            body_capture_layer,
        ));
    }

    let app = app.layer(TraceLayer::new_for_http());

    Ok((app, state))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "service": "amp-proxy",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn metrics_handler(State(state): State<SharedState>) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)],
        state.metrics.render_prometheus(),
    )
        .into_response()
}

async fn status_handler(State(state): State<SharedState>) -> Json<StatusResponse> {
    Json(StatusResponse {
        status: "ok",
        models: customproxy::global().model_ids(),
        providers: provider_statuses(),
        metrics: state.metrics.snapshot(),
    })
}

async fn providers_handler() -> Json<Vec<ProviderStatus>> {
    Json(provider_statuses())
}

async fn provider_recover_handler(Path(name): Path<String>) -> Json<serde_json::Value> {
    customproxy::global().record_success(&name);
    Json(json!({"status": "ok", "provider": name}))
}

fn provider_statuses() -> Vec<ProviderStatus> {
    customproxy::global()
        .health_snapshots()
        .into_iter()
        .map(|p| ProviderStatus {
            name: p.name,
            url: p.url,
            healthy: p.healthy,
            consecutive_failures: p.consecutive_failures,
            last_error: p.last_error,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request as HttpRequest, StatusCode};
    use tower::ServiceExt;

    fn test_config() -> Config {
        Config {
            host: "127.0.0.1".to_string(),
            port: 8317,
            api_keys: vec!["secret".to_string()],
            ampcode: Default::default(),
            debug: Default::default(),
        }
    }

    #[tokio::test]
    async fn metrics_endpoint_is_unauthenticated_and_skipped_by_metrics_middleware() {
        let (app, _state) = build_app(&test_config()).expect("build app");

        let metrics_req = HttpRequest::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let metrics_resp = app.clone().oneshot(metrics_req).await.unwrap();
        assert_eq!(metrics_resp.status(), StatusCode::OK);
        assert_eq!(
            metrics_resp.headers()[axum::http::header::CONTENT_TYPE],
            PROMETHEUS_CONTENT_TYPE
        );
        let body = to_bytes(metrics_resp.into_body(), 4096).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("requests_total 0"));
        assert!(body.contains("request_duration_seconds_count 0"));

        let health_req = HttpRequest::builder()
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let health_resp = app.clone().oneshot(health_req).await.unwrap();
        assert_eq!(health_resp.status(), StatusCode::OK);

        let metrics_req = HttpRequest::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let metrics_resp = app.oneshot(metrics_req).await.unwrap();
        let body = to_bytes(metrics_resp.into_body(), 4096).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("requests_total 1"));
        assert!(body.contains("request_duration_seconds_count 1"));
    }

    #[tokio::test]
    async fn admin_status_requires_auth_and_returns_json() {
        let (app, _state) = build_app(&test_config()).expect("build app");

        let unauth_req = HttpRequest::builder()
            .uri("/admin/status")
            .body(Body::empty())
            .unwrap();
        let unauth_resp = app.clone().oneshot(unauth_req).await.unwrap();
        assert_eq!(unauth_resp.status(), StatusCode::UNAUTHORIZED);

        let auth_req = HttpRequest::builder()
            .uri("/admin/status")
            .header("x-api-key", "secret")
            .body(Body::empty())
            .unwrap();
        let auth_resp = app.oneshot(auth_req).await.unwrap();
        assert_eq!(auth_resp.status(), StatusCode::OK);
        let body = to_bytes(auth_resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "ok");
        assert!(body["providers"].is_array());
        assert!(body["models"].is_array());
        assert_eq!(body["metrics"]["requestsTotal"], 1);
    }
}
