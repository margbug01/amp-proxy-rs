//! Optional body-capture middleware for debugging.
//!
//! Ported from `internal/server/body_capture.go` (Gin). When the incoming
//! request's path contains `path_substring`, both the request and response
//! bodies are buffered, written to disk under `dir`, and then forwarded
//! intact. One file per request, named
//! `<timestamp>-<method>-<sanitized-path>.log`.
//!
//! Failures during capture (file I/O, oversized bodies, etc.) never fail the
//! request — they are logged at WARN and the response is forwarded.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header::CONTENT_LENGTH, HeaderMap};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use tracing::warn;

/// Per-direction cap for the *captured* file. Bodies above this size are
/// truncated in the file with a clear marker; the live request/response is
/// unaffected.
const CAPTURE_TRUNCATE: usize = 2 * 1024 * 1024;

/// Maximum body size accepted for capture buffering. Matches the amp router
/// cap (16 MiB).
const BUFFER_LIMIT: usize = 16 * 1024 * 1024;

/// Configuration accepted by [`body_capture_layer`].
#[derive(Clone)]
pub struct CaptureConfig {
    /// Substring that must appear in the request path to trigger capture.
    pub path_substring: String,
    /// Output directory; created on demand.
    pub dir: PathBuf,
}

/// Axum middleware function — wire via
/// `axum::middleware::from_fn_with_state(cfg, body_capture_layer)`.
pub async fn body_capture_layer(
    State(cfg): State<CaptureConfig>,
    req: Request,
    next: Next,
) -> Response {
    if cfg.path_substring.is_empty() || !req.uri().path().contains(&cfg.path_substring) {
        return next.run(req).await;
    }

    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let path = parts.uri.path().to_string();
    let req_headers = parts.headers.clone();

    if should_skip_by_content_length(&req_headers) {
        let rebuilt = Request::from_parts(parts, body);
        return next.run(rebuilt).await;
    }

    let req_bytes = match axum::body::to_bytes(body, BUFFER_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            warn!(method = %method, path = %path, error = %e, "body_capture: request body buffer failed");
            // The body is already gone; pass through with empty body so we
            // don't break the request.
            let rebuilt = Request::from_parts(parts, Body::empty());
            return next.run(rebuilt).await;
        }
    };

    // Reconstruct the request and forward.
    let rebuilt = Request::from_parts(parts, Body::from(req_bytes.clone()));
    let response = next.run(rebuilt).await;

    let (resp_parts, resp_body) = response.into_parts();
    if should_skip_by_content_length(&resp_parts.headers) {
        return Response::from_parts(resp_parts, resp_body);
    }

    let resp_bytes = match axum::body::to_bytes(resp_body, BUFFER_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            warn!(method = %method, path = %path, error = %e, "body_capture: response body buffer failed");
            return Response::from_parts(resp_parts, Body::empty());
        }
    };

    let capture = CaptureRecord {
        method: &method,
        path: &path,
        req_headers: &req_headers,
        req_body: &req_bytes,
        status: resp_parts.status,
        resp_headers: &resp_parts.headers,
        resp_body: &resp_bytes,
    };
    if let Err(e) = write_capture_file(&cfg.dir, capture).await {
        warn!(method = %method, path = %path, error = %e, "body_capture: write failed");
    }

    Response::from_parts(resp_parts, Body::from(resp_bytes))
}

struct CaptureRecord<'a> {
    method: &'a axum::http::Method,
    path: &'a str,
    req_headers: &'a HeaderMap,
    req_body: &'a Bytes,
    status: axum::http::StatusCode,
    resp_headers: &'a HeaderMap,
    resp_body: &'a Bytes,
}

