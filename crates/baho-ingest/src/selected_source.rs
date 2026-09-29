//! Format-independent metadata and bounded row batches for a selected table.
//!
//! Adapters own record boundaries and access strategies. A selected-source
//! reader must preserve source coordinates and distinguish an absent field
//! from a present empty string. Reads are bound to the metadata's revision:
//! implementations must fail if the underlying source changes during a read.

use baho_model::candidate::HeaderDecision;
use baho_model::column::ColumnDefinition;
use baho_model::grid::GridRegion;
use baho_model::revision::SourceRevision;

/// Whether the selected source row count has been fully established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCount {
    /// The count covers the complete selected region.
    Exact(usize),
    /// A partial scan has observed this many rows so far.
    Provisional(usize),
}

/// Stable selection result shared by source readers and their consumers.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedSourceMetadata {
    pub source_revision: SourceRevision,
    pub source_sheet_index: usize,
    pub source_sheet_name: Option<String>,
    pub region: GridRegion,
    pub header: HeaderDecision,
    pub columns: Vec<ColumnDefinition>,
    pub row_count: RowCount,
}

/// Raw source value in a selected row. `Missing` differs from `Present("")`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceCellValue {
    Present(String),
    Missing,
}

/// A row with its original zero-based source coordinate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedRow {
    pub source_row: usize,
    pub cells: Vec<SourceCellValue>,
}

/// Caller limits for one read. Both limits must be nonzero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowReadLimits {
    /// Maximum number of selected rows returned by one read.
    pub max_rows: usize,
    /// Maximum combined UTF-8 bytes in returned present cell values.
    pub max_bytes: usize,
}

impl RowReadLimits {
    pub fn new(max_rows: usize, max_bytes: usize) -> Result<Self, InvalidRowReadLimits> {
        if max_rows == 0 || max_bytes == 0 {
            return Err(InvalidRowReadLimits);
        }
        Ok(Self {
            max_rows,
            max_bytes,
        })
    }
}

/// Invalid caller limits. A zero limit cannot make progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("row read limits must have nonzero max_rows and max_bytes")]
pub struct InvalidRowReadLimits;

/// Result of one bounded read from a selected source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedRowBatch {
    pub rows: Vec<SelectedRow>,
    /// True only when the reader has reached the end of the selected source.
    pub complete: bool,
}

/// Errors a selected-source reader can report while honoring batch limits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectedSourceReadError {
    #[error("row read limits must have nonzero max_rows and max_bytes")]
    InvalidLimits,
    #[error("source revision changed while reading selected rows")]
    RevisionChanged,
    #[error("selected source row {row} is malformed: {detail}")]
    MalformedRecord { row: usize, detail: String },
    #[error(
        "selected source row {row}, column {column} exceeds the field limit ({size} > {limit} bytes)"
    )]
    FieldTooLarge {
        row: usize,
        column: usize,
        size: usize,
        limit: usize,
    },
    #[error("selected source row {row} exceeds the batch byte limit ({size} > {limit} bytes)")]
    RowExceedsByteLimit {
        row: usize,
        size: usize,
        limit: usize,
    },
    #[error("could not read selected source: {detail}")]
    Io { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_reject_zero_and_accept_positive_bounds() {
        assert!(RowReadLimits::new(0, 10).is_err());
        assert!(RowReadLimits::new(10, 0).is_err());
        assert_eq!(
            RowReadLimits::new(1, 128).unwrap(),
            RowReadLimits {
                max_rows: 1,
                max_bytes: 128
            }
        );
    }

    #[test]
    fn batch_preserves_missing_empty_and_source_coordinates() {
        let batch = SelectedRowBatch {
            rows: vec![SelectedRow {
                source_row: 7,
                cells: vec![
                    SourceCellValue::Present(String::new()),
                    SourceCellValue::Missing,
                ],
            }],
            complete: false,
        };
        assert_eq!(batch.rows[0].source_row, 7);
        assert_eq!(
            batch.rows[0].cells[0],
            SourceCellValue::Present(String::new())
        );
        assert_eq!(batch.rows[0].cells[1], SourceCellValue::Missing);
        assert!(!batch.complete);
    }
}
