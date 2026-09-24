use serde::{Deserialize, Serialize};

use crate::decimal::{DecimalParseError, ExactDecimal};
use crate::document::{ParsedCell, SourcedCell};

/// Metadata for a single column in a materialized view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDefinition {
    /// Stable column identifier, e.g. "column-0".
    pub id: String,
    /// Zero-based ordinal position.
    pub ordinal: usize,
    /// Raw header text from the source, if available.
    pub source_header_raw: Option<String>,
    /// Normalized (trimmed, folded) header text, if available.
    pub source_header_normalized: Option<String>,
    /// Human-readable display name for presentation.
    pub display_name: String,
}

/// Locked Epic 006 decision 5: the malformed share of a compared column's
/// nonblank cells may not exceed this percentage before the column counts as
/// materially mixed.
pub const MIXED_COLUMN_MALFORMED_SHARE_PERCENT: u64 = 10;

/// Type inferred for a compared column from strict-decimal parse outcomes.
///
/// The inference is derived from [`ParsedColumn`] cells and is not persisted
/// on its own; it lets validation distinguish compatible and incompatible
/// comparisons without re-reading source formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferredColumnType {
    /// At least one valid decimal and the malformed share is within the
    /// locked limit; the column can back a decimal comparison.
    Numeric,
    /// No valid decimal, but at least one nonblank non-numeric cell.
    Text,
    /// At least one valid decimal but the malformed share exceeds the locked
    /// limit; decimal comparison is refused and text comparison is per cell.
    Mixed,
    /// No nonblank cells at all.
    Blank,
}

/// Policy that produced a column's parsed values.
///
/// Epic 008 locked decision 4: the two grouping variants cover the two common
/// separator conventions; [`NumericParsePolicy::StrictDecimal`] keeps the
/// Epic 006 grammar unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericParsePolicy {
    /// Accept only the exact decimal literal grammar: optional sign, ASCII
    /// digits, and an optional `.` fractional part. Refuse everything else.
    StrictDecimal,
    /// `.` marks decimals and `,` groups thousands: `1,234.56`.
    DotDecimalCommaGrouping,
    /// `,` marks decimals and `.` groups thousands: `1.234,56`.
    CommaDecimalDotGrouping,
}

impl NumericParsePolicy {
    /// Parse raw cell text as an exact decimal under this policy.
    pub fn parse_decimal(&self, text: &str) -> Result<ExactDecimal, DecimalParseError> {
        match self {
            NumericParsePolicy::StrictDecimal => ExactDecimal::parse(text),
            NumericParsePolicy::DotDecimalCommaGrouping => {
                crate::decimal::parse_grouped_decimal(text, '.', ',')
            }
            NumericParsePolicy::CommaDecimalDotGrouping => {
                crate::decimal::parse_grouped_decimal(text, ',', '.')
            }
        }
    }

    /// The decimal mark of this policy, if it has a fixed one.
    pub fn decimal_mark(self) -> Option<char> {
        match self {
            // StrictDecimal accepts `.` as the only fractional mark but has
            // no grouping mark; callers that only need the mark can use `'.'`.
            NumericParsePolicy::StrictDecimal => Some('.'),
            NumericParsePolicy::DotDecimalCommaGrouping => Some('.'),
            NumericParsePolicy::CommaDecimalDotGrouping => Some(','),
        }
    }

    /// The grouping mark of this policy, when grouping is accepted.
    pub fn group_mark(self) -> Option<char> {
        match self {
            NumericParsePolicy::StrictDecimal => None,
            NumericParsePolicy::DotDecimalCommaGrouping => Some(','),
            NumericParsePolicy::CommaDecimalDotGrouping => Some('.'),
        }
    }

