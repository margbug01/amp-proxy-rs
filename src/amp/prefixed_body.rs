//! Adapter that yields a buffered prefix and then forwards the rest of an
//! upstream body chunk-by-chunk. Lets the amp router peek the first N
//! bytes of an inbound body for routing decisions while still streaming
//! the bulk of the payload to the upstream without holding it in memory.
//!
//! # Design note
//!
//! The original brief described `PrefixedBody` as an `impl http_body::Body`
//! adapter implemented with `pin_project_lite`. That would require adding
//! `http-body` as a *direct* Cargo dependency (it is currently only a
//! transitive dep via axum 0.7). Since the build is locked down to no new
//! dependencies, this module instead exposes a builder that composes the
//! prefix with the tail using axum's own `Body::from_stream`, which yields
//! an `axum::body::Body` that is byte-for-byte equivalent to the originally
//! described struct. Callers that previously held a `PrefixedBody` value
//! now hold a `Body` directly — there is no public surface area lost.

use axum::body::Body;
use bytes::Bytes;
use futures::stream::{self, StreamExt};

/// Builder helper that produces a streaming [`Body`] composed of `prefix`
/// followed by every chunk from `tail`.
pub struct PrefixedBody;

impl PrefixedBody {
    /// Build a [`Body`] that yields `prefix` first, then the chunks from
    /// `tail`. An empty prefix is a transparent passthrough.
    pub fn build(prefix: Bytes, tail: Body) -> Body {
        if prefix.is_empty() {
            return tail;
        }
        let head = stream::once(async move { Ok::<Bytes, axum::Error>(prefix) });
        let tail_stream = tail.into_data_stream();
        Body::from_stream(head.chain(tail_stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use bytes::Bytes;
    use futures::StreamExt;

    /// Drain a `Body` into a `Vec<u8>` for assertion.
    async fn drain(body: Body) -> Vec<u8> {
        let mut s = body.into_data_stream();
        let mut out = Vec::new();
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk.expect("chunk ok"));
        }
        out
    }

    #[tokio::test]
    async fn prefixed_body_yields_prefix_then_tail() {
        let prefix = Bytes::from_static(b"PREFIX:");
        let tail = Body::from("TAIL");
        let combined = PrefixedBody::build(prefix, tail);
        assert_eq!(drain(combined).await, b"PREFIX:TAIL");
    }

    #[tokio::test]
    async fn empty_prefix_passes_through_tail_only() {
        let prefix = Bytes::new();
        let tail = Body::from("only-tail");
        let combined = PrefixedBody::build(prefix, tail);
        assert_eq!(drain(combined).await, b"only-tail");
    }

    #[tokio::test]
    async fn empty_tail_yields_only_prefix() {
        let prefix = Bytes::from_static(b"hello");
        let tail = Body::empty();
        let combined = PrefixedBody::build(prefix, tail);
        assert_eq!(drain(combined).await, b"hello");
    }
}
