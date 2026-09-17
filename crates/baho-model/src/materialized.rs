use serde::{Deserialize, Serialize};

use crate::column::ColumnDefinition;
use crate::document::{CellAddress, Value};

/// An immutable materialized result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterializedView {
    pub columns: Vec<ColumnDefinition>,
    pub rows: Vec<MaterializedRow>,
    pub provenance: Vec<RowProvenance>,
}

/// A single row in a materialized view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterializedRow {
    /// Column-aligned values; `None` represents a missing/null cell.
    pub values: Vec<Option<Value>>,
}

/// Tracks which source row and cells a materialized row originated from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowProvenance {
    /// Zero-based source row index.
    pub source_row: usize,
    /// Column-aligned source addresses for the materialized row's values.
    pub source_addresses: Vec<CellAddress>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnDefinition;
    use crate::document::{CellAddress, Value};

    fn sample_view() -> MaterializedView {
        MaterializedView {
            columns: vec![
                ColumnDefinition {
                    id: "column-0".to_string(),
                    ordinal: 0,
                    source_header_raw: Some("Name".to_string()),
                    source_header_normalized: Some("name".to_string()),
                    display_name: "Name".to_string(),
                },
                ColumnDefinition {
                    id: "column-1".to_string(),
                    ordinal: 1,
                    source_header_raw: Some("Value".to_string()),
                    source_header_normalized: Some("value".to_string()),
                    display_name: "Value".to_string(),
                },
            ],
            rows: vec![
                MaterializedRow {
                    values: vec![
                        Some(Value::Text("Alice".to_string())),
                        Some(Value::Number(100.0)),
                    ],
                },
                MaterializedRow {
                    values: vec![Some(Value::Text("Bob".to_string())), None],
                },
            ],
            provenance: vec![
                RowProvenance {
                    source_row: 1,
                    source_addresses: vec![
                        CellAddress {
                            sheet_index: 0,
                            row: 1,
                            col: 0,
                        },
                        CellAddress {
                            sheet_index: 0,
                            row: 1,
                            col: 1,
                        },
                    ],
                },
                RowProvenance {
                    source_row: 2,
                    source_addresses: vec![
                        CellAddress {
                            sheet_index: 0,
                            row: 2,
                            col: 0,
                        },
                        CellAddress {
                            sheet_index: 0,
                            row: 2,
                            col: 1,
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn construct_materialized_view() {
        let view = sample_view();
        assert_eq!(view.columns.len(), 2);
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.provenance.len(), 2);
    }

    #[test]
    fn null_values_are_none() {
        let view = sample_view();
        assert!(view.rows[1].values[1].is_none());
    }

    #[test]
    fn provenance_tracks_source() {
        let view = sample_view();
        assert_eq!(view.provenance[0].source_row, 1);
        assert_eq!(view.provenance[0].source_addresses[0].row, 1);
        assert_eq!(view.provenance[0].source_addresses[1].col, 1);
    }

    #[test]
    fn serde_round_trip() {
        let view = sample_view();
        let json = serde_json::to_string(&view).unwrap();
        let back: MaterializedView = serde_json::from_str(&json).unwrap();
        assert_eq!(view, back);
    }
}