    /// Build the policy implied by a decimal mark and an optional grouping mark.
    ///
    /// `('.', None)` is [`NumericParsePolicy::StrictDecimal`]; a comma decimal
    /// mark always selects [`NumericParsePolicy::CommaDecimalDotGrouping`]
    /// even when no grouping mark is evidenced. Anything else is refused.
    pub fn from_marks(decimal_mark: char, group_mark: Option<char>) -> Option<Self> {
        match (decimal_mark, group_mark) {
            ('.', None) => Some(NumericParsePolicy::StrictDecimal),
            ('.', Some(',')) => Some(NumericParsePolicy::DotDecimalCommaGrouping),
            (',', None) | (',', Some('.')) => Some(NumericParsePolicy::CommaDecimalDotGrouping),
            _ => None,
        }
    }

    /// Build the policy implied by a grouping mark alone (locked decision 6
    /// grouping preference): comma grouping means a dot decimal mark, and dot
    /// grouping means a comma decimal mark.
    pub fn from_grouping_mark(group_mark: char) -> Option<Self> {
        match group_mark {
            ',' => Some(NumericParsePolicy::DotDecimalCommaGrouping),
            '.' => Some(NumericParsePolicy::CommaDecimalDotGrouping),
            _ => None,
        }
    }
}

/// Parsed values for one compared column under its recorded policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedColumn {
    /// Stable column identifier matching `ColumnDefinition::id`.
    pub column_id: String,
    /// The parse policy that produced `cells`.
    pub policy: NumericParsePolicy,
    /// Source-order cells with raw text, coordinates, and parse outcomes.
    pub cells: Vec<SourcedCell>,
}

