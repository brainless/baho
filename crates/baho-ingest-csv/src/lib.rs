//! CSV dialect analysis, logical record parsing, row features,
//! table/header candidates, row classification, and CSV-specific diagnostics.
//!
//! This crate owns CSV-specific ingestion logic. It does not own
//! operation execution or CLI presentation.

pub mod candidates;
pub mod classifier;
pub mod dialect;
pub mod header;
pub mod importer;
pub mod inspector;
pub mod row_features;

pub use candidates::{CandidateConfig, detect_candidates};
pub use classifier::classify_rows;
pub use dialect::CsvDialect;
pub use header::{build_header, normalize_header_cell};
pub use importer::CsvImporter;
pub use inspector::{InspectionResult, LogicalRecord, MalformedRecord, inspect_csv};
pub use row_features::{ColumnShape, RowFeatures, compute_row_features};
