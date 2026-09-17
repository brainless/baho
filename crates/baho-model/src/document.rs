use serde::{Deserialize, Serialize};

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
}
