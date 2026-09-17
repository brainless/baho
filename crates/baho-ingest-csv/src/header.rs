use baho_model::candidate::{HeaderCell, HeaderDecision};
use baho_model::diagnostic::{Diagnostic, DiagnosticLocation, Severity};

use crate::inspector::LogicalRecord;
use crate::row_features::RowFeatures;

/// Normalize a header cell's raw text.
///
/// Trims leading/trailing whitespace, collapses internal whitespace
/// (including embedded newlines) to a single space, and returns an
/// empty string for blank cells.
pub fn normalize_header_cell(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut result = String::with_capacity(trimmed.len());
    let mut prev_was_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                result.push(' ');
            }
            prev_was_space = true;
        } else {
            result.push(ch);
            prev_was_space = false;
        }
    }
    result
}

/// Build a header decision from a row.
pub fn build_header(
    _row: &RowFeatures,
    record: &LogicalRecord,
    _sheet_index: usize,
) -> (HeaderDecision, Vec<Diagnostic>) {
    let mut cells = Vec::new();
    let mut diagnostics = Vec::new();

    for (col, field) in record.fields.iter().enumerate() {
        let normalized = normalize_header_cell(field);
        let column_id = format!("column-{}", col);

        if normalized.is_empty() {
            diagnostics.push(Diagnostic {
                code: "header.unnamed_column".to_string(),
                severity: Severity::Warning,
                stage: "ingest-csv".to_string(),
                message: format!("Column {} has no header text", col),
                location: Some(DiagnosticLocation {
                    row: Some(record.index),
                    col: Some(col),
                    cell: None,
                }),
            });
        }

        cells.push(HeaderCell {
            col,
            raw_text: field.clone(),
            normalized_text: normalized,
            column_id,
        });
    }

    let decision = HeaderDecision {
        source_row: record.index,
        cells,
    };

    (decision, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_newline_in_header() {
        assert_eq!(normalize_header_cell("Floor\nPlan"), "Floor Plan");
    }

    #[test]
    fn normalize_whitespace() {
        assert_eq!(normalize_header_cell("  hello   world  "), "hello world");
    }

    #[test]
    fn normalize_blank_cell() {
        assert_eq!(normalize_header_cell(""), "");
        assert_eq!(normalize_header_cell("   "), "");
    }

    #[test]
    fn stable_column_ids() {
        let record = LogicalRecord {
            index: 5,
            fields: vec!["Name".to_string(), "".to_string(), "Age".to_string()],
            is_blank: false,
        };
        let row = RowFeatures {
            index: 5,
            physical_width: 3,
            nonblank_count: 2,
            density: 2.0 / 3.0,
            column_shapes: vec![],
            normalized_tokens: vec![],
            is_blank: false,
            similarity_to_prev: None,
        };
        let (decision, diagnostics) = build_header(&row, &record, 0);
        assert_eq!(decision.source_row, 5);
        assert_eq!(decision.cells.len(), 3);
        assert_eq!(decision.cells[0].column_id, "column-0");
        assert_eq!(decision.cells[1].column_id, "column-1");
        assert_eq!(decision.cells[2].column_id, "column-2");
        assert_eq!(decision.cells[0].normalized_text, "Name");
        assert_eq!(decision.cells[1].normalized_text, "");
        assert_eq!(decision.cells[2].normalized_text, "Age");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "header.unnamed_column");
    }
}
