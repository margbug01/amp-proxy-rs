//! API-key generation for the Amp module.
//!
//! Ported from `internal/amp/secret.go`'s key-generation helpers. The
//! multi-source-secret machinery (config / env / file precedence + cache) lives
//! elsewhere; this file only exposes the random-key generator that the rest of
//! the system uses to mint fresh keys.
//!
//! Format: 32 random bytes, base64 url-safe, no padding. This matches the Go
//! version which uses `base64.RawURLEncoding.EncodeToString(rand.Read(32))`.

use rand::RngCore;

/// Length of the random byte buffer used to derive an API key. Kept in sync
/// with the Go constant of the same value.
const API_KEY_BYTES: usize = 32;

/// Generate a fresh random API key: 32 random bytes encoded as
/// base64-url-safe with no padding (so it is safe to drop into URLs and
/// HTTP headers without escaping). The result is 43 ASCII characters.
pub fn generate_api_key() -> String {
    let mut buf = [0u8; API_KEY_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    encode_url_safe_no_pad(&buf)
}

/// Hand-rolled base64 url-safe (RFC 4648 §5) encoder, no padding. Avoids
/// pulling in an extra `base64` dependency given the `Cargo.toml` budget.
fn encode_url_safe_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    let mut chunks = bytes.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        _ => unreachable!(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generated_key_is_non_empty_and_correct_length() {
        let k = generate_api_key();
        // 32 bytes -> ceil(32 * 4 / 3) = 43 chars, no padding.
        assert_eq!(k.len(), 43);
        assert!(!k.is_empty());
    }

    #[test]
    fn generated_keys_are_url_safe() {
        let k = generate_api_key();
        for c in k.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "non-url-safe char {c:?} in {k:?}",
            );
        }
        assert!(!k.contains('='), "padding leaked: {k:?}");
        assert!(!k.contains('+'), "non-url-safe '+' leaked: {k:?}");
        assert!(!k.contains('/'), "non-url-safe '/' leaked: {k:?}");
    }

    #[test]
    fn generated_keys_are_unique() {
        let mut seen = HashSet::new();
        for _ in 0..256 {
            let k = generate_api_key();
            assert!(seen.insert(k), "duplicate key generated");
        }
    }

    #[test]
    fn encode_known_vector() {
        // RFC 4648 test vector: "foobar" -> base64 std "Zm9vYmFy" (no padding
        // needed since 6 bytes is multiple of 3). url-safe is the same here.
        assert_eq!(encode_url_safe_no_pad(b"foobar"), "Zm9vYmFy");
        // "fo"   -> "Zm8" (no pad)
        assert_eq!(encode_url_safe_no_pad(b"fo"), "Zm8");
        // "f"    -> "Zg"  (no pad)
        assert_eq!(encode_url_safe_no_pad(b"f"), "Zg");
        // bytes 0xfb,0xff,0xbf -> std "+/+/", url-safe "-_-_"
        assert_eq!(encode_url_safe_no_pad(&[0xfb, 0xff, 0xbf]), "-_-_");
    }
}
