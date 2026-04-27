//! Custom upstream provider routing.
//!
//! Ported from `internal/customproxy/customproxy.go`. The registry is a
//! process-wide singleton so that hot-reloads in `amp.go` `OnConfigUpdated`
//! can swap the active provider set atomically; in Rust we use
//! [`arc_swap::ArcSwap`] for the same effect with cheap reads.
//!
//! Translator submodules (`gemini_translator`, `responses_translator`,
//! `responses_stream_translator`) and the request-forwarder live in
//! sibling files owned by Phase 2A.

pub mod extract_leaf;
pub mod retry_transport;
pub mod sse_messages_collapser;
pub mod sse_rewriter;
// Stubs for Phase 2A — declared so cargo check passes; agents will fill them.
pub mod gemini_stream_translator;
pub mod gemini_translator;
pub mod messages_translator;
pub mod responses_stream_translator;
pub mod responses_translator;

pub use extract_leaf::extract_leaf;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use arc_swap::ArcSwap;

use crate::config::CustomProvider as ProviderCfg;

/// A single configured upstream endpoint.
#[derive(Debug, Clone)]
pub struct Provider {
    /// Display name from config.
    pub name: String,
    /// Base URL of the upstream endpoint.
    pub url: String,
    /// API key forwarded as `Authorization: Bearer ...` to the upstream.
    pub api_key: String,
    /// Models this provider serves (preserves original case for display).
    pub models: Vec<String>,
    /// Shallow-merged JSON patch applied to every POST `/messages` body.
    pub request_overrides: serde_json::Map<String, serde_json::Value>,
    /// When true, OpenAI Responses requests are translated to/from
    /// chat/completions for this provider.
    pub responses_translate: bool,
    /// When true, requests on the Gemini bridge path are translated to
    /// Anthropic Messages format (`/v1/messages`) instead of OpenAI Responses.
    pub messages_translate: bool,
}

/// Inner snapshot held inside the [`Registry`]'s [`ArcSwap`]. Reads see a
/// consistent snapshot for the duration of an `Arc` clone.
#[derive(Default)]
struct RegistryInner {
    by_model: HashMap<String, Vec<Arc<Provider>>>, // key = lowercase trimmed model
    providers: Vec<Arc<Provider>>,
    models: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProviderHealth {
    consecutive_failures: u32,
    healthy: bool,
    last_error: Option<String>,
}

impl Default for ProviderHealth {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            healthy: true,
            last_error: None,
        }
    }
}

/// Public view of a provider's current health state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub name: String,
    pub url: String,
    pub api_key: String,
    pub healthy: bool,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
}

/// Process-wide custom provider registry. Cheap to read concurrently;
/// writers atomically swap a fresh inner snapshot via [`ArcSwap`].
pub struct Registry {
    inner: ArcSwap<RegistryInner>,
    health: RwLock<HashMap<String, ProviderHealth>>,
}

impl Registry {
    /// Creates an empty registry with no providers.
    pub fn new() -> Self {
        Self {
            inner: ArcSwap::from_pointee(RegistryInner::default()),
            health: RwLock::new(HashMap::new()),
        }
    }

    /// Replaces the active set of providers atomically. Returns the first
    /// error message encountered if any provider config is invalid; on
    /// error the existing registry contents are preserved unchanged.
    pub fn configure(&self, cfgs: &[ProviderCfg]) -> Result<(), String> {
        let mut by_model: HashMap<String, Vec<Arc<Provider>>> =
            HashMap::with_capacity(cfgs.len() * 2);
        let mut providers: Vec<Arc<Provider>> = Vec::with_capacity(cfgs.len());
        let mut active_models: Vec<String> = Vec::with_capacity(cfgs.len() * 2);
        let mut active_model_keys: HashSet<String> = HashSet::with_capacity(cfgs.len() * 2);
        let mut seen_names: HashSet<String> = HashSet::with_capacity(cfgs.len());

        for (i, c) in cfgs.iter().enumerate() {
            let name = c.name.trim();
            let url = c.url.trim();

            if name.is_empty() || url.is_empty() || c.models.is_empty() {
                return Err(format!(
                    "custom provider {i} is invalid: name, url, and models are required"
                ));
            }
            if !seen_names.insert(name.to_lowercase()) {
                return Err(format!(
                    "custom provider {i} duplicates provider name {name:?}"
                ));
            }

            let provider = Arc::new(Provider {
                name: name.to_string(),
                url: url.to_string(),
                api_key: c.api_key.clone(),
                models: c.models.clone(),
                request_overrides: c.request_overrides.clone(),
                responses_translate: c.responses_translate,
                messages_translate: c.messages_translate,
            });
            providers.push(provider.clone());

            for model in &c.models {
                let trimmed = model.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let key = model_lookup_key(trimmed);
                by_model
                    .entry(key.clone())
                    .or_default()
                    .push(provider.clone());
                if active_model_keys.insert(key) {
                    active_models.push(trimmed.to_string());
                }
            }
        }

        let active_names: HashSet<String> = providers.iter().map(|p| p.name.clone()).collect();
        {
            let mut health = self.health.write().unwrap_or_else(|e| e.into_inner());
            health.retain(|name, _| active_names.contains(name));
            for name in active_names {
                health.entry(name).or_default();
            }
        }

        self.inner.store(Arc::new(RegistryInner {
            by_model,
            providers,
            models: active_models,
        }));
        Ok(())
    }

