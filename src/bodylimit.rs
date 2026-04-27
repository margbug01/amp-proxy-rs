//! Body-size-limited reader.
//!
//! Ported from `internal/bodylimit/bodylimit.go`. The Go version exposes
//! `ReadAll`, `ReadPrefix`, an `ErrTooLarge` sentinel, and a `Wrap` helper that
//! decorates errors with the limit context. We mirror those semantics with a
//! thiserror-based error enum and async helpers built on `tokio::io::AsyncRead`.
//!
//! The trick used by the Go version is to read `limit + 1` bytes via a
//! `LimitReader`. If the resulting slice is longer than `limit`, the body
//! exceeded the limit; otherwise it fits.

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Error returned by the body-limit helpers.
#[derive(Debug, thiserror::Error)]
pub enum BodyLimitError {
    /// The body exceeded the configured limit.
    #[error("body too large (limit {limit} bytes)")]
    TooLarge { limit: usize },

    /// An underlying I/O error occurred while reading.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Read up to `max + 1` bytes from `r` so the caller can tell whether the
/// stream fits in `max`. Returns the buffer and a `fits` boolean: `true` when
/// the stream was fully consumed within the limit, `false` when it exceeded.
///
/// The returned buffer always contains every byte that was read, including the
/// (single) byte that caused the overflow. This matches the Go version which
/// returns the raw `LimitReader` output.
pub async fn read_prefix<R>(r: &mut R, max: usize) -> Result<(Bytes, bool), BodyLimitError>
where
    R: AsyncRead + Unpin,
{
    // Read at most max+1 bytes. If we get all max+1, the body is over the
    // limit. Saturating add keeps us safe at usize::MAX.
    let cap = max.saturating_add(1);
    let mut buf: Vec<u8> = Vec::new();
    let mut limited = r.take(cap as u64);
    limited.read_to_end(&mut buf).await?;
    let over = buf.len() > max;
    Ok((Bytes::from(buf), !over))
}

/// Read the entire stream, but fail with [`BodyLimitError::TooLarge`] if it
/// exceeds `max` bytes. On success the returned buffer is the full body.
pub async fn read_all<R>(r: &mut R, max: usize) -> Result<Bytes, BodyLimitError>
where
    R: AsyncRead + Unpin,
{
    let (buf, fits) = read_prefix(r, max).await?;
    if !fits {
        return Err(BodyLimitError::TooLarge { limit: max });
    }
    Ok(buf)
}

/// Synchronous helper for already-buffered bytes (e.g. a body that's already
/// been collected into memory). Returns the original bytes if they fit, else
/// [`BodyLimitError::TooLarge`].
pub fn read_all_bytes(buf: &[u8], max: usize) -> Result<Bytes, BodyLimitError> {
    if buf.len() > max {
        return Err(BodyLimitError::TooLarge { limit: max });
    }
    Ok(Bytes::copy_from_slice(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn read_all_under_limit() {
        let data = b"hello";
        let mut r = Cursor::new(data.to_vec());
        let got = read_all(&mut r, 100).await.expect("should succeed");
        assert_eq!(&got[..], &data[..]);
    }

    #[tokio::test]
    async fn read_all_exact_limit() {
        let data = b"hello world!"; // 12 bytes
        let mut r = Cursor::new(data.to_vec());
        let got = read_all(&mut r, data.len()).await.expect("should succeed");
        assert_eq!(&got[..], &data[..]);
    }

    #[tokio::test]
    async fn read_all_over_limit() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let mut r = Cursor::new(data.to_vec());
        let err = read_all(&mut r, 10).await.expect_err("should fail");
        match err {
            BodyLimitError::TooLarge { limit } => assert_eq!(limit, 10),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_prefix_reports_fits() {
        let data = b"abc";
        let mut r = Cursor::new(data.to_vec());
        let (buf, fits) = read_prefix(&mut r, 10).await.unwrap();
        assert!(fits);
        assert_eq!(&buf[..], b"abc");
    }

    #[tokio::test]
    async fn read_prefix_reports_overflow() {
        let data = b"abcdefghij";
        let mut r = Cursor::new(data.to_vec());
        let (buf, fits) = read_prefix(&mut r, 5).await.unwrap();
        assert!(!fits);
        // We read max+1 bytes: 6.
        assert_eq!(buf.len(), 6);
    }

    #[test]
    fn read_all_bytes_under_limit() {
        let got = read_all_bytes(b"abc", 10).unwrap();
        assert_eq!(&got[..], b"abc");
    }

    #[test]
    fn read_all_bytes_over_limit() {
        let err = read_all_bytes(b"abcdef", 3).expect_err("should fail");
        assert!(matches!(err, BodyLimitError::TooLarge { limit: 3 }));
    }
}
