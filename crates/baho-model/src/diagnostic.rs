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
        };
        assert_eq!(loc.row, Some(10));
        assert!(loc.col.is_none());
        assert!(loc.cell.is_none());
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
            }),
        };
        let json = serde_json::to_string(&diag).unwrap();
        let back: Diagnostic = serde_json::from_str(&json).unwrap();
        assert_eq!(diag, back);
    }
}
