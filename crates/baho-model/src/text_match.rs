use serde::{Deserialize, Serialize};

/// Policy for comparing text values against text literals in plan predicates.
///
/// Epic 008 locked decision 1: [`TextMatchPolicy::UnicodeLowercase`] applies
/// [`str::to_lowercase`] to both sides and then compares the folded strings
/// for exact equality. That is deterministic std-only Unicode full lowercase
/// mapping: not locale-aware (Turkish dotted-I stays simple) and not Unicode
/// case folding (`ß` does not equal `ss` or `SS`). Outer whitespace is never
/// trimmed (locked decision 11).
///
/// The default is [`TextMatchPolicy::Exact`], the historical semantics of
/// plan schema versions 1 and 2 and of recognition-evidence version 2
/// artifacts that predate this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMatchPolicy {
    /// Compare the raw strings exactly, case-sensitively (plan schema
    /// versions 1 and 2).
    #[default]
    Exact,
    /// Lowercase both sides with [`str::to_lowercase`], then compare exactly
    /// (plan schema version 3).
    UnicodeLowercase,
}

impl TextMatchPolicy {
    /// Whether `left` and `right` are equal under this policy.
    pub fn text_eq(&self, left: &str, right: &str) -> bool {
        match self {
            TextMatchPolicy::Exact => left == right,
            TextMatchPolicy::UnicodeLowercase => left.to_lowercase() == right.to_lowercase(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_policy_compares_raw_strings() {
        let policy = TextMatchPolicy::Exact;
        assert!(policy.text_eq("inactive", "inactive"));
        assert!(!policy.text_eq("Inactive", "inactive"));
        assert!(!policy.text_eq("INACTIVE", "inactive"));
    }

    #[test]
    fn unicode_lowercase_policy_folds_both_sides() {
        let policy = TextMatchPolicy::UnicodeLowercase;
        assert!(policy.text_eq("Inactive", "inactive"));
        assert!(policy.text_eq("INACTIVE", "inactive"));
        assert!(policy.text_eq("InActive", "INACTIVE"));
        assert!(!policy.text_eq("inactive", "active"));
    }

    #[test]
    fn unicode_lowercase_policy_folds_non_ascii_case_pairs() {
        let policy = TextMatchPolicy::UnicodeLowercase;
        assert!(policy.text_eq("Été", "ÉTÉ"));
        assert!(policy.text_eq("ÄÖÜ", "äöü"));
        assert!(!policy.text_eq("Ä", "A"));
    }

    #[test]
    fn unicode_lowercase_is_lowercase_conversion_not_case_folding() {
        let policy = TextMatchPolicy::UnicodeLowercase;
        // Pinned Unicode example: `ß` lowercases to itself, so it never
        // equals `SS` or `ss` under this policy (locked decision 1).
        assert!(policy.text_eq("ß", "ß"));
        assert!(!policy.text_eq("ß", "SS"));
        assert!(!policy.text_eq("ß", "ss"));
        assert!(policy.text_eq("STRASSE", "strasse"));
        assert!(!policy.text_eq("STRASSE", "straße"));
    }

    #[test]
    fn unicode_lowercase_policy_never_trims_whitespace() {
        let policy = TextMatchPolicy::UnicodeLowercase;
        assert!(!policy.text_eq("unemployed ", "unemployed"));
        assert!(!policy.text_eq(" unemployed", "unemployed"));
    }

    #[test]
    fn text_match_policy_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(TextMatchPolicy::Exact).unwrap(),
            serde_json::json!("exact")
        );
        assert_eq!(
            serde_json::to_value(TextMatchPolicy::UnicodeLowercase).unwrap(),
            serde_json::json!("unicode_lowercase")
        );
        let back: TextMatchPolicy = serde_json::from_str("\"exact\"").unwrap();
        assert_eq!(back, TextMatchPolicy::Exact);
        let back: TextMatchPolicy = serde_json::from_str("\"unicode_lowercase\"").unwrap();
        assert_eq!(back, TextMatchPolicy::UnicodeLowercase);
    }
}
