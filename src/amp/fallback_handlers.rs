//! Routing brain for the amp module.
//!
//! Ported from `internal/amp/fallback_handlers.go`. This module is body-aware:
//! given an incoming request path and its already-buffered body, it decides
//! whether the request should be served by a custom upstream provider, by the
//! ampcode.com transparent proxy, or rejected outright.
//!
//! The Rust port is split a little differently from the Go original. The Go
//! `WrapHandler` does both routing *and* dispatch; in Rust we keep the routing
//! decision pure (returns a `RouteDecision`) and let `routes.rs` perform the
//! dispatch. That separation makes the logic easy to unit-test without
//! standing up a full axum stack.

use std::sync::Arc;

use serde_json::Value;

use crate::amp::model_mapping::ModelMapper;
use crate::config::AmpCode;
use crate::customproxy::{self, Provider, Registry};
use crate::thinking::parse_suffix;

/// Where the amp module decided to forward a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmpRouteType {
    /// Served by a local OAuth provider (kept for parity with the Go enum;
    /// not currently used in the Rust port because amp-proxy-rs doesn't ship
    /// SDK-managed local providers — every "local" call goes through a
    /// `CustomProvider` instead).
    LocalProvider,
    /// Model name was rewritten via `model-mappings` before resolving.
    ModelMapping,
    /// Forwarded to ampcode.com (consumes Amp credits).
    AmpCredits,
    /// No provider available and no fallback configured.
    NoProvider,
    /// Served by a configured custom provider.
    CustomProvider,
}

/// Outcome of [`FallbackHandler::decide`].
#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub route_type: AmpRouteType,
    /// What the client asked for, before any mapping.
    pub requested_model: String,
    /// What we will actually send upstream (after mapping + suffix logic).
    pub resolved_model: String,
    /// Display name of the matched custom provider, if any.
    pub provider_name: Option<String>,
    /// Base URL of the matched custom provider, if any.
    pub provider_url: Option<String>,
    /// Bearer token to forward to the custom provider, if any.
    pub provider_api_key: Option<String>,
    /// True iff this is a Gemini path (`:generateContent`) routed to a
    /// custom provider with `gemini-route-mode: translate`.
    pub gemini_translate: bool,
}

/// Compiled, hot-swappable routing state.
pub struct FallbackHandler {
    pub model_mapper: Option<ModelMapper>,
    pub force_model_mappings: bool,
    pub gemini_route_mode: String,
    pub registry: &'static Registry,
}

impl FallbackHandler {
    /// Build a fresh handler from the current `[ampcode]` config block.
    pub fn new(cfg: &AmpCode) -> anyhow::Result<Self> {
        let mapper = if cfg.model_mappings.is_empty() {
            None
        } else {
            Some(
                ModelMapper::new(&cfg.model_mappings)
                    .map_err(|e| anyhow::anyhow!("compile ampcode.model-mappings regex: {e}"))?,
            )
        };
        Ok(Self {
            model_mapper: mapper,
            force_model_mappings: cfg.force_model_mappings,
            gemini_route_mode: cfg.gemini_route_mode.trim().to_lowercase(),
            registry: customproxy::global(),
        })
    }