/// Build the capture file and write it. Returns the path written on success.
async fn write_capture_file(
    dir: &std::path::Path,
    capture: CaptureRecord<'_>,
) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    let sanitized = sanitize_path(capture.path);
    let file_name = format!("{timestamp}-{}-{sanitized}.log", capture.method);
    let file_path = dir.join(file_name);

    let mut out = String::new();
    out.push_str("=== REQUEST ===\n");
    out.push_str(&format!("{} {}\n", capture.method, capture.path));
    append_headers(&mut out, capture.req_headers);
    out.push('\n');
    append_body(&mut out, capture.req_body);
    out.push_str("\n\n=== RESPONSE ===\n");
    out.push_str(&format!("status: {}\n", capture.status.as_u16()));
    append_headers(&mut out, capture.resp_headers);
    out.push('\n');
    append_body(&mut out, capture.resp_body);
    out.push('\n');

    tokio::fs::write(&file_path, out).await?;
    Ok(file_path)
}

/// Append each header as `Name: value` on its own line. Non-UTF-8 values are
/// rendered with [`String::from_utf8_lossy`] so capture never aborts on
/// binary header data. Sensitive header values are redacted.
fn append_headers(buf: &mut String, headers: &HeaderMap) {
    for (name, value) in headers.iter() {
        let v = if is_sensitive_header(name.as_str()) {
            "[REDACTED]".to_string()
        } else {
            match value.to_str() {
                Ok(s) => s.to_string(),
                Err(_) => String::from_utf8_lossy(value.as_bytes()).into_owned(),
            }
        };
        buf.push_str(&format!("{}: {}\n", name.as_str(), v));
    }
}

fn is_sensitive_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization" | "x-api-key" | "proxy-authorization" | "cookie" | "set-cookie"
    ) || name.contains("token")
        || name.contains("key")
        || name.contains("secret")
        || name.contains("password")
}

fn should_skip_by_content_length(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|len| len > BUFFER_LIMIT as u64)
}

/// Append the body, truncating beyond [`CAPTURE_TRUNCATE`] with a marker.
/// Bytes are decoded lossily so a binary body still produces something
/// human-skimmable.
fn append_body(buf: &mut String, body: &Bytes) {
    if body.len() > CAPTURE_TRUNCATE {
        let head = &body[..CAPTURE_TRUNCATE];
        buf.push_str(&String::from_utf8_lossy(head));
        buf.push_str(&format!(
            "\n[... truncated, original was {} bytes ...]",
            body.len()
        ));
    } else {
        buf.push_str(&String::from_utf8_lossy(body));
    }
}

