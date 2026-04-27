//! Path-leaf extractor for routing requests to custom upstream providers.
//!
//! Ported 1:1 from `internal/customproxy/customproxy.go`'s `extractLeaf`.

/// Strips `/api/provider/<name>/` and an optional `/v1`, `/v1beta`, or
/// `/v1beta1` version prefix from the incoming request path, returning the
/// suffix that should be appended to the target base URL.
///
/// Examples:
///
/// - `/api/provider/openai/v1/chat/completions` -> `/chat/completions`
/// - `/api/provider/anthropic/v1/messages` -> `/messages`
/// - `/v1/chat/completions` -> `/chat/completions`
/// - `/api/provider/google/v1beta/models/x:y` -> `/models/x:y`
/// - `/chat/completions` -> `/chat/completions`
pub fn extract_leaf(p: &str) -> &str {
    let mut stripped = p;
    if let Some(rest) = stripped.strip_prefix("/api/provider/") {
        if let Some(idx) = rest.find('/') {
            stripped = &rest[idx..];
        } else {
            stripped = "/";
        }
    }
    for prefix in &["/v1beta1", "/v1beta", "/v1"] {
        let with_slash = format!("{}/", prefix);
        if stripped.starts_with(&with_slash) {
            return &stripped[prefix.len()..];
        }
        if stripped == *prefix {
            return "/";
        }
    }
    stripped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_prefix_and_v1() {
        assert_eq!(
            extract_leaf("/api/provider/openai/v1/chat/completions"),
            "/chat/completions"
        );
    }

    #[test]
    fn anthropic_messages() {
        assert_eq!(
            extract_leaf("/api/provider/anthropic/v1/messages"),
            "/messages"
        );
    }

    #[test]
    fn google_v1beta_with_action_suffix() {
        assert_eq!(
            extract_leaf("/api/provider/google/v1beta/models/gpt-5.4:generateContent"),
            "/models/gpt-5.4:generateContent"
        );
    }

    #[test]
    fn google_v1beta1() {
        assert_eq!(
            extract_leaf("/api/provider/google/v1beta1/models/x"),
            "/models/x"
        );
    }

    #[test]
    fn bare_v1_prefix() {
        assert_eq!(extract_leaf("/v1/chat/completions"), "/chat/completions");
    }

    #[test]
    fn already_leaf() {
        assert_eq!(extract_leaf("/chat/completions"), "/chat/completions");
    }

    #[test]
    fn provider_prefix_no_trailing_segment() {
        assert_eq!(extract_leaf("/api/provider/foo"), "/");
    }

    #[test]
    fn bare_v1beta_prefix() {
        assert_eq!(extract_leaf("/v1beta/models"), "/models");
    }

    #[test]
    fn empty_input() {
        assert_eq!(extract_leaf(""), "");
    }
}
