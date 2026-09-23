use serde::{Deserialize, Serialize};

use crate::decimal::{DecimalParseError, ExactDecimal};
use crate::revision::SourceRevision;

/// Imported source metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub source: SourceRevision,
    pub sheets: Vec<Sheet>,
    pub path: String,
}

/// A two-dimensional source surface such as a CSV file or a spreadsheet sheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sheet {
    /// Zero-based sheet index.
    pub index: usize,
    /// Optional human-readable name (sheet tab name, or `None` for CSV).
    pub name: Option<String>,
    pub rows: Vec<Row>,
}

/// A physical row in a sheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// Zero-based row index within the sheet.
    pub index: usize,
    pub cells: Vec<Cell>,
}

/// Raw cell representation before any operations are applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub address: CellAddress,
    /// The raw text as it appeared in the source.
    pub raw_text: String,
    /// Interpreted value, if parsing succeeded.
    pub interpreted: Option<Value>,
}

/// Unambiguous location of a cell within a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellAddress {
    /// Zero-based sheet index.
    pub sheet_index: usize,
    /// Zero-based row index.
    pub row: usize,
    /// Zero-based column index.
    pub col: usize,
}

/// Interpreted cell value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Text(String),
    Number(f64),
    Boolean(bool),
    Blank,
}

/// Strict numeric parse outcome for one compared source cell.
///
/// Distinguishes absent cells from present-but-valueless cells, successfully
/// parsed exact decimals, and refused raw text. This is separate from
/// [`Value`], which serves materialized views, and never coerces a malformed
/// cell to null or zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParsedCell {
    /// The cell is absent from a physically short (ragged) row.
    Missing,
    /// The cell is present but carries no value text under the active blank
    /// rule.
    Blank,
    /// The raw text parsed as an exact decimal.
    Valid(ExactDecimal),
    /// The raw text is not a valid literal under the parse policy. The raw
    /// text is retained so the failure record is self-contained.
    Malformed {
        raw_text: String,
        reason: DecimalParseError,
    },
}

/// A source cell's coordinates and raw text alongside its parse outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourcedCell {
    pub address: CellAddress,
    /// Raw text exactly as it appeared in the source; `None` only when
    /// `parsed` is [`ParsedCell::Missing`].
    pub raw_text: Option<String>,
    pub parsed: ParsedCell,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cell(sheet: usize, row: usize, col: usize, text: &str) -> Cell {
        Cell {
            address: CellAddress {
                sheet_index: sheet,
                row,
                col,
            },
            raw_text: text.to_string(),
            interpreted: None,
        }
    }

    #[test]
    fn zero_based_coordinates() {
        let addr = CellAddress {
            sheet_index: 0,
            row: 0,
            col: 0,
        };
        assert_eq!(addr.sheet_index, 0);
        assert_eq!(addr.row, 0);
        assert_eq!(addr.col, 0);
    }

    #[test]
    fn construct_document_with_sheets() {
        let doc = Document {
            source: SourceRevision {
                content_hash: "hash".to_string(),
                file_size: 100,
                modified_time: None,
            },
            sheets: vec![Sheet {
                index: 0,
                name: None,
                rows: vec![Row {
                    index: 0,
                    cells: vec![sample_cell(0, 0, 0, "hello")],
                }],
            }],
            path: "test.csv".to_string(),
        };
        assert_eq!(doc.sheets.len(), 1);
        assert_eq!(doc.sheets[0].rows[0].cells[0].raw_text, "hello");
    }

    #[test]
    fn value_variants() {
        let text = Value::Text("abc".to_string());
        let num = Value::Number(42.0);
        let boolean = Value::Boolean(true);
        let blank = Value::Blank;

        assert_eq!(text, Value::Text("abc".to_string()));
        assert_eq!(num, Value::Number(42.0));
        assert_eq!(boolean, Value::Boolean(true));
        assert_eq!(blank, Value::Blank);
    }

    #[test]
    fn blank_value_is_distinct() {
        assert_ne!(Value::Blank, Value::Text("".to_string()));
    }

    #[test]
    fn cell_serde_round_trip() {
        let cell = Cell {
            address: CellAddress {
                sheet_index: 0,
                row: 3,
                col: 5,
            },
            raw_text: "42".to_string(),
            interpreted: Some(Value::Number(42.0)),
        };
        let json = serde_json::to_string(&cell).unwrap();
        let back: Cell = serde_json::from_str(&json).unwrap();
        assert_eq!(cell, back);
    }

    #[test]
    fn sheet_with_name() {
        let sheet = Sheet {
            index: 2,
            name: Some("Revenue".to_string()),
            rows: vec![],
        };
        assert_eq!(sheet.name.as_deref(), Some("Revenue"));
    }

    #[test]
    fn parsed_cell_distinguishes_missing_blank_valid_malformed() {
        let missing = ParsedCell::Missing;
        let blank = ParsedCell::Blank;
        let valid = ParsedCell::Valid(ExactDecimal::parse("1.5").unwrap());
        let malformed = ParsedCell::Malformed {
            raw_text: "10,000".to_string(),
            reason: DecimalParseError::InvalidCharacter,
        };
        assert_ne!(missing, blank);
        assert_ne!(blank, valid);
        assert_ne!(valid, malformed);
    }

    #[test]
    fn sourced_cell_retains_raw_text_and_coordinates() {
        let cell = SourcedCell {
            address: CellAddress {
                sheet_index: 0,
                row: 4,
                col: 2,
            },
            raw_text: Some("10,000".to_string()),
            parsed: ParsedCell::Malformed {
                raw_text: "10,000".to_string(),
                reason: DecimalParseError::InvalidCharacter,
            },
        };
        assert_eq!(cell.raw_text.as_deref(), Some("10,000"));
        assert_eq!(cell.address.row, 4);
        assert_eq!(cell.address.col, 2);
    }

    #[test]
    fn missing_sourced_cell_has_no_raw_text() {
        let cell = SourcedCell {
            address: CellAddress {
                sheet_index: 0,
                row: 2,
                col: 1,
            },
            raw_text: None,
            parsed: ParsedCell::Missing,
        };
        assert!(cell.raw_text.is_none());
    }

    #[test]
    fn parsed_cell_serde_round_trip() {
        let cells = vec![
            ParsedCell::Missing,
            ParsedCell::Blank,
            ParsedCell::Valid(ExactDecimal::parse("-2.50").unwrap()),
            ParsedCell::Malformed {
                raw_text: "1e5".to_string(),
                reason: DecimalParseError::InvalidCharacter,
            },
        ];
        for cell in cells {
            let json = serde_json::to_string(&cell).unwrap();
            let back: ParsedCell = serde_json::from_str(&json).unwrap();
            assert_eq!(cell, back);
        }
    }

    #[test]
    fn parsed_cell_json_structure_uses_snake_case_kinds() {
        assert_eq!(
            serde_json::to_value(ParsedCell::Missing).unwrap(),
            serde_json::json!("missing")
        );
        assert_eq!(
            serde_json::to_value(ParsedCell::Blank).unwrap(),
            serde_json::json!("blank")
        );
        assert_eq!(
            serde_json::to_value(ParsedCell::Valid(ExactDecimal::parse("1.10").unwrap())).unwrap(),
            serde_json::json!({ "valid": "1.1" })
        );
        assert_eq!(
            serde_json::to_value(ParsedCell::Malformed {
                raw_text: "10,000".to_string(),
                reason: DecimalParseError::InvalidCharacter,
            })
            .unwrap(),
            serde_json::json!({
                "malformed": { "raw_text": "10,000", "reason": "invalid_character" }
            })
        );
    }
}