    /// Inspect path + body to decide where to send the request. Pure function
    /// — no I/O, no async — so it can be called from any handler context.
    pub fn decide(&self, path: &str, body: &[u8]) -> RouteDecision {
        // 1. Extract model from the request (body first, then URL path).
        let requested_model = extract_model_from_request(body, path).unwrap_or_default();

        if requested_model.is_empty() {
            // Without a model name we cannot route. Mirror Go behaviour:
            // hand the request to ampcode.com so it can return a clean
            // upstream error rather than a synthesized 400.
            return RouteDecision {
                route_type: AmpRouteType::AmpCredits,
                requested_model: String::new(),
                resolved_model: String::new(),
                provider_name: None,
                provider_url: None,
                provider_api_key: None,
                gemini_translate: false,
            };
        }

        // Normalize: strip a trailing thinking suffix like "(high)" so the
        // registry lookup matches the raw model id, but keep the suffix to
        // re-glue onto a mapped target if needed.
        let suffix = parse_suffix(&requested_model);
        let normalized = suffix.model_name.clone();
        let thinking_suffix = if suffix.has_suffix {
            // Reconstruct exactly the original "(...)" segment.
            let raw = &requested_model[normalized.len()..];
            raw.to_string()
        } else {
            String::new()
        };

        // 2. Resolve via mapping (if configured + applicable).
        let mapped = self.resolve_mapped_model(&requested_model, &normalized, &thinking_suffix);

        // Two-pass resolution mirroring Go behavior. We track *both* the
        // mapped result and which model name to look up in the registry.
        let (lookup_model, used_mapping) = if self.force_model_mappings {
            // Force mode: mapping wins if it produced something *and* the
            // registry actually has a provider for the mapped name.
            if let Some(ref m) = mapped {
                if self.registry.provider_for_model(m).is_some() {
                    (m.clone(), true)
                } else {
                    (requested_model.clone(), false)
                }
            } else {
                (requested_model.clone(), false)
            }
        } else {
            // Default mode: try the original model first; fall back to the
            // mapped name only if nothing claims the original.
            if self.registry.provider_for_model(&requested_model).is_some() {
                (requested_model.clone(), false)
            } else if let Some(ref m) = mapped {
                if self.registry.provider_for_model(m).is_some() {
                    (m.clone(), true)
                } else {
                    (requested_model.clone(), false)
                }
            } else {
                (requested_model.clone(), false)
            }
        };

        // 3. Look up the custom provider for the resolved model.
        let provider: Option<Arc<Provider>> = self.registry.provider_for_model(&lookup_model);

        // 4. Decide.
        if let Some(p) = provider {
            // Gemini native path? Custom providers don't speak generateContent
            // natively; respect gemini-route-mode. Both `:generateContent`
            // (non-streaming) and `:streamGenerateContent` are translated when
            // mode==translate; the bridge dispatches to the right translator.
            if is_google_native_path(path) {
                let translatable =
                    path.ends_with(":generateContent") || path.ends_with(":streamGenerateContent");
                if self.gemini_route_mode == "translate" && translatable {
                    return RouteDecision {
                        route_type: AmpRouteType::CustomProvider,
                        requested_model,
                        resolved_model: lookup_model,
                        provider_name: Some(p.name.clone()),
                        provider_url: Some(p.url.clone()),
                        provider_api_key: Some(p.api_key.clone()),
                        gemini_translate: true,
                    };
                }
                // Translate mode disabled or non-content Gemini path: fall
                // through to ampcode.com.
                return RouteDecision {
                    route_type: AmpRouteType::AmpCredits,
                    requested_model,
                    resolved_model: lookup_model,
                    provider_name: None,
                    provider_url: None,
                    provider_api_key: None,
                    gemini_translate: false,
                };
            }

            let route_type = if used_mapping {
                AmpRouteType::ModelMapping
            } else {
                AmpRouteType::CustomProvider
            };
            return RouteDecision {
                route_type,
                requested_model,
                resolved_model: lookup_model,
                provider_name: Some(p.name.clone()),
                provider_url: Some(p.url.clone()),
                provider_api_key: Some(p.api_key.clone()),
                gemini_translate: false,
            };
        }

        // 5. No provider found: fall back to ampcode.com.
        RouteDecision {
            route_type: AmpRouteType::AmpCredits,
            requested_model,
            resolved_model: lookup_model,
            provider_name: None,
            provider_url: None,
            provider_api_key: None,
            gemini_translate: false,
        }
    }

    /// Apply the model mapper, preserving thinking suffix when the target
    /// doesn't already specify one. Mirrors Go's `resolveMappedModel` with
    /// the exception that we don't consult `util.GetProviderName` here —
    /// that check moves up into the caller (it queries the customproxy
    /// registry instead).
    fn resolve_mapped_model(
        &self,
        full: &str,
        normalized: &str,
        thinking_suffix: &str,
    ) -> Option<String> {
        let mapper = self.model_mapper.as_ref()?;
        let mut mapped = mapper.apply(full).or_else(|| mapper.apply(normalized))?;
        mapped = mapped.trim().to_string();
        if mapped.is_empty() {
            return None;
        }
        if !thinking_suffix.is_empty() {
            // Glue the original suffix onto the mapped model unless the
            // mapping already supplied its own.
            if !parse_suffix(&mapped).has_suffix {
                mapped.push_str(thinking_suffix);
            }
        }
        Some(mapped)
    }
}

