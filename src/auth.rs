//! API-key authentication middleware.
//!
//! The validator's set of accepted keys is read on every request but only
//! mutated when the YAML config is reloaded (rare, asynchronous). That makes
//! a classic `RwLock` pessimistic: every request would acquire a reader lock,
//! contend on the same atomic, and risk poisoning if a writer panicked.
//!
//! Instead we keep the key set behind [`arc_swap::ArcSwap`]. Reads are a
//! single atomic load (no lock, no clone of the `HashSet`), and `set_keys`
//! atomically swaps in a brand-new `Arc<HashSet<String>>` — old readers keep
//! their snapshot until they drop it. This puts zero synchronization overhead
//! on the hot path while still letting the config watcher hot-reload keys.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::server::SharedState;

/// Validates inbound API keys against a hot-swappable allow-list.
///
/// Cheap to `Clone` — internally just an `Arc` bump.
#[derive(Clone)]
pub struct ApiKeyValidator {
    keys: Arc<ArcSwap<HashSet<String>>>,
}

impl ApiKeyValidator {
    /// Builds a validator initialised with the provided keys.
    pub fn new<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: HashSet<String> = keys.into_iter().map(Into::into).collect();
        Self {
            keys: Arc::new(ArcSwap::from_pointee(set)),
        }
    }

    /// Atomically replaces the entire accepted key set.
    ///
    /// In-flight `contains` calls keep observing the previous snapshot and
    /// finish without contention.
    pub fn set_keys<I, S>(&self, keys: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: HashSet<String> = keys.into_iter().map(Into::into).collect();
        self.keys.store(Arc::new(set));
    }

    /// Returns `true` iff `key` is currently in the accepted set. Lock-free.
    pub fn contains(&self, key: &str) -> bool {
        self.keys.load().contains(key)
    }
}

/// Axum middleware that rejects requests lacking a recognised API key.
///
/// `/healthz` and `/metrics` are allow-listed so probes and Prometheus don't need credentials.
pub async fn auth_middleware(
    State(state): State<SharedState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // /healthz and /metrics must stay reachable without credentials so
    // external probes and Prometheus don't need to know the API key.
    if matches!(req.uri().path(), "/healthz" | "/metrics") {
        return Ok(next.run(req).await);
    }
    match extract_key(&req) {
        Some(k) if state.validator.contains(&k) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Pulls a candidate API key out of either `x-api-key` or
/// `Authorization: Bearer ...`. Returns `None` if neither yields a non-empty
/// token.
fn extract_key(req: &Request<Body>) -> Option<String> {
    if let Some(v) = req.headers().get("x-api-key").and_then(|v| v.to_str().ok()) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(v) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let v = v.trim();
        if let Some(stripped) = v.strip_prefix("Bearer ") {
            let trimmed = stripped.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_round_trip() {
        let v = ApiKeyValidator::new(["abc", "def"]);
        assert!(v.contains("abc"));
        assert!(v.contains("def"));
        assert!(!v.contains("xyz"));
    }

    #[test]
    fn set_keys_replaces() {
        let v = ApiKeyValidator::new(["abc"]);
        v.set_keys(["xyz"]);
        assert!(!v.contains("abc"));
        assert!(v.contains("xyz"));
    }

    #[tokio::test]
    async fn concurrent_reads_and_swaps_dont_block() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let validator = ApiKeyValidator::new(["seed"]);
        let stop = Arc::new(AtomicBool::new(false));

        // Four reader tasks hammer `contains` from different angles.
        let mut readers = Vec::new();
        for i in 0..4 {
            let v = validator.clone();
            let stop = stop.clone();
            readers.push(tokio::spawn(async move {
                let probe = format!("probe-{i}");
                let mut hits: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    // Mix recognised and unrecognised lookups; we don't care
                    // about the exact answer — only that no call panics or
                    // blocks indefinitely while the writer swaps under us.
                    if v.contains(&probe) {
                        hits = hits.wrapping_add(1);
                    }
                    if v.contains("seed") {
                        hits = hits.wrapping_add(1);
                    }
                    tokio::task::yield_now().await;
                }
                hits
            }));
        }

        // Writer task swaps the key set repeatedly, then settles on a known
        // final value so the assertion below is deterministic.
        let writer = {
            let v = validator.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                for round in 0..200u32 {
                    v.set_keys([format!("k-{round}")]);
                    tokio::task::yield_now().await;
                }
                v.set_keys(["final-key"]);
                stop.store(true, Ordering::Relaxed);
            })
        };

        writer.await.expect("writer task panicked");
        for r in readers {
            r.await.expect("reader task panicked");
        }

        // Final state matches the last `set_keys` call.
        assert!(validator.contains("final-key"));
        assert!(!validator.contains("seed"));
        assert!(!validator.contains("k-0"));
        assert!(!validator.contains("k-199"));
    }
}
