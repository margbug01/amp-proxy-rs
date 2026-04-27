//! Small utility helpers.
//!
//! The Go `internal/util/provider.go` exposes a richer `GetProviderName(model)`
//! that consults the customproxy and registry singletons. The Rust port pushes
//! that lookup into the customproxy module itself; here we only retain the
//! one helper that needed a different signature: extracting the provider name
//! out of a `/api/provider/<name>/...` URL path.

/// Extract the provider name from a request path of the form
/// `/api/provider/<name>/<rest>` or `/api/provider/<name>` (with optional
/// trailing slash). Returns `""` when the path does not match.
///
/// The slice returned is borrowed from `path`, so the caller doesn't pay an
/// allocation for the common hot-path case.
pub fn get_provider_name(path: &str) -> &str {
    const PREFIX: &str = "/api/provider/";
    let rest = match path.strip_prefix(PREFIX) {
        Some(r) => r,
        None => return "",
    };
    if rest.is_empty() {
        return "";
    }

    // Take everything up to the next '/'. If the name is followed by a slash
    // and nothing else (i.e. "/api/provider/foo/"), we still want "foo".
    match rest.find('/') {
        Some(idx) => &rest[..idx],
        None => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_name_with_subpath() {
        assert_eq!(get_provider_name("/api/provider/foo/v1/x"), "foo");
    }

    #[test]
    fn extracts_name_with_trailing_slash() {
        assert_eq!(get_provider_name("/api/provider/foo/"), "foo");
    }

    #[test]
    fn extracts_name_without_subpath() {
        assert_eq!(get_provider_name("/api/provider/foo"), "foo");
    }

    #[test]
    fn returns_empty_for_non_provider_path() {
        assert_eq!(get_provider_name("/api/something/else"), "");
        assert_eq!(get_provider_name("/"), "");
        assert_eq!(get_provider_name(""), "");
    }

    #[test]
    fn returns_empty_when_name_missing() {
        // "/api/provider/" with no name at all
        assert_eq!(get_provider_name("/api/provider/"), "");
    }

    #[test]
    fn name_with_dashes_and_dots() {
        assert_eq!(
            get_provider_name("/api/provider/openai-compat.v2/v1/chat"),
            "openai-compat.v2"
        );
    }
}
