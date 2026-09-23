use serde::{Deserialize, Serialize};

use crate::document::CellAddress;

/// Severity of a diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A stable diagnostic attached to a source location.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Stable diagnostic code, e.g. "csv.malformed_record".
    pub code: String,
    pub severity: Severity,
    /// Pipeline stage that produced this diagnostic.
    pub stage: String,
    pub message: String,
    pub location: Option<DiagnosticLocation>,
}

/// Where in the source a diagnostic applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticLocation {
    pub row: Option<usize>,
    pub col: Option<usize>,
    pub cell: Option<CellAddress>,
    /// Bounded, deterministic sample of source cells a diagnostic applies to.
    /// Optional and additive: absent in older artifacts and omitted when
    /// empty, so existing diagnostics keep their serialized shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<CellAddress>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::CellAddress;

    #[test]
    fn severity_variants() {
        let sevs = [Severity::Error, Severity::Warning, Severity::Info];
        assert_eq!(sevs.len(), 3);
    }

    #[test]
    fn construct_diagnostic_with_location() {
        let diag = Diagnostic {
            code: "csv.malformed_record".to_string(),
            severity: Severity::Warning,
            stage: "ingest-csv".to_string(),
            message: "Row has 3 cells but header has 5".to_string(),
            location: Some(DiagnosticLocation {
                row: Some(4),
                col: None,
                cell: Some(CellAddress {
                    sheet_index: 0,
                    row: 4,
                    col: 0,
                }),
                cells: Vec::new(),
            }),
        };
        assert_eq!(diag.code, "csv.malformed_record");
        assert_eq!(diag.severity, Severity::Warning);
        assert!(diag.location.is_some());
    }

    #[test]
    fn diagnostic_without_location() {
        let diag = Diagnostic {
            code: "csv.encoding_mismatch".to_string(),
            severity: Severity::Info,
            stage: "ingest-csv".to_string(),
            message: "Detected UTF-8 encoding".to_string(),
            location: None,
        };
        assert!(diag.location.is_none());
    }

    #[test]
    fn location_with_partial_fields() {
        let loc = DiagnosticLocation {
            row: Some(10),
            col: None,
            cell: None,
            cells: Vec::new(),
        };
        assert_eq!(loc.row, Some(10));
        assert!(loc.col.is_none());
        assert!(loc.cell.is_none());
        assert!(loc.cells.is_empty());
    }

    #[test]
    fn serde_round_trip() {
        let diag = Diagnostic {
            code: "csv.blank_row".to_string(),
            severity: Severity::Warning,
            stage: "ingest-csv".to_string(),
            message: "Encountered blank row".to_string(),
            location: Some(DiagnosticLocation {
                row: Some(7),
                col: None,
                cell: None,
                cells: Vec::new(),
            }),
        };
        let json = serde_json::to_string(&diag).unwrap();
        let back: Diagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(diag, back);
    }

    #[test]
    fn legacy_location_without_cells_deserializes_with_empty_cells() {
        let location: DiagnosticLocation = serde_json::from_str(
            r#"{"row":4,"col":null,"cell":{"sheet_index":0,"row":4,"col":0}}"#,
        )
        .unwrap();
        assert_eq!(location.row, Some(4));
        assert!(location.col.is_none());
        assert_eq!(
            location.cell,
            Some(CellAddress {
                sheet_index: 0,
                row: 4,
                col: 0,
            })
        );
        assert!(location.cells.is_empty());
    }

    #[test]
    fn serialization_omits_empty_cells_and_keeps_legacy_shape() {
        let location = DiagnosticLocation {
            row: Some(4),
            col: None,
            cell: None,
            cells: Vec::new(),
        };
        let value = serde_json::to_value(&location).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "row": 4, "col": null, "cell": null })
        );
    }

    #[test]
    fn cells_round_trip_preserving_order() {
        let location = DiagnosticLocation {
            row: None,
            col: None,
            cell: Some(CellAddress {
                sheet_index: 0,
                row: 4,
                col: 1,
            }),
            cells: vec![
                CellAddress {
                    sheet_index: 0,
                    row: 4,
                    col: 1,
                },
                CellAddress {
                    sheet_index: 0,
                    row: 5,
                    col: 1,
                },
            ],
        };
        let value = serde_json::to_value(&location).unwrap();
        assert_eq!(value["cells"][0]["row"], 4);
        assert_eq!(value["cells"][1]["row"], 5);
        let back: DiagnosticLocation = serde_json::from_value(value).unwrap();
        assert_eq!(location, back);
    }
}
