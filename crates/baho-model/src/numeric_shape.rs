//! Per-value separator-shape evidence for numeric format detection.
//!
//! Epic 008 locked decision 5: each raw numeric token is classified by the
//! decimal/grouping roles its `.` and `,` characters can carry. This module
//! owns the pure per-value classification; column-level policy selection is
//! owned by `baho-ingest-csv`.

use serde::{Deserialize, Serialize};

/// Role of a separator character in a numeric value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeparatorRole {
    /// Marks the start of the fractional part.
    DecimalMark,
    /// Separates digit groups in the integer part.
    GroupingMark,
}

/// Separator-shape evidence for one raw numeric token.
///
/// Classification describes separator roles only. Validity under a concrete
/// policy is enforced by [`crate::column::NumericParsePolicy::parse_decimal`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NumberShape {
    /// No `.` or `,` after the optional sign; compatible with every policy.
    Neutral,
    /// Role assignments decided by this value, in first-occurrence order of
    /// the separator character. One or two entries.
    Decided {
        /// `(separator, role)` pairs in first-occurrence order.
        assignments: Vec<(char, SeparatorRole)>,
    },
    /// Single separator occurring once, with a well-formed 1–3 digit leading
    /// group and an exactly-3-digit trailing group, so both the grouping and
    /// decimal readings are well-formed (`1,234`, `1.234`).
    Undecided {
        /// The separator character.
        separator: char,
    },
}

/// Classify the separator shape of one raw numeric token.
///
/// Epic 008 locked decision 15: surrounding Unicode whitespace is trimmed
/// before classification, so padded cells contribute the same evidence as
/// unpadded ones. An optional leading `+` or `-` is ignored. After the sign
/// (locked decision 5):
///
/// - no `.` or `,` → [`NumberShape::Neutral`];
/// - both `.` and `,` → the later separator is the decimal mark and the
///   earlier one is the grouping mark (decided);
/// - one separator occurring more than once → it is the grouping mark
///   (decided);
/// - one separator occurring once with a trailing group whose length is not
///   exactly 3, or a leading group that is not 1–3 digits → it is the decimal
///   mark (decided);
/// - one separator occurring once with a 1–3 digit leading group and an
///   exactly-3-digit trailing group → [`NumberShape::Undecided`].
pub fn classify_number_shape(text: &str) -> NumberShape {
    let text = text.trim();
    let rest = match text.as_bytes().first() {
        Some(b'+') | Some(b'-') => &text[1..],
        _ => text,
    };
    let mut dot_count = 0usize;
    let mut comma_count = 0usize;
    let mut first_sep: Option<char> = None;
    let mut last_sep: Option<char> = None;
    for ch in rest.chars() {
        match ch {
            '.' | ',' => {
                if ch == '.' {
                    dot_count += 1;
                } else {
                    comma_count += 1;
                }
                if first_sep.is_none() {
                    first_sep = Some(ch);
                }
                last_sep = Some(ch);
            }
            _ => {}
        }
    }
    match (dot_count, comma_count) {
        (0, 0) => NumberShape::Neutral,
        (0, count) if count > 1 => single_separator_decided(','),
        (count, 0) if count > 1 => single_separator_decided('.'),
        (0, 1) => single_occurrence_shape(rest, ','),
        (1, 0) => single_occurrence_shape(rest, '.'),
        _ => {
            let later = last_sep.expect("both separator kinds occur");
            let earlier = first_sep.expect("both separator kinds occur");
            NumberShape::Decided {
                assignments: vec![
                    (earlier, SeparatorRole::GroupingMark),
                    (later, SeparatorRole::DecimalMark),
                ],
            }
        }
    }
}

/// One separator occurring more than once: it is the grouping mark.
fn single_separator_decided(separator: char) -> NumberShape {
    NumberShape::Decided {
        assignments: vec![(separator, SeparatorRole::GroupingMark)],
    }
}