    /// Returns the provider serving `model`, or None if unregistered.
    /// Lookup is case-insensitive and falls back to a thinking-suffix-stripped
    /// form ("model-x(high)" -> "model-x") for compatibility with Amp CLI's
    /// resolved model names.
    pub fn provider_for_model(&self, model: &str) -> Option<Arc<Provider>> {
        let model = model.trim();
        if model.is_empty() {
            return None;
        }
        let snapshot = self.inner.load();
        if let Some(p) = self.choose_provider(snapshot.by_model.get(&model_lookup_key(model))) {
            return Some(p);
        }
        let base = strip_thinking_suffix(model);
        if base != model {
            if let Some(p) = self.choose_provider(snapshot.by_model.get(&model_lookup_key(base))) {
                return Some(p);
            }
        }
        None
    }

    fn choose_provider(&self, providers: Option<&Vec<Arc<Provider>>>) -> Option<Arc<Provider>> {
        let providers = providers?;
        if providers.is_empty() {
            return None;
        }
        let health = self.health.read().unwrap_or_else(|e| e.into_inner());
        providers
            .iter()
            .find(|p| health.get(&p.name).map(|h| h.healthy).unwrap_or(true))
            .cloned()
            .or_else(|| providers.first().cloned())
    }

    /// Returns a provider by its unique configured name.
    pub fn provider_for_name(&self, name: &str) -> Option<Arc<Provider>> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let snapshot = self.inner.load();
        snapshot.providers.iter().find(|p| p.name == name).cloned()
    }

    /// Record a successful upstream send/connection for `name` and mark it healthy.
    pub fn record_success(&self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let mut health = self.health.write().unwrap_or_else(|e| e.into_inner());
        let state = health.entry(name.to_string()).or_default();
        state.consecutive_failures = 0;
        state.healthy = true;
        state.last_error = None;
    }

    /// Record a failed upstream send/connection for `name`.
    /// The provider becomes unhealthy after two consecutive failures.
    pub fn record_failure(&self, name: &str, error: impl Into<String>) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let mut health = self.health.write().unwrap_or_else(|e| e.into_inner());
        let state = health.entry(name.to_string()).or_default();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures >= 2 {
            state.healthy = false;
        }
        state.last_error = Some(error.into());
    }

    /// Returns current provider health in configured provider order.
    pub fn health_snapshots(&self) -> Vec<HealthSnapshot> {
        let snapshot = self.inner.load();
        let health = self.health.read().unwrap_or_else(|e| e.into_inner());
        snapshot
            .providers
            .iter()
            .map(|p| {
                let state = health.get(&p.name).cloned().unwrap_or_default();
                HealthSnapshot {
                    name: p.name.clone(),
                    url: p.url.clone(),
                    api_key: p.api_key.clone(),
                    healthy: state.healthy,
                    consecutive_failures: state.consecutive_failures,
                    last_error: state.last_error,
                }
            })
            .collect()
    }

    /// Returns the registered custom provider model IDs in deterministic
    /// (sorted) order.
    pub fn model_ids(&self) -> Vec<String> {
        let snapshot = self.inner.load();
        let mut out: Vec<String> = snapshot.models.clone();
        out.sort();
        out
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the process-wide singleton registry. Matches Go's
/// `globalRegistry` / `GetGlobal` semantics.
pub fn global() -> &'static Registry {
    static GLOBAL: OnceLock<Registry> = OnceLock::new();
    GLOBAL.get_or_init(Registry::new)
}

/// Lowercases and trims `model` for case-insensitive map lookup. Mirrors
/// Go's `modelLookupKey`.
pub fn model_lookup_key(model: &str) -> String {
    model.trim().to_lowercase()
}