/// Try to read the `model` field out of a JSON request body, falling back to
/// extracting the model from a Gemini-style URL path.
pub fn extract_model_from_request(body: &[u8], path: &str) -> Option<String> {
    if !body.is_empty() {
        if let Ok(v) = serde_json::from_slice::<Value>(body) {
            if let Some(s) = v.get("model").and_then(|m| m.as_str()) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    extract_gemini_model_from_path(path)
}

/// Extract the model id from a Google-shaped URL like
/// `/v1beta/models/<model>:generateContent` or
/// `/api/provider/google/v1beta/models/<model>:streamGenerateContent`.
pub fn extract_gemini_model_from_path(path: &str) -> Option<String> {
    let idx = path.find("/models/")?;
    let rest = &path[idx + "/models/".len()..];
    let model = match rest.find(':') {
        Some(c) if c > 0 => &rest[..c],
        Some(_) => return None,
        None => rest,
    };
    let model = model.trim_end_matches('/');
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

/// Reports whether a path is a Google v1beta / v1beta1 native generateContent
/// shape that custom providers can't serve directly.
pub fn is_google_native_path(p: &str) -> bool {
    if p.contains("/v1beta1/") || p.contains("/v1beta/") {
        return true;
    }
    p.ends_with(":generateContent") || p.ends_with(":streamGenerateContent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AmpCode, CustomProvider, ModelMapping};
    use serde_json::Map;
    use std::sync::Mutex;

    /// All FallbackHandler tests touch the process-wide customproxy
    /// registry, so they must be serialised. cargo runs tests in parallel
    /// by default; without this mutex one test's `configure` call would
    /// race with another's `decide` lookup.
    static SERIALISE_TESTS: Mutex<()> = Mutex::new(());

    fn provider(name: &str, models: &[&str]) -> CustomProvider {
        CustomProvider {
            name: name.into(),
            url: format!("https://{name}.example.com"),
            api_key: format!("key-{name}"),
            models: models.iter().map(|s| s.to_string()).collect(),
            request_overrides: Map::new(),
            responses_translate: false,
        }
    }

    fn mapping(from: &str, to: &str) -> ModelMapping {
        ModelMapping {
            from: from.into(),
            to: to.into(),
            regex: false,
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

    /// RAII guard returned from `install`. Holds the serialisation mutex
    /// for the duration of the test so the registry stays consistent. Derefs
    /// to the inner FallbackHandler so call sites read like
    /// `h.decide(path, body)`.
    struct TestGuard {
        _g: std::sync::MutexGuard<'static, ()>,
        h: FallbackHandler,
    }

    impl std::ops::Deref for TestGuard {
        type Target = FallbackHandler;
        fn deref(&self) -> &FallbackHandler {
            &self.h
        }
    }

    fn install(cfg: &AmpCode) -> TestGuard {
        let g = SERIALISE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        customproxy::global()
            .configure(&cfg.custom_providers)
            .expect("configure registry");
        let h = FallbackHandler::new(cfg).expect("new handler");
        TestGuard { _g: g, h }
    }

    #[test]
    fn no_model_falls_back_to_ampcode() {
        let cfg = cfg(vec![], vec![]);
        let h = install(&cfg);
        let d = h.decide("/v1/chat/completions", b"{}");
        assert_eq!(d.route_type, AmpRouteType::AmpCredits);
        assert!(d.provider_name.is_none());
    }

    #[test]
    fn unknown_model_falls_back_to_ampcode() {
        let cfg = cfg(vec![provider("a", &["gpt-5.4"])], vec![]);
        let h = install(&cfg);
        let body = br#"{"model":"claude-opus-4.6"}"#;
        let d = h.decide("/v1/messages", body);
        assert_eq!(d.route_type, AmpRouteType::AmpCredits);
    }

    #[test]
    fn known_model_routes_to_custom_provider() {
        let cfg = cfg(vec![provider("gw", &["gpt-5.4"])], vec![]);
        let h = install(&cfg);
        let body = br#"{"model":"gpt-5.4"}"#;
        let d = h.decide("/api/provider/gw/v1/chat/completions", body);
        assert_eq!(d.route_type, AmpRouteType::CustomProvider);
        assert_eq!(d.provider_name.as_deref(), Some("gw"));
        assert_eq!(d.resolved_model, "gpt-5.4");
    }

    #[test]
    fn mapping_kicks_in_when_force_disabled() {
        let cfg = cfg(
            vec![provider("gw", &["gpt-5.4"])],
            vec![mapping("claude-opus-4.6", "gpt-5.4")],
        );
        let h = install(&cfg);
        let body = br#"{"model":"claude-opus-4.6"}"#;
        let d = h.decide("/v1/messages", body);
        assert_eq!(d.route_type, AmpRouteType::ModelMapping);
        assert_eq!(d.resolved_model, "gpt-5.4");
        assert_eq!(d.provider_name.as_deref(), Some("gw"));
    }

    #[test]
    fn force_mapping_routes_via_mapping_even_if_orig_exists() {
        let mut c = cfg(
            vec![
                provider("gw1", &["claude-opus-4.6"]),
                provider("gw2", &["gpt-5.4"]),
            ],
            vec![mapping("claude-opus-4.6", "gpt-5.4")],
        );
        c.force_model_mappings = true;
        let h = install(&c);
        let body = br#"{"model":"claude-opus-4.6"}"#;
        let d = h.decide("/v1/messages", body);
        assert_eq!(d.route_type, AmpRouteType::ModelMapping);
        assert_eq!(d.provider_name.as_deref(), Some("gw2"));
    }

    #[test]
    fn gemini_translate_mode_routes_to_custom() {
        let mut c = cfg(vec![provider("gw", &["gpt-5.4-mini"])], vec![]);
        c.gemini_route_mode = "translate".into();
        let h = install(&c);
        let body = b"";
        let d = h.decide(
            "/api/provider/google/v1beta/models/gpt-5.4-mini:generateContent",
            body,
        );
        assert_eq!(d.route_type, AmpRouteType::CustomProvider);
        assert!(d.gemini_translate);
        assert_eq!(d.resolved_model, "gpt-5.4-mini");
    }

    #[test]
    fn gemini_translate_now_handles_streamgenerate() {
        // streamGenerateContent used to fall through to ampcode.com (Go and
        // earlier Rust port). The streaming translator added in
        // customproxy::gemini_stream_translator changes that — translate
        // mode now claims both endpoints.
        let mut c = cfg(vec![provider("gw", &["gpt-5.4-mini"])], vec![]);
        c.gemini_route_mode = "translate".into();
        let h = install(&c);
        let body = b"";
        let d = h.decide(
            "/api/provider/google/v1beta/models/gpt-5.4-mini:streamGenerateContent",
            body,
        );
        assert_eq!(d.route_type, AmpRouteType::CustomProvider);
        assert!(d.gemini_translate);
    }

    #[test]
    fn gemini_default_mode_falls_through_to_ampcode() {
        let c = cfg(vec![provider("gw", &["gpt-5.4-mini"])], vec![]);
        let h = install(&c);
        let body = b"";
        let d = h.decide(
            "/api/provider/google/v1beta/models/gpt-5.4-mini:generateContent",
            body,
        );
        assert_eq!(d.route_type, AmpRouteType::AmpCredits);
        assert!(!d.gemini_translate);
    }

    #[test]
    fn extracts_model_from_gemini_path() {
        assert_eq!(
            extract_gemini_model_from_path(
                "/api/provider/google/v1beta/models/gemini-2.5-pro:generateContent"
            )
            .as_deref(),
            Some("gemini-2.5-pro")
        );
        assert_eq!(
            extract_gemini_model_from_path("/v1beta/models/gemini-pro:streamGenerateContent")
                .as_deref(),
            Some("gemini-pro")
        );
        assert_eq!(
            extract_gemini_model_from_path("/v1beta/models/gemini-pro").as_deref(),
            Some("gemini-pro")
        );
        // AMP CLI / Vertex-style path with publishers/google/ segment.
        assert_eq!(
            extract_gemini_model_from_path(
                "/api/provider/google/v1beta1/publishers/google/models/gemini-3-flash-preview:generateContent"
            )
            .as_deref(),
            Some("gemini-3-flash-preview")
        );
        assert!(extract_gemini_model_from_path("/api/provider/openai/v1/chat").is_none());
    }

    #[test]
    fn is_google_native_path_basics() {
        assert!(is_google_native_path("/v1beta/models/x:generateContent"));
        assert!(is_google_native_path("/v1beta1/models/x"));
        assert!(is_google_native_path("/foo:streamGenerateContent"));
        assert!(!is_google_native_path("/v1/chat/completions"));
    }

    #[test]
    fn extract_model_prefers_body_over_path() {
        let body = br#"{"model":"gpt-5.4"}"#;
        assert_eq!(
            extract_model_from_request(body, "/v1beta/models/gemini-pro:generateContent")
                .as_deref(),
            Some("gpt-5.4")
        );
    }

    #[test]
    fn duplicate_model_fails_over_to_second_provider() {
        let cfg = cfg(
            vec![provider("a", &["gpt-5.4"]), provider("b", &["gpt-5.4"])],
            vec![],
        );
        let h = install(&cfg);
        let body = br#"{"model":"gpt-5.4"}"#;
        let d = h.decide("/v1/messages", body);
        assert_eq!(d.route_type, AmpRouteType::CustomProvider);
        assert_eq!(d.provider_name.as_deref(), Some("a"));

        customproxy::global().record_failure("a", "first");
        customproxy::global().record_failure("a", "second");
        let d = h.decide("/v1/messages", body);
        assert_eq!(d.route_type, AmpRouteType::CustomProvider);
        assert_eq!(d.provider_name.as_deref(), Some("b"));
    }

    #[test]
    fn thinking_suffix_preserved_on_mapping() {
        let cfg = cfg(
            vec![provider("gw", &["gpt-5.4(high)"])],
            vec![mapping("claude-opus-4.6", "gpt-5.4")],
        );
        let h = install(&cfg);
        let body = br#"{"model":"claude-opus-4.6(high)"}"#;
        let d = h.decide("/v1/messages", body);
        // Mapper produces "gpt-5.4", we glue "(high)" back, then the
        // registry strips it for lookup → matches "gpt-5.4(high)".
        assert_eq!(d.route_type, AmpRouteType::ModelMapping);
        assert_eq!(d.provider_name.as_deref(), Some("gw"));
    }
}
