//! CSV dialect analysis, logical record parsing, row features,
//! table/header candidates, row classification, and CSV-specific diagnostics.
//!
//! This crate owns CSV-specific ingestion logic. It does not own
//! operation execution or CLI presentation.

pub mod candidates;
pub mod classifier;
pub mod config;
pub mod dialect;
pub mod header;
pub mod importer;
pub mod inspector;
pub mod policy_selection;
pub mod row_features;
pub mod typed_values;

pub use candidates::{CandidateConfig, detect_candidates, detect_candidates_with_config};
pub use classifier::{classify_rows, classify_rows_with_config};
pub use config::ParserConfig;
pub use dialect::{CsvDialect, DialectDetectionConfig, DialectDetectionError};
pub use header::{build_header, build_header_with_config, normalize_header_cell};
pub use importer::{CsvImporter, SelectedRegion, SelectedRegionError, read_selected_region};
pub use inspector::{
    InspectionResult, LogicalRecord, MalformedRecord, inspect_csv, inspect_csv_with_config,
};
pub use policy_selection::{
    PolicyAmbiguityReason, PolicySelection, PolicySelectionEvidence, PolicySelectionOutcome,
    PolicySelectionRefused, select_numeric_policy,
};
pub use row_features::{
    ColumnShape, RowFeatures, compute_row_features, compute_row_features_with_config,
};
pub use typed_values::{
    ColumnParseCounts, ComparedColumnParse, MAX_MALFORMED_SAMPLE_CELLS,
    MIXED_COLUMN_MALFORMED_SHARE_PERCENT, MalformedValuesEvidence, MixedRefusalReason,
    ParseVerdict, parse_compared_column, parse_compared_column_selecting_policy,
};
