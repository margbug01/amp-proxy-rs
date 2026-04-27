use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use reqwest::Client;
use tracing::{info, warn};
use url::Url;

use crate::server::SharedState;

#[derive(Clone)]
pub struct AmpcodeProxy {
    pub client: Client,
    pub base: Arc<Url>,
    pub upstream_api_key: Option<String>,
}

impl AmpcodeProxy {
    pub fn new(upstream_url: &str, upstream_api_key: &str) -> Result<Self, url::ParseError> {
        let base = Url::parse(upstream_url)?;
        let upstream_api_key = {
            let trimmed = upstream_api_key.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        let client = Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("build reqwest client");
        Ok(Self {
            client,
            base: Arc::new(base),
            upstream_api_key,
        })
    }
}

// forward is the catch-all fallback that mirrors Go-version's
// `amp.FallbackHandler` ampcode.com path. Custom-provider routing and the
// translators are intentionally absent in v0.1 — those are the next
// learning milestones.
pub async fn forward(
    State(state): State<SharedState>,
    req: Request,
) -> Result<Response, StatusCode> {
    let Some(proxy) = state.ampcode.clone() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    let (parts, body) = req.into_parts();

    // Buffering the request body is a v0.1 simplification — most ampcode.com
    // calls are small JSON. Replace with a streaming bridge once you start
    // implementing customproxy in week 2-3.
    let body_bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return Err(StatusCode::PAYLOAD_TOO_LARGE),
    };

    let mut target = (*proxy.base).clone();
    target.set_path(parts.uri.path());
    target.set_query(parts.uri.query());

    info!(
        method = %parts.method,
        path = %parts.uri.path(),
        "ampcode fallback: forwarding (BILLABLE — uses Amp credits)"
    );

    let headers = sanitize_request_headers(&parts.headers, proxy.upstream_api_key.as_deref());

    let upstream = proxy
        .client
        .request(parts.method, target.as_str())
        .headers(headers)
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| {
            warn!(error = %e, "ampcode upstream error");
            StatusCode::BAD_GATEWAY
        })?;

    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let stream = upstream.bytes_stream();

    let mut response = Response::builder().status(status);
    {
        let h = response.headers_mut().expect("fresh builder has headers");
        for (k, v) in upstream_headers.iter() {
            // hop-by-hop headers must not be forwarded.
            if is_hop_by_hop(k.as_str()) {
                continue;
            }
            h.append(k.clone(), v.clone());
        }
    }
    response
        .body(Body::from_stream(stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn sanitize_request_headers(src: &HeaderMap, upstream_api_key: Option<&str>) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (k, v) in src.iter() {
        let name = k.as_str();
        if matches!(name, "host" | "content-length") || is_hop_by_hop(name) {
            continue;
        }
        out.append(k.clone(), v.clone());
    }
    if let Some(key) = upstream_api_key {
        if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {key}")) {
            out.insert(http::header::AUTHORIZATION, val);
        }
    }
    out
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}