/// One separator occurring exactly once in `rest`.
fn single_occurrence_shape(rest: &str, separator: char) -> NumberShape {
    let (head, tail) = rest
        .split_once(separator)
        .expect("exactly one separator occurrence");
    let tail_is_three_digits = tail.len() == 3 && tail.bytes().all(|byte| byte.is_ascii_digit());
    let head_is_leading_group =
        (1..=3).contains(&head.len()) && head.bytes().all(|byte| byte.is_ascii_digit());
    if tail_is_three_digits && head_is_leading_group {
        NumberShape::Undecided { separator }
    } else {
        NumberShape::Decided {
            assignments: vec![(separator, SeparatorRole::DecimalMark)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decided(pairs: &[(char, SeparatorRole)]) -> NumberShape {
        NumberShape::Decided {
            assignments: pairs.to_vec(),
        }
    }

    #[test]
    fn values_without_separators_are_neutral() {
        for text in ["500", "1000", "-7", "+42", "0", ""] {
            assert_eq!(classify_number_shape(text), NumberShape::Neutral, "{text}");
        }
    }

    #[test]
    fn both_separators_decide_later_as_decimal_and_earlier_as_grouping() {
        assert_eq!(
            classify_number_shape("1,234.56"),
            decided(&[
                (',', SeparatorRole::GroupingMark),
                ('.', SeparatorRole::DecimalMark)
            ])
        );
        assert_eq!(
            classify_number_shape("1.234,56"),
            decided(&[
                ('.', SeparatorRole::GroupingMark),
                (',', SeparatorRole::DecimalMark)
            ])
        );
        assert_eq!(
            classify_number_shape("-1,234.5"),
            decided(&[
                (',', SeparatorRole::GroupingMark),
                ('.', SeparatorRole::DecimalMark)
            ])
        );
    }

    #[test]
    fn repeated_single_separator_is_grouping() {
        assert_eq!(
            classify_number_shape("1,234,567"),
            decided(&[(',', SeparatorRole::GroupingMark)])
        );
        assert_eq!(
            classify_number_shape("1.234.567"),
            decided(&[('.', SeparatorRole::GroupingMark)])
        );
        assert_eq!(
            classify_number_shape("+1,234,567"),
            decided(&[(',', SeparatorRole::GroupingMark)])
        );
    }

    #[test]
    fn non_three_digit_tail_is_decimal() {
        assert_eq!(
            classify_number_shape("1.23"),
            decided(&[('.', SeparatorRole::DecimalMark)])
        );
        assert_eq!(
            classify_number_shape("1,23"),
            decided(&[(',', SeparatorRole::DecimalMark)])
        );
        assert_eq!(
            classify_number_shape("12,3456"),
            decided(&[(',', SeparatorRole::DecimalMark)])
        );
    }

    #[test]
    fn leading_group_outside_one_to_three_digits_is_decimal() {
        // `1234,567` cannot be a well-formed grouped integer (the leading
        // group is 4 digits), so the only well-formed reading is decimal.
        assert_eq!(
            classify_number_shape("1234,567"),
            decided(&[(',', SeparatorRole::DecimalMark)])
        );
    }

    #[test]
    fn single_separator_with_three_digit_tail_is_undecided() {
        assert_eq!(
            classify_number_shape("1,234"),
            NumberShape::Undecided { separator: ',' }
        );
        assert_eq!(
            classify_number_shape("1.234"),
            NumberShape::Undecided { separator: '.' }
        );
        assert_eq!(
            classify_number_shape("12,345"),
            NumberShape::Undecided { separator: ',' }
        );
        assert_eq!(
            classify_number_shape("123.456"),
            NumberShape::Undecided { separator: '.' }
        );
        assert_eq!(
            classify_number_shape("-1,234"),
            NumberShape::Undecided { separator: ',' }
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_before_classification() {
        assert_eq!(classify_number_shape(" 42 "), NumberShape::Neutral);
        assert_eq!(
            classify_number_shape("\t1,234\n"),
            NumberShape::Undecided { separator: ',' }
        );
        assert_eq!(
            classify_number_shape(" 1,234.56 "),
            decided(&[
                (',', SeparatorRole::GroupingMark),
                ('.', SeparatorRole::DecimalMark)
            ])
        );
    }

    #[test]
    fn classification_serializes_deterministically() {
        let json = serde_json::to_value(classify_number_shape("1,234.56")).unwrap();
        assert_eq!(json["kind"], "decided");
        assert_eq!(
            json["assignments"],
            serde_json::json!([[",", "grouping_mark"], [".", "decimal_mark"]])
        );
        let json = serde_json::to_value(NumberShape::Undecided { separator: ',' }).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "kind": "undecided", "separator": "," })
        );
    }
}
