//! Retrying outbound HTTP transport.
//!
//! Ported from `internal/customproxy/retry_transport.go`. The Go version
//! wraps an `http.RoundTripper` and retries once on transient transport-level
//! errors (EOF, connection reset, dial/read errors during connection
//! establishment, network timeouts). The Rust port wraps a `reqwest::Client`
//! and exposes a `send_with_retry` helper that takes a closure producing a
//! fresh `reqwest::RequestBuilder` per attempt — that side-steps the fact
//! that `RequestBuilder` is not `Clone` for streaming bodies.
//!
//! Retry policy mirrors Go: at most one retry (`max_attempts = 2`) with a
//! 250 ms delay between attempts.

use std::error::Error as _;
use std::future::Future;
use std::time::Duration;

use reqwest::{Client, RequestBuilder, Response};
use tracing::warn;

/// Default delay between the first attempt and its single retry. Matches
/// the Go `retryingTransport.delay` default.
pub const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Default max attempts. Go runs the request once and retries at most once
/// on a transient error, for a total of 2 attempts.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 2;

/// Retrying transport configuration. Holds the underlying client and the
/// retry policy.
#[derive(Clone, Debug)]
pub struct RetryTransport {
    /// Underlying outbound client.
    pub client: Client,
    /// Maximum number of attempts (including the first). Must be >= 1.
    pub max_attempts: u32,
    /// Sleep duration between attempts.
    pub delay: Duration,
}

impl RetryTransport {
    /// Creates a new transport using the provided client and the Go-version
    /// defaults (2 attempts, 250 ms delay).
    pub fn new(client: Client) -> Self {
        Self {
            client,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            delay: DEFAULT_RETRY_DELAY,
        }
    }

    /// Sends a request produced by the given builder factory, retrying on
    /// transient transport-level errors. The factory is invoked for every
    /// attempt so a fresh body can be supplied.
    ///
    /// HTTP status codes (including 4xx/5xx) are NOT retried — they're
    /// application-level results and replaying could double-bill, replay
    /// side effects, or corrupt partial streams. This matches the Go
    /// behavior exactly.
    pub async fn send_with_retry<F>(&self, mut make_request: F) -> reqwest::Result<Response>
    where
        F: FnMut() -> RequestBuilder,
    {
        let max = self.max_attempts.max(1);
        let mut last_err: Option<reqwest::Error> = None;
        for attempt in 1..=max {
            match make_request().send().await {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    if attempt == max || !is_transient(&err) {
                        return Err(err);
                    }
                    warn!(
                        attempt,
                        delay_ms = self.delay.as_millis() as u64,
                        error = %err,
                        "customproxy: retrying after transient error"
                    );
                    last_err = Some(err);
                    tokio::time::sleep(self.delay).await;
                }
            }
        }
        // Unreachable: loop always returns or sets last_err; keep a sane
        // fallback for the type-checker.
        Err(last_err.expect("send_with_retry: loop must record an error if it exits"))
    }

    /// Convenience wrapper that takes an async closure returning a `Response`
    /// directly. Useful when the caller already has a request future shape
    /// and just wants the retry loop.
    pub async fn run_with_retry<F, Fut, T, E>(&self, mut op: F, classify: impl Fn(&E) -> bool) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let max = self.max_attempts.max(1);
        let mut last_err: Option<E> = None;
        for attempt in 1..=max {
            match op().await {
                Ok(v) => return Ok(v),
                Err(err) => {
                    if attempt == max || !classify(&err) {
                        return Err(err);
                    }
                    warn!(
                        attempt,
                        delay_ms = self.delay.as_millis() as u64,
                        error = %err,
                        "customproxy: retrying after transient error"
                    );
                    last_err = Some(err);
                    tokio::time::sleep(self.delay).await;
                }
            }
        }
        Err(last_err.expect("run_with_retry: loop must record an error if it exits"))
    }
}

/// Reports whether the given reqwest error is a transport-level failure
/// that we believe can be safely retried. Mirrors the Go `isTransient`
/// function's intent: connection-establishment failures, EOF-class errors,
/// and timeouts. Anything we don't recognize is treated as non-retryable
/// to avoid masking bugs.
pub fn is_transient(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }
    // Walk the source chain looking for io::ErrorKind we treat as transient.
    let mut src: Option<&dyn std::error::Error> = err.source();
    while let Some(e) = src {
        if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind;
            return matches!(
                io_err.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::TimedOut
                    | ErrorKind::Interrupted
            );
        }
        src = e.source();
    }
    // reqwest's `is_request` covers some pre-flight builder errors; those
    // aren't safe to retry without rebuilding the request, but our caller
    // already does that on every attempt, so opt-in here.
    err.is_request()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// Synthetic error type for the closure-based retry path tests. We can't
    /// easily fabricate a `reqwest::Error` in unit tests, so the
    /// `run_with_retry` helper provides a generic surface we can exercise.
    #[derive(Debug)]
    struct FakeErr {
        transient: bool,
    }
    impl std::fmt::Display for FakeErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake error (transient={})", self.transient)
        }
    }

    #[tokio::test]
    async fn success_first_try() {
        let client = Client::new();
        let rt = RetryTransport {
            client,
            max_attempts: 2,
            delay: Duration::from_millis(1),
        };
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let res: Result<u32, FakeErr> = rt
            .run_with_retry(
                move || {
                    let calls = calls_c.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<u32, FakeErr>(42)
                    }
                },
                |e| e.transient,
            )
            .await;
        assert_eq!(res.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_once_then_success() {
        let rt = RetryTransport {
            client: Client::new(),
            max_attempts: 2,
            delay: Duration::from_millis(1),
        };
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let res: Result<&'static str, FakeErr> = rt
            .run_with_retry(
                move || {
                    let calls = calls_c.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                        if n == 1 {
                            Err(FakeErr { transient: true })
                        } else {
                            Ok("ok")
                        }
                    }
                },
                |e| e.transient,
            )
            .await;
        assert_eq!(res.unwrap(), "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn max_retries_fail() {
        let rt = RetryTransport {
            client: Client::new(),
            max_attempts: 2,
            delay: Duration::from_millis(1),
        };
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let res: Result<u8, FakeErr> = rt
            .run_with_retry(
                move || {
                    let calls = calls_c.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err(FakeErr { transient: true })
                    }
                },
                |e| e.transient,
            )
            .await;
        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn non_transient_does_not_retry() {
        let rt = RetryTransport {
            client: Client::new(),
            max_attempts: 5,
            delay: Duration::from_millis(1),
        };
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let res: Result<u8, FakeErr> = rt
            .run_with_retry(
                move || {
                    let calls = calls_c.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err(FakeErr { transient: false })
                    }
                },
                |e| e.transient,
            )
            .await;
        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
