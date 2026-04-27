//! Model-name aliasing for Amp requests.
//!
//! Ported from `internal/amp/model_mapping.go`. Two important deviations from
//! the Go version, both intentional:
//!
//!   1. Provider availability check. The Go `MapModel` consults
//!      `util.GetProviderName(target)` and returns "" when no provider exists
//!      for the target. That coupling lives one layer up here in Rust — this
//!      type is a pure name-mapper. Phase 2's fallback handler is responsible
//!      for verifying that a provider actually exists.
//!   2. Thinking-suffix preservation. Same reasoning: this layer answers "is
//!      there a mapping for `model`?". The handler that actually issues the
//!      upstream request decides whether and how to glue the user's suffix
//!      back onto the result.
//!
//! What is preserved verbatim:
//!   * Exact-match keys are case-insensitive (lowered on insert and on query).
//!   * Regex rules are evaluated *in order*, after exact lookup fails.
//!   * Regex rules are case-insensitive (compiled with the `(?i)` prefix).
//!   * Empty / whitespace-only `from`/`to` are silently skipped, matching the
//!     Go version which logs a warning and continues.

use std::collections::HashMap;

use regex::Regex;

use crate::config::ModelMapping;

/// Maps a requested model name to a configured target.
///
/// See the module docs for what this does *not* do (provider checks, suffix
/// preservation). Construction is fallible because regex rules are compiled
/// up-front; an invalid pattern bubbles up as `regex::Error`.
pub struct ModelMapper {
    /// Exact-match table. Keys are lowercased on insert; query lookups
    /// lowercase the input too, so mappings are case-insensitive.
    exact: HashMap<String, String>,
    /// Ordered list of regex rules. Each pattern was prefixed with `(?i)` so
    /// it matches case-insensitively, mirroring the Go version.
    regex_rules: Vec<(Regex, String)>,
}

impl ModelMapper {
    /// Build a mapper from configured rules. Empty `from` or `to` entries are
    /// dropped (matching the Go behavior which logs a warning and skips).
    /// Returns `Err` on the first invalid regex pattern.
    pub fn new(rules: &[ModelMapping]) -> Result<Self, regex::Error> {
        let mut exact: HashMap<String, String> = HashMap::new();
        let mut regex_rules: Vec<(Regex, String)> = Vec::new();

        for rule in rules {
            let from = rule.from.trim();
            let to = rule.to.trim();
            if from.is_empty() || to.is_empty() {
                continue;
            }
            if rule.regex {
                // Match Go: prefix with (?i) for case-insensitive matching.
                let pattern = format!("(?i){from}");
                let re = Regex::new(&pattern)?;
                regex_rules.push((re, to.to_string()));
            } else {
                exact.insert(from.to_lowercase(), to.to_string());
            }
        }

        Ok(Self { exact, regex_rules })
    }

    /// Resolve `model` to its mapped target. Returns `None` when no rule
    /// applies. Exact matches always win over regex; regex rules are checked
    /// in their declared order.
    pub fn apply(&self, model: &str) -> Option<String> {
        if model.is_empty() {
            return None;
        }
        let key = model.trim().to_lowercase();
        if let Some(t) = self.exact.get(&key) {
            return Some(t.clone());
        }
        for (re, to) in &self.regex_rules {
            if re.is_match(model) {
                return Some(to.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(from: &str, to: &str, regex: bool) -> ModelMapping {
        ModelMapping {
            from: from.to_string(),
            to: to.to_string(),
            regex,
        }
    }

    #[test]
    fn exact_match_case_insensitive() {
        let m = ModelMapper::new(&[rule("Claude-Opus-4.5", "claude-sonnet-4", false)]).unwrap();
        assert_eq!(
            m.apply("claude-opus-4.5").as_deref(),
            Some("claude-sonnet-4")
        );
        assert_eq!(
            m.apply("CLAUDE-OPUS-4.5").as_deref(),
            Some("claude-sonnet-4")
        );
    }

    #[test]
    fn regex_match() {
        let m = ModelMapper::new(&[rule("^gpt-5.*$", "gemini-2.5-pro", true)]).unwrap();
        assert_eq!(
            m.apply("gpt-5-turbo").as_deref(),
            Some("gemini-2.5-pro")
        );
    }

    #[test]
    fn regex_case_insensitive() {
        let m = ModelMapper::new(&[rule("^CLAUDE-OPUS-.*$", "claude-sonnet-4", true)]).unwrap();
        assert_eq!(
            m.apply("claude-opus-4.5").as_deref(),
            Some("claude-sonnet-4")
        );
    }

    #[test]
    fn no_match_returns_none() {
        let m = ModelMapper::new(&[rule("foo", "bar", false)]).unwrap();
        assert!(m.apply("not-a-known-model").is_none());
        assert!(m.apply("").is_none());
    }

    #[test]
    fn exact_priority_over_regex() {
        let rules = vec![
            rule("gpt-5", "claude-sonnet-4", false),
            rule("^gpt-5.*$", "gemini-2.5-pro", true),
        ];
        let m = ModelMapper::new(&rules).unwrap();
        // Exact wins for the bare key.
        assert_eq!(m.apply("gpt-5").as_deref(), Some("claude-sonnet-4"));
        // Regex still works for inputs the exact map doesn't cover.
        assert_eq!(
            m.apply("gpt-5-mini").as_deref(),
            Some("gemini-2.5-pro")
        );
    }

    #[test]
    fn regex_rules_evaluated_in_order() {
        let rules = vec![
            rule("^gpt-5.*$", "first", true),
            rule("^gpt-.*$", "second", true),
        ];
        let m = ModelMapper::new(&rules).unwrap();
        // Both regexes match "gpt-5-turbo"; first declared wins.
        assert_eq!(m.apply("gpt-5-turbo").as_deref(), Some("first"));
        // Only the second matches "gpt-4".
        assert_eq!(m.apply("gpt-4").as_deref(), Some("second"));
    }

    #[test]
    fn empty_rules_are_skipped() {
        // The Go version warns-and-skips invalid mapping entries; we do the
        // same. Constructor must succeed.
        let rules = vec![
            rule("", "to", false),
            rule("from", "", false),
            rule("   ", "to", false),
            rule("good", "target", false),
        ];
        let m = ModelMapper::new(&rules).unwrap();
        assert_eq!(m.apply("good").as_deref(), Some("target"));
        assert!(m.apply("from").is_none());
    }

    #[test]
    fn invalid_regex_returns_err() {
        let rules = vec![rule("(", "x", true)];
        assert!(ModelMapper::new(&rules).is_err());
    }
}