/// Replace filesystem-hostile characters with `_`. Covers POSIX-bad chars
/// (`/`, `\`) and the Windows-reserved set (`:?*<>|"`). An empty path becomes
/// `_` so the filename never starts with a separator.
fn sanitize_path(path: &str) -> String {
    let mut s = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '/' | '\\' | ':' | '?' | '*' | '<' | '>' | '|' | '"' => s.push('_'),
            _ => s.push(c),
        }
    }
    if s.is_empty() {
        s.push('_');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;
    use axum::Router;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tower::ServiceExt;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("amp-proxy-body-capture-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sanitize_replaces_path_separators_and_reserved() {
        assert_eq!(
            sanitize_path("/v1/chat/completions"),
            "_v1_chat_completions"
        );
        assert_eq!(sanitize_path("/a:b?c*d"), "_a_b_c_d");
        assert_eq!(sanitize_path(""), "_");
    }

    #[test]
    fn append_body_truncates_above_cap() {
        let mut buf = String::new();
        let big = Bytes::from(vec![b'a'; CAPTURE_TRUNCATE + 10]);
        append_body(&mut buf, &big);
        assert!(buf.contains("[... truncated, original was"));
        assert!(buf.len() >= CAPTURE_TRUNCATE);
    }

    #[test]
    fn append_headers_redacts_sensitive_values() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("x-api-key", "api-secret".parse().unwrap());
        headers.insert("proxy-authorization", "Basic secret".parse().unwrap());
        headers.insert("cookie", "session=secret".parse().unwrap());
        headers.insert("set-cookie", "session=secret".parse().unwrap());
        headers.insert("x-custom-token", "token-secret".parse().unwrap());
        headers.insert("x-client-key-id", "key-secret".parse().unwrap());
        headers.insert("x-shared-secret", "shared-secret".parse().unwrap());
        headers.insert("x-password-hint", "password-secret".parse().unwrap());
        headers.insert("x-visible", "visible".parse().unwrap());

        let mut buf = String::new();
        append_headers(&mut buf, &headers);

        assert!(buf.contains("authorization: [REDACTED]"));
        assert!(buf.contains("x-api-key: [REDACTED]"));
        assert!(buf.contains("proxy-authorization: [REDACTED]"));
        assert!(buf.contains("cookie: [REDACTED]"));
        assert!(buf.contains("set-cookie: [REDACTED]"));
        assert!(buf.contains("x-custom-token: [REDACTED]"));
        assert!(buf.contains("x-client-key-id: [REDACTED]"));
        assert!(buf.contains("x-shared-secret: [REDACTED]"));
        assert!(buf.contains("x-password-hint: [REDACTED]"));
        assert!(buf.contains("x-visible: visible"));
        assert!(!buf.contains("Bearer secret"));
        assert!(!buf.contains("api-secret"));
        assert!(!buf.contains("session=secret"));
    }

    #[test]
    fn should_skip_by_content_length_only_when_explicitly_over_limit() {
        let mut headers = HeaderMap::new();
        assert!(!should_skip_by_content_length(&headers));

        headers.insert(CONTENT_LENGTH, BUFFER_LIMIT.to_string().parse().unwrap());
        assert!(!should_skip_by_content_length(&headers));

        headers.insert(
            CONTENT_LENGTH,
            (BUFFER_LIMIT + 1).to_string().parse().unwrap(),
        );
        assert!(should_skip_by_content_length(&headers));

        headers.insert(CONTENT_LENGTH, "not-a-number".parse().unwrap());
        assert!(!should_skip_by_content_length(&headers));
    }

    #[tokio::test]
    async fn request_content_length_over_limit_skips_capture_and_preserves_body() {
        let dir = unique_temp_dir("request-content-length-skip");
        let cfg = CaptureConfig {
            path_substring: "/capture-me".to_string(),
            dir: dir.clone(),
        };

        let app = Router::new()
            .route(
                "/capture-me",
                post(|body: Bytes| async move { (StatusCode::OK, body) }),
            )
            .layer(from_fn_with_state(cfg, body_capture_layer));

        let req: Request = HttpRequest::builder()
            .method("POST")
            .uri("/capture-me")
            .header(CONTENT_LENGTH, (BUFFER_LIMIT + 1).to_string())
            .body(Body::from("still forwarded"))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(body, Bytes::from_static(b"still forwarded"));

        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 0, "content-length skip should not capture a file");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn writes_a_file_when_path_matches() {
        let dir = unique_temp_dir("smoke");
        let cfg = CaptureConfig {
            path_substring: "/capture-me".to_string(),
            dir: dir.clone(),
        };

        let app = Router::new()
            .route(
                "/capture-me",
                post(|| async { (StatusCode::OK, "hello-resp") }),
            )
            .layer(from_fn_with_state(cfg, body_capture_layer));

        let req: Request = HttpRequest::builder()
            .method("POST")
            .uri("/capture-me")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"hi":"there"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Drain the response body so the middleware finishes its capture.
        let _ = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !entries.is_empty(),
            "expected at least one capture file in {:?}",
            dir
        );
        let body = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(body.contains("=== REQUEST ==="));
        assert!(body.contains("POST /capture-me"));
        assert!(body.contains("=== RESPONSE ==="));
        assert!(body.contains("status: 200"));
        assert!(body.contains("hello-resp"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_when_path_substring_does_not_match() {
        let dir = unique_temp_dir("skip");
        let cfg = CaptureConfig {
            path_substring: "/never-matches".to_string(),
            dir: dir.clone(),
        };

        let app = Router::new()
            .route("/other", post(|| async { (StatusCode::OK, "ok") }))
            .layer(from_fn_with_state(cfg, body_capture_layer));

        let req: Request = HttpRequest::builder()
            .method("POST")
            .uri("/other")
            .body(Body::from("x"))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(resp.into_body(), 16).await.unwrap();

        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 0, "no file should have been captured");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
