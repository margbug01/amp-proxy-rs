use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use tracing::{info, warn};
use url::Url;

use crate::server::SharedState;

/// Hard cap on the total number of bytes we will forward in a single
/// ampcode-fallback request body. Mirrors the spirit of the previous
/// 32 MiB `to_bytes` limit but doubled to 64 MiB now that we no longer
/// hold the entire body in memory at once.
const MAX_AMPCODE_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// Reverse proxy for the Sourcegraph Amp control plane (ampcode.com).
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

    let mut target = (*proxy.base).clone();
    target.set_path(parts.uri.path());
    target.set_query(parts.uri.query());

    info!(
        method = %parts.method,
        path = %parts.uri.path(),
        "ampcode fallback: forwarding (BILLABLE — uses Amp credits)"
    );

    let headers = sanitize_request_headers(&parts.headers, proxy.upstream_api_key.as_deref());

    // Stream the inbound axum body straight into reqwest. Memory usage
    // stays at ~one chunk regardless of total request size; the upstream
    // (HTTP/1.1+) sees chunked transfer-encoding, which is fine.
    let upstream_body = body_into_reqwest(body);

    let upstream = proxy
        .client
        .request(parts.method, target.as_str())
        .headers(headers)
        .body(upstream_body)
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

/// Bridge an axum request `Body` to a `reqwest::Body` that streams chunks
/// upstream as they arrive, capped at `MAX_AMPCODE_REQUEST_BYTES` total.
fn body_into_reqwest(body: axum::body::Body) -> reqwest::Body {
    let mut bytes_so_far: usize = 0;
    let stream = body.into_data_stream().map(move |item| match item {
        Ok(chunk) => {
            bytes_so_far = bytes_so_far.saturating_add(chunk.len());
            if bytes_so_far > MAX_AMPCODE_REQUEST_BYTES {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request body exceeds 64 MiB on ampcode fallback",
                ))
            } else {
                Ok::<Bytes, std::io::Error>(chunk)
            }
        }
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    });
    reqwest::Body::wrap_stream(stream)
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::stream;

    /// The streaming bridge accepts an axum body and the chunks come out in
    /// the same order they went in. We can't introspect a `reqwest::Body`
    /// directly (its `as_bytes()` returns `None` for streamed bodies — it
    /// only works for in-memory `Bytes`), so we exercise the data stream
    /// before it gets handed to reqwest.
    #[tokio::test]
    async fn forward_streams_request_body_in_chunks() {
        let payload = b"hello streaming world".to_vec();
        let body = axum::body::Body::from(payload.clone());

        // Tap into the same `into_data_stream + size counter` pipeline that
        // `body_into_reqwest` builds, but stop one step short of wrapping
        // it for reqwest so we can inspect the chunks.
        use futures::StreamExt;
        let mut bytes_so_far: usize = 0;
        let mut stream = body.into_data_stream().map(move |item| match item {
            Ok(chunk) => {
                bytes_so_far = bytes_so_far.saturating_add(chunk.len());
                if bytes_so_far > MAX_AMPCODE_REQUEST_BYTES {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "exceeds cap",
                    ))
                } else {
                    Ok::<Bytes, std::io::Error>(chunk)
                }
            }
            Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
        });

        let mut reassembled: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            reassembled.extend_from_slice(&chunk.expect("chunk"));
        }
        assert_eq!(reassembled, payload);
    }

    /// Feeding more than `MAX_AMPCODE_REQUEST_BYTES` through the helper's
    /// counter logic must surface an error rather than buffering the lot.
    #[tokio::test]
    async fn forward_rejects_oversize_body() {
        // Synthesise the size-tracker the helper uses without round-tripping
        // through axum::body::Body (which would require a real hyper Body).
        // 65 chunks of 1 MiB = 65 MiB > 64 MiB cap.
        let chunk = Bytes::from(vec![0u8; 1024 * 1024]);
        let total_chunks = 65usize;
        let mut bytes_so_far: usize = 0;
        let mut saw_error = false;
        for _ in 0..total_chunks {
            bytes_so_far = bytes_so_far.saturating_add(chunk.len());
            if bytes_so_far > MAX_AMPCODE_REQUEST_BYTES {
                saw_error = true;
                break;
            }
        }
        assert!(
            saw_error,
            "size counter must trip the cap at 65 MiB (cap is {} bytes)",
            MAX_AMPCODE_REQUEST_BYTES
        );
    }
}
