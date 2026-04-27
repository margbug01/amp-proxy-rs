use std::path::PathBuf;
use std::sync::Arc;

use axum::{middleware, routing::get, Json, Router};
use serde_json::json;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::access_log::access_log_layer;
use crate::amp::routes::{build_router as build_amp_router, AmpState};
use crate::amp::AmpModule;
use crate::auth::{auth_middleware, ApiKeyValidator};
use crate::body_capture::{body_capture_layer, CaptureConfig};
use crate::config::Config;
use crate::error::{AppError, Result};
use crate::proxy::{forward, AmpcodeProxy};

pub type SharedState = Arc<AppState>;

pub struct AppState {
    pub validator: ApiKeyValidator,
    pub ampcode: Option<AmpcodeProxy>,
    pub amp_module: Arc<AmpModule>,
}

pub fn build_app(cfg: &Config) -> Result<(Router, SharedState)> {
    let ampcode = if cfg.ampcode.upstream_url.trim().is_empty() {
        None
    } else {
        Some(
            AmpcodeProxy::new(&cfg.ampcode.upstream_url, &cfg.ampcode.upstream_api_key).map_err(
                |e| AppError::Config(format!("ampcode.upstream-url: {e}")),
            )?,
        )
    };

    let amp_module = Arc::new(
        AmpModule::new(&cfg.ampcode)
            .map_err(|e| AppError::Config(format!("amp module init: {e}")))?,
    );

    let state: SharedState = Arc::new(AppState {
        validator: ApiKeyValidator::new(cfg.api_keys.clone()),
        ampcode,
        amp_module: amp_module.clone(),
    });

    // Amp routes (model routing brain) handle every recognised provider /
    // OpenAI / Anthropic / Gemini path. Anything they don't claim falls
    // through to the ampcode.com transparent proxy.
    let amp_router: Router<()> = build_amp_router(AmpState::new(amp_module.clone()));

    let core: Router<()> = Router::new()
        .route("/healthz", get(healthz))
        .fallback(forward)
        .with_state(state.clone());

    let mut app = core
        .merge(amp_router)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
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
    Json(json!({"status": "ok"}))
}
