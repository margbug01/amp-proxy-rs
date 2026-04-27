use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::server::SharedState;

#[derive(Clone)]
pub struct ApiKeyValidator {
    keys: Arc<RwLock<HashSet<String>>>,
}

impl ApiKeyValidator {
    pub fn new<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: HashSet<String> = keys.into_iter().map(Into::into).collect();
        Self {
            keys: Arc::new(RwLock::new(set)),
        }
    }

    pub fn set_keys<I, S>(&self, keys: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut g = self.keys.write().expect("api key validator poisoned");
        g.clear();
        g.extend(keys.into_iter().map(Into::into));
    }

    pub fn contains(&self, key: &str) -> bool {
        self.keys
            .read()
            .expect("api key validator poisoned")
            .contains(key)
    }
}

pub async fn auth_middleware(
    State(state): State<SharedState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // /healthz must stay reachable without credentials so external probes
    // (Docker, Kubernetes, uptime monitors) don't need to know the API key.
    if req.uri().path() == "/healthz" {
        return Ok(next.run(req).await);
    }
    match extract_key(&req) {
        Some(k) if state.validator.contains(&k) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

fn extract_key(req: &Request<Body>) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
    {
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
}
