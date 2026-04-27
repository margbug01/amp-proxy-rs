//! Amp module: routing brain for amp-proxy.
//!
//! This module ties together the customproxy registry, the model mapper, the
//! Gemini translation bridge, and the ampcode.com fallback proxy. It also
//! owns the route table mounted by the server.
//!
//! Phase 1A owns: `secret`, `model_mapping`.
//! Phase 2B owns: `fallback_handlers`, `gemini_bridge`, `response_rewriter`,
//! `routes`, `proxy`, and the `AmpModule` aggregate below.

pub mod fallback_handlers;
pub mod gemini_bridge;
pub mod model_mapping;
pub mod proxy;
pub mod response_rewriter;
pub mod routes;
pub mod secret;

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::AmpCode;

/// Module-wide state for amp routing. Hot-reloadable via [`on_config_updated`].
///
/// The Go version exposes a `*AmpModule` that wires the gin engine and lazy
/// upstream proxy. In the Rust port the upstream proxy is owned by
/// [`crate::server::AppState`] and the Module is reduced to the small slice
/// of state the route handlers need to read on every request: the snapshotted
/// AmpCode config and the compiled fallback handler. Both are wrapped in
/// `ArcSwap` so config reloads can swap them atomically without taking a
/// lock on the request hot-path.
pub struct AmpModule {
    pub config: ArcSwap<AmpCode>,
    pub fallback: ArcSwap<fallback_handlers::FallbackHandler>,
}

impl AmpModule {
    /// Build a new module from the current config. Also installs the
    /// global custom-provider registry so [`crate::customproxy::global()`]
    /// returns the right snapshot.
    pub fn new(cfg: &AmpCode) -> anyhow::Result<Self> {
        // Install custom providers into the global registry. Errors here
        // are surfaced as anyhow::Error rather than a string so the caller
        // can `?` them.
        crate::customproxy::global()
            .configure(&cfg.custom_providers)
            .map_err(|e| anyhow::anyhow!("configure custom providers: {e}"))?;

        let fallback = fallback_handlers::FallbackHandler::new(cfg)?;
        Ok(Self {
            config: ArcSwap::from_pointee(cfg.clone()),
            fallback: ArcSwap::from_pointee(fallback),
        })
    }

    /// Re-apply config to the live module. Mirrors the partial-reload contract
    /// of the Go `AmpModule.OnConfigUpdated`: the registry is reconfigured and
    /// the fallback handler is rebuilt with the new mapper / mode flags.
    pub fn on_config_updated(&self, cfg: &AmpCode) -> anyhow::Result<()> {
        crate::customproxy::global()
            .configure(&cfg.custom_providers)
            .map_err(|e| anyhow::anyhow!("configure custom providers: {e}"))?;

        let fallback = fallback_handlers::FallbackHandler::new(cfg)?;
        self.fallback.store(Arc::new(fallback));
        self.config.store(Arc::new(cfg.clone()));
        Ok(())
    }
}