impl ParsedColumn {
    /// Infer this column's type from its strict-decimal parse outcomes.
    ///
    /// Counts only nonblank cells. Zero nonblank cells is [`Blank`]; zero
    /// valid decimals is [`Text`]; a malformed share above
    /// [`MIXED_COLUMN_MALFORMED_SHARE_PERCENT`] is [`Mixed`]; otherwise
    /// [`Numeric`].
    ///
    /// [`Blank`]: InferredColumnType::Blank
    /// [`Text`]: InferredColumnType::Text
    /// [`Mixed`]: InferredColumnType::Mixed
    /// [`Numeric`]: InferredColumnType::Numeric
    pub fn inferred_type(&self) -> InferredColumnType {
        let mut valid = 0u128;
        let mut malformed = 0u128;
        for cell in &self.cells {
            match cell.parsed {
                ParsedCell::Valid(_) => valid += 1,
                ParsedCell::Malformed { .. } => malformed += 1,
                ParsedCell::Blank | ParsedCell::Missing => {}
            }
        }
        let nonblank = valid + malformed;
        if nonblank == 0 {
            InferredColumnType::Blank
        } else if valid == 0 {
            InferredColumnType::Text
        } else if malformed * 100 > nonblank * u128::from(MIXED_COLUMN_MALFORMED_SHARE_PERCENT) {
            InferredColumnType::Mixed
        } else {
            InferredColumnType::Numeric
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{CellAddress, ParsedCell};

    #[test]
    fn construct_column_definition() {
        let col = ColumnDefinition {
            id: "column-0".to_string(),
            ordinal: 0,
            source_header_raw: Some("Name".to_string()),
            source_header_normalized: Some("name".to_string()),
            display_name: "Name".to_string(),
        };
        assert_eq!(col.id, "column-0");
        assert_eq!(col.ordinal, 0);
        assert_eq!(col.display_name, "Name");
    }

    #[test]
    fn optional_headers_can_be_none() {
        let col = ColumnDefinition {
            id: "column-1".to_string(),
            ordinal: 1,
            source_header_raw: None,
            source_header_normalized: None,
            display_name: "Column 2".to_string(),
        };
        assert!(col.source_header_raw.is_none());
        assert!(col.source_header_normalized.is_none());
    }

    #[test]
    fn serde_round_trip() {
        let col = ColumnDefinition {
            id: "column-5".to_string(),
            ordinal: 5,
            source_header_raw: Some("Revenue ($)".to_string()),
            source_header_normalized: Some("revenue ($)".to_string()),
            display_name: "Revenue ($)".to_string(),
        };
        let json = serde_json::to_string(&col).unwrap();
        let back: ColumnDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(col, back);
    }

    #[test]
    fn numeric_parse_policy_serializes_as_strict_decimal() {
        assert_eq!(
            serde_json::to_value(NumericParsePolicy::StrictDecimal).unwrap(),
            serde_json::json!("strict_decimal")
        );
    }

    #[test]
    fn numeric_parse_policy_serializes_grouping_variants() {
        assert_eq!(
            serde_json::to_value(NumericParsePolicy::DotDecimalCommaGrouping).unwrap(),
            serde_json::json!("dot_decimal_comma_grouping")
        );
        assert_eq!(
            serde_json::to_value(NumericParsePolicy::CommaDecimalDotGrouping).unwrap(),
            serde_json::json!("comma_decimal_dot_grouping")
        );
        for name in [
            "strict_decimal",
            "dot_decimal_comma_grouping",
            "comma_decimal_dot_grouping",
        ] {
            let back: NumericParsePolicy = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(
                serde_json::to_value(back).unwrap(),
                serde_json::json!(name),
                "{name}"
            );
        }
    }

    #[test]
    fn strict_decimal_policy_parses_and_refuses_literals() {
        let policy = NumericParsePolicy::StrictDecimal;
        assert_eq!(
            policy.parse_decimal("1.10").unwrap(),
            ExactDecimal::parse("1.1").unwrap()
        );
        assert_eq!(
            policy.parse_decimal("10,000"),
            Err(DecimalParseError::InvalidCharacter)
        );
    }

    #[test]
    fn grouping_policies_parse_comma_and_dot_formats_exactly() {
        use crate::decimal::ExactDecimal as Dec;
        let us = NumericParsePolicy::DotDecimalCommaGrouping;
        let eu = NumericParsePolicy::CommaDecimalDotGrouping;
        assert_eq!(
            us.parse_decimal("1,234.56").unwrap(),
            Dec::parse("1234.56").unwrap()
        );
        assert_eq!(
            eu.parse_decimal("1.234,56").unwrap(),
            Dec::parse("1234.56").unwrap()
        );
        assert_eq!(
            us.parse_decimal("10,000").unwrap(),
            Dec::parse("10000").unwrap()
        );
        assert_eq!(
            eu.parse_decimal("1.234").unwrap(),
            Dec::parse("1234").unwrap()
        );
        assert_eq!(us.parse_decimal("2.5").unwrap(), Dec::parse("2.5").unwrap());
        assert_eq!(eu.parse_decimal("2,5").unwrap(), Dec::parse("2.5").unwrap());
        assert_eq!(
            us.parse_decimal("-1,234.50").unwrap(),
            Dec::parse("-1234.5").unwrap()
        );
        assert_eq!(
            us.parse_decimal("1,234,567").unwrap(),
            Dec::parse("1234567").unwrap()
        );
        assert_eq!(
            eu.parse_decimal("1.234.567").unwrap(),
            Dec::parse("1234567").unwrap()
        );
    }

    #[test]
    fn grouping_policies_refuse_invalid_grouping_and_wrong_roles() {
        let us = NumericParsePolicy::DotDecimalCommaGrouping;
        let eu = NumericParsePolicy::CommaDecimalDotGrouping;
        assert_eq!(
            us.parse_decimal("1,23"),
            Err(DecimalParseError::InvalidGrouping)
        );
        assert_eq!(
            eu.parse_decimal("1.23"),
            Err(DecimalParseError::InvalidGrouping)
        );
        // `2.5` under the EU policy uses `.` as a grouping mark with a
        // 1-digit group, so it is wrong-role there and only parses as a
        // decimal under strict and US policies.
        assert_eq!(
            eu.parse_decimal("2.5"),
            Err(DecimalParseError::InvalidGrouping)
        );
        assert_eq!(
            us.parse_decimal("2.5").unwrap(),
            ExactDecimal::parse("2.5").unwrap()
        );
        assert_eq!(
            us.parse_decimal("1.2.3"),
            Err(DecimalParseError::MultipleDecimalPoints)
        );
        assert_eq!(
            eu.parse_decimal("1.2.3"),
            Err(DecimalParseError::InvalidGrouping)
        );
        assert_eq!(
            us.parse_decimal("1e5"),
            Err(DecimalParseError::InvalidCharacter)
        );
        assert_eq!(
            eu.parse_decimal("1e5"),
            Err(DecimalParseError::InvalidCharacter)
        );
        assert_eq!(
            us.parse_decimal("1.234,56"),
            Err(DecimalParseError::InvalidGrouping)
        );
        assert_eq!(
            eu.parse_decimal("1,234.56"),
            Err(DecimalParseError::InvalidGrouping)
        );
        assert_eq!(
            us.parse_decimal("1,"),
            Err(DecimalParseError::InvalidGrouping)
        );
        assert_eq!(
            us.parse_decimal(",234"),
            Err(DecimalParseError::InvalidGrouping)
        );
        assert_eq!(
            us.parse_decimal("1234,567"),
            Err(DecimalParseError::InvalidGrouping)
        );
        assert_eq!(
            NumericParsePolicy::StrictDecimal.parse_decimal("1,234.56"),
            Err(DecimalParseError::InvalidCharacter)
        );
    }

    #[test]
    fn policy_mark_helpers_cover_locked_decisions() {
        assert_eq!(NumericParsePolicy::StrictDecimal.group_mark(), None);
        assert_eq!(
            NumericParsePolicy::DotDecimalCommaGrouping.group_mark(),
            Some(',')
        );
        assert_eq!(
            NumericParsePolicy::CommaDecimalDotGrouping.group_mark(),
            Some('.')
        );
        assert_eq!(
            NumericParsePolicy::from_marks('.', None),
            Some(NumericParsePolicy::StrictDecimal)
        );
        assert_eq!(
            NumericParsePolicy::from_marks('.', Some(',')),
            Some(NumericParsePolicy::DotDecimalCommaGrouping)
        );
        assert_eq!(
            NumericParsePolicy::from_marks(',', None),
            Some(NumericParsePolicy::CommaDecimalDotGrouping)
        );
        assert_eq!(
            NumericParsePolicy::from_marks(',', Some('.')),
            Some(NumericParsePolicy::CommaDecimalDotGrouping)
        );
        assert_eq!(NumericParsePolicy::from_marks('.', Some('.')), None);
        assert_eq!(
            NumericParsePolicy::from_grouping_mark(','),
            Some(NumericParsePolicy::DotDecimalCommaGrouping)
        );
        assert_eq!(
            NumericParsePolicy::from_grouping_mark('.'),
            Some(NumericParsePolicy::CommaDecimalDotGrouping)
        );
        assert_eq!(NumericParsePolicy::from_grouping_mark('x'), None);
    }

    #[test]
    fn parsed_column_records_column_id_and_policy() {
        let column = ParsedColumn {
            column_id: "column-1".to_string(),
            policy: NumericParsePolicy::StrictDecimal,
            cells: vec![SourcedCell {
                address: CellAddress {
                    sheet_index: 0,
                    row: 1,
                    col: 1,
                },
                raw_text: Some("1.10".to_string()),
                parsed: ParsedCell::Valid(ExactDecimal::parse("1.10").unwrap()),
            }],
        };
        assert_eq!(column.column_id, "column-1");
        assert_eq!(column.policy, NumericParsePolicy::StrictDecimal);
        assert_eq!(column.cells.len(), 1);
    }

    #[test]
    fn parsed_column_serde_round_trip() {
        let column = ParsedColumn {
            column_id: "column-1".to_string(),
            policy: NumericParsePolicy::StrictDecimal,
            cells: vec![
                SourcedCell {
                    address: CellAddress {
                        sheet_index: 0,
                        row: 1,
                        col: 1,
                    },
                    raw_text: Some("1.10".to_string()),
                    parsed: ParsedCell::Valid(ExactDecimal::parse("1.10").unwrap()),
                },
                SourcedCell {
                    address: CellAddress {
                        sheet_index: 0,
                        row: 2,
                        col: 1,
                    },
                    raw_text: None,
                    parsed: ParsedCell::Missing,
                },
            ],
        };
        let json = serde_json::to_string(&column).unwrap();
        let back: ParsedColumn = serde_json::from_str(&json).unwrap();
        assert_eq!(column, back);
    }

    #[test]
    fn parsed_column_json_structure() {
        let column = ParsedColumn {
            column_id: "column-1".to_string(),
            policy: NumericParsePolicy::StrictDecimal,
            cells: vec![SourcedCell {
                address: CellAddress {
                    sheet_index: 0,
                    row: 3,
                    col: 1,
                },
                raw_text: Some("abc".to_string()),
                parsed: ParsedCell::Malformed {
                    raw_text: "abc".to_string(),
                    reason: DecimalParseError::InvalidCharacter,
                },
            }],
        };
        let json = serde_json::to_value(&column).unwrap();
        assert_eq!(json["column_id"], "column-1");
        assert_eq!(json["policy"], "strict_decimal");
        assert_eq!(json["cells"][0]["raw_text"], "abc");
        assert_eq!(json["cells"][0]["address"]["row"], 3);
        assert_eq!(
            json["cells"][0]["parsed"],
            serde_json::json!({
                "malformed": { "raw_text": "abc", "reason": "invalid_character" }
            })
        );
    }

    fn inferred_column(cells: Vec<ParsedCell>) -> ParsedColumn {
        ParsedColumn {
            column_id: "column-1".to_string(),
            policy: NumericParsePolicy::StrictDecimal,
            cells: cells
                .into_iter()
                .enumerate()
                .map(|(row, parsed)| SourcedCell {
                    address: CellAddress {
                        sheet_index: 0,
                        row,
                        col: 1,
                    },
                    raw_text: None,
                    parsed,
                })
                .collect(),
        }
    }

    fn valid(text: &str) -> ParsedCell {
        ParsedCell::Valid(ExactDecimal::parse(text).unwrap())
    }

    fn malformed(text: &str) -> ParsedCell {
        ParsedCell::Malformed {
            raw_text: text.to_string(),
            reason: DecimalParseError::InvalidCharacter,
        }
    }

    #[test]
    fn inferred_type_covers_numeric_text_mixed_and_blank() {
        assert_eq!(
            inferred_column(vec![valid("500"), ParsedCell::Blank, ParsedCell::Missing])
                .inferred_type(),
            InferredColumnType::Numeric
        );
        assert_eq!(
            inferred_column(vec![malformed("abc"), ParsedCell::Blank]).inferred_type(),
            InferredColumnType::Text
        );
        assert_eq!(
            inferred_column(vec![valid("500"), malformed("abc"), valid("600")]).inferred_type(),
            InferredColumnType::Mixed
        );
        assert_eq!(
            inferred_column(vec![ParsedCell::Blank, ParsedCell::Missing]).inferred_type(),
            InferredColumnType::Blank
        );
        assert_eq!(
            inferred_column(vec![]).inferred_type(),
            InferredColumnType::Blank
        );
    }

    #[test]
    fn inferred_type_applies_locked_malformed_share_boundary() {
        let mut exact = vec![valid("500"); 9];
        exact.push(malformed("10,000"));
        assert_eq!(
            inferred_column(exact).inferred_type(),
            InferredColumnType::Numeric
        );

        let mut over = vec![valid("500"); 8];
        over.push(malformed("10,000"));
        assert_eq!(
            inferred_column(over).inferred_type(),
            InferredColumnType::Mixed
        );
    }
}