/// Removes a trailing thinking suffix of the form `(content)` from a model
/// name. Mirrors Go's `stripThinkingSuffix`: returns the input unchanged if
/// the string does not end with `)` or contains no `(`.
pub fn strip_thinking_suffix(model: &str) -> &str {
    let last_open = match model.rfind('(') {
        Some(i) if i > 0 => i,
        _ => return model,
    };
    if !model.ends_with(')') {
        return model;
    }
    &model[..last_open]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn cfg(name: &str, url: &str, models: &[&str]) -> ProviderCfg {
        ProviderCfg {
            name: name.into(),
            url: url.into(),
            api_key: "k".into(),
            models: models.iter().map(|s| s.to_string()).collect(),
            request_overrides: Map::new(),
            responses_translate: false,
            messages_translate: false,
        }
    }

    #[test]
    fn configure_and_lookup_case_insensitive() {
        let r = Registry::new();
        r.configure(&[cfg("a", "https://a.example.com", &["GPT-5"])])
            .unwrap();
        assert!(r.provider_for_model("gpt-5").is_some());
        assert!(r.provider_for_model("GPT-5").is_some());
        assert!(r.provider_for_model("  gpt-5  ").is_some());
        assert!(r.provider_for_model("missing").is_none());
    }

    #[test]
    fn thinking_suffix_fallback() {
        let r = Registry::new();
        r.configure(&[cfg("a", "https://a.example.com", &["gpt-5"])])
            .unwrap();
        assert!(r.provider_for_model("gpt-5(high)").is_some());
        assert!(r.provider_for_model("gpt-5(xhigh)").is_some());
        assert!(r.provider_for_model("gpt-5-different").is_none());
    }

    #[test]
    fn duplicate_model_can_fail_over_to_second_after_first_unhealthy() {
        let r = Registry::new();
        r.configure(&[
            cfg("a", "https://a.example.com", &["gpt-5"]),
            cfg("b", "https://b.example.com", &["GPT-5"]),
        ])
        .unwrap();
        assert_eq!(r.provider_for_model("gpt-5").unwrap().name, "a");

        r.record_failure("a", "connect failed once");
        assert_eq!(
            r.provider_for_model("gpt-5").unwrap().name,
            "a",
            "one failure should not mark unhealthy"
        );
        r.record_failure("a", "connect failed twice");
        assert_eq!(r.provider_for_model("gpt-5").unwrap().name, "b");

        let snapshots = r.health_snapshots();
        let a = snapshots.iter().find(|s| s.name == "a").unwrap();
        assert!(!a.healthy);
        assert_eq!(a.consecutive_failures, 2);
        assert_eq!(a.last_error.as_deref(), Some("connect failed twice"));
    }

    #[test]
    fn record_success_recovers_primary() {
        let r = Registry::new();
        r.configure(&[
            cfg("a", "https://a.example.com", &["gpt-5"]),
            cfg("b", "https://b.example.com", &["gpt-5"]),
        ])
        .unwrap();
        r.record_failure("a", "first");
        r.record_failure("a", "second");
        assert_eq!(r.provider_for_model("gpt-5").unwrap().name, "b");

        r.record_success("a");
        assert_eq!(r.provider_for_model("gpt-5").unwrap().name, "a");

        let snapshots = r.health_snapshots();
        let a = snapshots.iter().find(|s| s.name == "a").unwrap();
        assert!(a.healthy);
        assert_eq!(a.consecutive_failures, 0);
        assert_eq!(a.last_error, None);
    }

    #[test]
    fn rejects_invalid_provider() {
        let r = Registry::new();
        // Empty name.
        let err = r
            .configure(&[ProviderCfg {
                name: "".into(),
                url: "https://x".into(),
                api_key: "k".into(),
                models: vec!["m".into()],
                request_overrides: Map::new(),
                responses_translate: false,
                messages_translate: false,
            }])
            .unwrap_err();
        assert!(err.contains("invalid"));

        // Empty models.
        let err = r
            .configure(&[ProviderCfg {
                name: "n".into(),
                url: "https://x".into(),
                api_key: "k".into(),
                models: vec![],
                request_overrides: Map::new(),
                responses_translate: false,
                messages_translate: false,
            }])
            .unwrap_err();
        assert!(err.contains("invalid"));
    }

    #[test]
    fn rejects_duplicate_provider_name() {
        let r = Registry::new();
        let err = r
            .configure(&[
                cfg("a", "https://a.example.com", &["gpt-5"]),
                cfg("a", "https://b.example.com", &["gpt-5-mini"]),
            ])
            .unwrap_err();
        assert!(err.contains("duplicates provider name"));
    }

    #[test]
    fn model_ids_sorted() {
        let r = Registry::new();
        r.configure(&[
            cfg("a", "https://a.example.com", &["m-z", "m-a"]),
            cfg("b", "https://b.example.com", &["m-m"]),
        ])
        .unwrap();
        assert_eq!(r.model_ids(), vec!["m-a", "m-m", "m-z"]);
    }

    #[test]
    fn strip_thinking_suffix_basic() {
        assert_eq!(strip_thinking_suffix("gpt-5(high)"), "gpt-5");
        assert_eq!(strip_thinking_suffix("gpt-5(xhigh)"), "gpt-5");
        assert_eq!(strip_thinking_suffix("gpt-5(16384)"), "gpt-5");
        assert_eq!(strip_thinking_suffix("gpt-5"), "gpt-5");
        // No closing paren -> unchanged.
        assert_eq!(strip_thinking_suffix("gpt-5(high"), "gpt-5(high");
        // Leading paren -> unchanged.
        assert_eq!(strip_thinking_suffix("(weird)"), "(weird)");
    }

    #[test]
    fn model_lookup_key_normalizes() {
        assert_eq!(model_lookup_key("  GPT-5  "), "gpt-5");
        assert_eq!(model_lookup_key("Claude-Opus-4.6"), "claude-opus-4.6");
    }

    #[test]
    fn global_returns_same_instance() {
        let a = global() as *const Registry;
        let b = global() as *const Registry;
        assert_eq!(a, b);
    }
}
