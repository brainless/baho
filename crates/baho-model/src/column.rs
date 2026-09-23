use serde::{Deserialize, Serialize};

use crate::decimal::{DecimalParseError, ExactDecimal};
use crate::document::SourcedCell;

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

/// Policy that produced a column's parsed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericParsePolicy {
    /// Accept only the exact decimal literal grammar: optional sign, ASCII
    /// digits, and an optional `.` fractional part. Refuse everything else.
    StrictDecimal,
}

impl NumericParsePolicy {
    /// Parse raw cell text as an exact decimal under this policy.
    pub fn parse_decimal(&self, text: &str) -> Result<ExactDecimal, DecimalParseError> {
        match self {
            NumericParsePolicy::StrictDecimal => ExactDecimal::parse(text),
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
}
