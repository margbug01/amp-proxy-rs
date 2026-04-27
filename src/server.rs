use std::sync::Arc;

use axum::{middleware, routing::get, Json, Router};
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::amp::routes::{build_router as build_amp_router, AmpState};
use crate::amp::AmpModule;
use crate::auth::{auth_middleware, ApiKeyValidator};
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

    let app = core
        .merge(amp_router)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(TraceLayer::new_for_http());

    Ok((app, state))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}
