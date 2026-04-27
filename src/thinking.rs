//! Thinking-suffix parsing.
//!
//! Ported from `internal/thinking/suffix.go` and `types.go`. The Go version
//! returns a `SuffixResult { ModelName, HasSuffix, RawSuffix }` and never
//! interprets the raw suffix string. The Rust port goes one step further: it
//! also classifies the raw suffix into `effort` (`"low"`, `"medium"`, `"high"`,
//! `"xhigh"`) or `budget_tokens` (positive integer).
//!
//! Recognized formats (matching Go behavior at the parse-suffix level):
//!   * `"claude-sonnet-4-5(16384)"`  -> base + budget=16384
//!   * `"gpt-5.2(high)"`              -> base + effort="high"
//!   * `"gemini-2.5-pro"`             -> base, no suffix
//!   * `"foo(bar)baz"` (no trailing ')') -> no suffix
//!   * `"foo()"`                      -> has_suffix=true, effort=None, budget=None
//!     (the Go ParseSuffix sets HasSuffix=true with empty RawSuffix; we mirror
//!     that, but neither effort nor budget will be populated)

/// Result of parsing a possibly-suffixed model name.
///
/// `model_name` always contains the input with the trailing `(suffix)` stripped
/// when one was found, otherwise the input unchanged. `effort` is `Some` for
/// the four recognized level strings; `budget_tokens` is `Some` for a numeric
/// suffix. They are mutually exclusive: at most one of them is populated.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThinkingSuffix {
    pub model_name: String,
    pub has_suffix: bool,
    pub effort: Option<String>,
    pub budget_tokens: Option<u32>,
}

/// Parse a model name for a trailing `(suffix)` block.
///
/// This mirrors the Go `ParseSuffix`:
///   * the suffix must be the very last `(...)` group, AND the string must end
///     with a `)`.
///   * empty suffix `()` still sets `has_suffix=true` (with neither effort nor
///     budget populated).
///   * the model_name is everything up to (and not including) the `(`.
///
/// Whitespace inside the suffix is *trimmed* before classification, so
/// `"foo( high )"` yields `effort = Some("high")`. The Go version does not do
/// this itself — it leaves classification to callers — but every Go caller in
/// amp-proxy ends up trimming, so we do the trim here once.
pub fn parse_suffix(model: &str) -> ThinkingSuffix {
    // Find the *last* '(' so a model name like "foo(bar)baz(high)" still works.
    let last_open = match model.rfind('(') {
        Some(idx) => idx,
        None => {
            return ThinkingSuffix {
                model_name: model.to_string(),
                has_suffix: false,
                effort: None,
                budget_tokens: None,
            };
        }
    };

    if !model.ends_with(')') {
        return ThinkingSuffix {
            model_name: model.to_string(),
            has_suffix: false,
            effort: None,
            budget_tokens: None,
        };
    }

    let base = &model[..last_open];
    // Slice between '(' and the trailing ')'.
    let raw = &model[last_open + 1..model.len() - 1];

    let trimmed = raw.trim();
    let (effort, budget) = classify(trimmed);

    ThinkingSuffix {
        model_name: base.to_string(),
        has_suffix: true,
        effort,
        budget_tokens: budget,
    }
}

fn classify(raw: &str) -> (Option<String>, Option<u32>) {
    if raw.is_empty() {
        return (None, None);
    }
    let lower = raw.to_ascii_lowercase();
    match lower.as_str() {
        "low" | "medium" | "high" | "xhigh" => return (Some(lower), None),
        _ => {}
    }
    if let Ok(n) = raw.parse::<u32>() {
        return (None, Some(n));
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_model_no_suffix() {
        let r = parse_suffix("gemini-2.5-pro");
        assert_eq!(r.model_name, "gemini-2.5-pro");
        assert!(!r.has_suffix);
        assert!(r.effort.is_none());
        assert!(r.budget_tokens.is_none());
    }

    #[test]
    fn high_effort_suffix() {
        let r = parse_suffix("gpt-5.2(high)");
        assert_eq!(r.model_name, "gpt-5.2");
        assert!(r.has_suffix);
        assert_eq!(r.effort.as_deref(), Some("high"));
        assert!(r.budget_tokens.is_none());
    }

    #[test]
    fn low_medium_xhigh_effort() {
        assert_eq!(parse_suffix("m(low)").effort.as_deref(), Some("low"));
        assert_eq!(parse_suffix("m(medium)").effort.as_deref(), Some("medium"));
        assert_eq!(parse_suffix("m(xhigh)").effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn budget_tokens_suffix() {
        let r = parse_suffix("claude-sonnet-4-5(16384)");
        assert_eq!(r.model_name, "claude-sonnet-4-5");
        assert!(r.has_suffix);
        assert!(r.effort.is_none());
        assert_eq!(r.budget_tokens, Some(16384));
    }

    #[test]
    fn parens_but_not_a_suffix() {
        // No trailing ')' -> Go ParseSuffix returns HasSuffix=false.
        let r = parse_suffix("g25p(high");
        assert_eq!(r.model_name, "g25p(high");
        assert!(!r.has_suffix);
        assert!(r.effort.is_none());
        assert!(r.budget_tokens.is_none());
    }

    #[test]
    fn empty_suffix_has_suffix_true_no_classification() {
        let r = parse_suffix("g25p()");
        assert_eq!(r.model_name, "g25p");
        assert!(r.has_suffix);
        assert!(r.effort.is_none());
        assert!(r.budget_tokens.is_none());
    }

    #[test]
    fn unknown_string_suffix_has_suffix_but_no_effort() {
        let r = parse_suffix("g25p(none)");
        assert_eq!(r.model_name, "g25p");
        assert!(r.has_suffix);
        assert!(r.effort.is_none());
        assert!(r.budget_tokens.is_none());
    }

    #[test]
    fn whitespace_inside_suffix_is_trimmed() {
        let r = parse_suffix("m( high )");
        assert!(r.has_suffix);
        assert_eq!(r.effort.as_deref(), Some("high"));
    }
}
