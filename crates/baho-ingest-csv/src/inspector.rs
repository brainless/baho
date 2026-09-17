use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use baho_ingest::error::ImportError;
use baho_ingest::profile::InspectOptions;
use baho_model::diagnostic::{Diagnostic, DiagnosticLocation, Severity};

use crate::dialect::CsvDialect;

/// A parsed logical CSV record.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalRecord {
    pub index: usize,
    pub fields: Vec<String>,
    pub is_blank: bool,
}

/// A malformed record with an explanation.
#[derive(Debug, Clone, PartialEq)]
pub struct MalformedRecord {
    pub index: usize,
    pub reason: String,
}

/// Result of inspecting a CSV file.
#[derive(Debug, Clone)]
pub struct InspectionResult {
    pub dialect: CsvDialect,
    pub logical_record_count: Option<usize>,
    pub sampled_records: Vec<LogicalRecord>,
    pub width_min: usize,
    pub width_max: usize,
    pub blank_record_indices: Vec<usize>,
    pub malformed_records: Vec<MalformedRecord>,
    pub limits_reached: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

fn make_logical_record(
    index: usize,
    fields: Vec<String>,
    normalization: &crate::config::NormalizationConfig,
) -> LogicalRecord {
    let is_blank = fields.iter().all(|f| normalization.is_blank(f));
    LogicalRecord {
        index,
        fields,
        is_blank,
    }
}

/// Inspect a CSV file with bounded sampling.
pub fn inspect_csv(
    path: &Path,
    dialect: &CsvDialect,
    options: &InspectOptions,
) -> Result<InspectionResult, ImportError> {
    inspect_csv_with_config(
        path,
        dialect,
        options,
        &crate::config::NormalizationConfig::default(),
        crate::config::EvidenceLimitsConfig::default().max_blank_record_indices,
    )
}

pub fn inspect_csv_with_config(
    path: &Path,
    dialect: &CsvDialect,
    options: &InspectOptions,
    normalization: &crate::config::NormalizationConfig,
    max_blank_record_indices: usize,
) -> Result<InspectionResult, ImportError> {
    let file = File::open(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(dialect.delimiter)
        .quote(dialect.quote)
        .escape(Some(dialect.quote_escape))
        .has_headers(false)
        .flexible(true)
        .from_reader(BufReader::new(file));

    let mut sampled_records = Vec::new();
    let mut blank_record_indices = Vec::new();
    let mut malformed_records = Vec::new();
    let mut diagnostics = Vec::new();
    let mut limits_reached = Vec::new();
    let mut width_min = usize::MAX;
    let mut width_max = 0usize;
    let mut total_count = 0usize;
    let mut valid_count = 0usize;
    let mut sampling_complete = true;
    let mut blank_indices_capped = false;

    let max_samples = options.max_sample_records;
    let max_field_size = options.max_field_size;

    if let Some(ref encoding) = options.force_encoding {
        if encoding.to_lowercase() != "utf-8" {
            diagnostics.push(Diagnostic {
                code: "csv.unsupported_encoding".to_string(),
                severity: Severity::Warning,
                stage: "ingest-csv".to_string(),
                message: format!(
                    "force_encoding is set to '{}' but only UTF-8 is currently supported",
                    encoding
                ),
                location: None,
            });
        }
    }

    for result in reader.records() {
        match result {
            Ok(record) => {
                let fields: Vec<String> = record.iter().map(|s| s.to_string()).collect();

                let field_too_large = fields.iter().enumerate().find_map(|(col, field)| {
                    if field.len() > max_field_size {
                        Some((col, field.len()))
                    } else {
                        None
                    }
                });

                if let Some((col, size)) = field_too_large {
                    malformed_records.push(MalformedRecord {
                        index: total_count,
                        reason: format!(
                            "field at column {} exceeds max_field_size ({} bytes > {} bytes)",
                            col, size, max_field_size
                        ),
                    });
                    diagnostics.push(Diagnostic {
                        code: "csv.field_too_large".to_string(),
                        severity: Severity::Warning,
                        stage: "ingest-csv".to_string(),
                        message: format!(
                            "Record {}: field at column {} is {} bytes, exceeding the {} byte limit",
                            total_count, col, size, max_field_size
                        ),
                        location: Some(DiagnosticLocation {
                            row: Some(total_count),
                            col: Some(col),
                            cell: None,
                        }),
                    });
                    total_count += 1;
                    continue;
                }

                let width = fields.len();
                if width < width_min {
                    width_min = width;
                }
                if width > width_max {
                    width_max = width;
                }

                let rec = make_logical_record(total_count, fields, normalization);

                if rec.is_blank {
                    if blank_record_indices.len() < max_blank_record_indices {
                        blank_record_indices.push(rec.index);
                    } else if !blank_indices_capped {
                        blank_indices_capped = true;
                        limits_reached.push("max_blank_record_indices".to_string());
                    }
                }

                if sampled_records.len() < max_samples {
                    sampled_records.push(rec);
                } else {
                    sampling_complete = false;
                }

                total_count += 1;
                valid_count += 1;
            }
            Err(e) => {
                let reason = e.to_string();
                malformed_records.push(MalformedRecord {
                    index: total_count,
                    reason: reason.clone(),
                });
                diagnostics.push(Diagnostic {
                    code: "csv.malformed_record".to_string(),
                    severity: Severity::Warning,
                    stage: "ingest-csv".to_string(),
                    message: format!("Record {}: {}", total_count, reason),
                    location: Some(DiagnosticLocation {
                        row: Some(total_count),
                        col: None,
                        cell: None,
                    }),
                });
                total_count += 1;
            }
        }
    }

    if !sampling_complete {
        limits_reached.push("max_sample_records".to_string());
    }

    if width_min == usize::MAX {
        width_min = 0;
    }

    // Inspection always completes the streaming pass; only retained samples
    // are capped. The aggregate count therefore remains available.
    let logical_record_count = Some(valid_count);

    Ok(InspectionResult {
        dialect: dialect.clone(),
        logical_record_count,
        sampled_records,
        width_min,
        width_max,
        blank_record_indices,
        malformed_records,
        limits_reached,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn inspect_from_str(csv: &str, max_samples: usize) -> InspectionResult {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(csv.as_bytes()).unwrap();
        let dialect = CsvDialect::default();
        let options = InspectOptions {
            max_sample_records: max_samples,
            ..Default::default()
        };
        inspect_csv(file.path(), &dialect, &options).unwrap()
    }

    #[test]
    fn simple_csv_with_header() {
        let result = inspect_from_str("name,age\nAlice,30\nBob,25\n", 100);
        assert_eq!(result.logical_record_count, Some(3));
        assert_eq!(result.sampled_records.len(), 3);
        assert_eq!(result.width_min, 2);
        assert_eq!(result.width_max, 2);
        assert!(result.blank_record_indices.is_empty());
        assert!(result.malformed_records.is_empty());
        assert_eq!(result.sampled_records[0].fields, vec!["name", "age"]);
        assert_eq!(result.sampled_records[1].fields, vec!["Alice", "30"]);
        assert_eq!(result.sampled_records[2].index, 2);
    }

    #[test]
    fn quoted_field_with_newline() {
        let csv = "a,b\n\"hello\nworld\",c\n";
        let result = inspect_from_str(csv, 100);
        assert_eq!(result.logical_record_count, Some(2));
        assert_eq!(result.sampled_records[1].fields[0], "hello\nworld");
        assert_eq!(result.sampled_records[1].fields[1], "c");
    }

    #[test]
    fn blank_rows_detected() {
        let csv = "a,b\n,,\n,,\nx,y\n";
        let result = inspect_from_str(csv, 100);
        assert_eq!(result.logical_record_count, Some(4));
        assert_eq!(result.blank_record_indices, vec![1, 2]);
        assert!(result.sampled_records[1].is_blank);
        assert!(result.sampled_records[2].is_blank);
        assert!(!result.sampled_records[3].is_blank);
    }

    #[test]
    fn empty_csv() {
        let result = inspect_from_str("", 100);
        assert_eq!(result.logical_record_count, Some(0));
        assert!(result.sampled_records.is_empty());
        assert_eq!(result.width_min, 0);
        assert_eq!(result.width_max, 0);
    }

    #[test]
    fn bounded_sampling_sets_limit() {
        let csv = "a,b\n1,2\n3,4\n5,6\n";
        let result = inspect_from_str(csv, 2);
        assert_eq!(result.sampled_records.len(), 2);
        assert_eq!(result.logical_record_count, Some(4));
        assert!(
            result
                .limits_reached
                .contains(&"max_sample_records".to_string())
        );
    }

    #[test]
    fn ragged_rows_detected() {
        let csv = "a,b,c\n1,2\nx,y,z\n";
        let result = inspect_from_str(csv, 100);
        assert_eq!(result.width_min, 2);
        assert_eq!(result.width_max, 3);
        assert_eq!(result.logical_record_count, Some(3));
    }

    #[test]
    fn field_too_large_rejects_record() {
        let mut csv = String::from("a,b\n");
        csv.push_str(&"x".repeat(200));
        csv.push_str(",y\n");
        csv.push_str("c,d\n");

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(csv.as_bytes()).unwrap();
        let dialect = CsvDialect::default();
        let options = InspectOptions {
            max_sample_records: 100,
            max_field_size: 100,
            ..Default::default()
        };
        let result = inspect_csv(file.path(), &dialect, &options).unwrap();

        assert_eq!(result.logical_record_count, Some(2));
        assert_eq!(result.malformed_records.len(), 1);
        assert_eq!(result.malformed_records[0].index, 1);
        assert!(
            result.malformed_records[0]
                .reason
                .contains("max_field_size")
        );
        assert_eq!(result.sampled_records.len(), 2);
        assert_eq!(result.sampled_records[0].fields, vec!["a", "b"]);
        assert_eq!(result.sampled_records[1].fields, vec!["c", "d"]);

        let field_diag = result
            .diagnostics
            .iter()
            .find(|d| d.code == "csv.field_too_large");
        assert!(field_diag.is_some());
        let diag = field_diag.unwrap();
        assert_eq!(diag.severity, Severity::Warning);
        assert_eq!(diag.location.as_ref().unwrap().row, Some(1));
        assert_eq!(diag.location.as_ref().unwrap().col, Some(0));
    }

    #[test]
    fn blank_record_indices_capped() {
        let mut csv = String::from("a,b\n");
        for _ in 0..15_000 {
            csv.push_str(",,\n");
        }
        csv.push_str("x,y\n");

        let result = inspect_from_str(&csv, 20_000);
        assert_eq!(result.blank_record_indices.len(), 10_000);
        assert!(
            result
                .limits_reached
                .contains(&"max_blank_record_indices".to_string())
        );
        assert_eq!(result.logical_record_count, Some(15_002));
    }

    #[test]
    fn force_encoding_utf8_no_warning() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"a,b\n1,2\n").unwrap();
        let dialect = CsvDialect::default();
        let options = InspectOptions {
            force_encoding: Some("utf-8".to_string()),
            ..Default::default()
        };
        let result = inspect_csv(file.path(), &dialect, &options).unwrap();
        let encoding_diag = result
            .diagnostics
            .iter()
            .find(|d| d.code == "csv.unsupported_encoding");
        assert!(encoding_diag.is_none());
    }

    #[test]
    fn force_encoding_non_utf8_warns() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"a,b\n1,2\n").unwrap();
        let dialect = CsvDialect::default();
        let options = InspectOptions {
            force_encoding: Some("latin-1".to_string()),
            ..Default::default()
        };
        let result = inspect_csv(file.path(), &dialect, &options).unwrap();
        let encoding_diag = result
            .diagnostics
            .iter()
            .find(|d| d.code == "csv.unsupported_encoding");
        assert!(encoding_diag.is_some());
        let diag = encoding_diag.unwrap();
        assert_eq!(diag.severity, Severity::Warning);
        assert!(diag.message.contains("latin-1"));
        assert!(diag.message.contains("UTF-8"));
    }

    #[test]
    fn force_encoding_case_insensitive() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"a,b\n1,2\n").unwrap();
        let dialect = CsvDialect::default();
        let options = InspectOptions {
            force_encoding: Some("UTF-8".to_string()),
            ..Default::default()
        };
        let result = inspect_csv(file.path(), &dialect, &options).unwrap();
        let encoding_diag = result
            .diagnostics
            .iter()
            .find(|d| d.code == "csv.unsupported_encoding");
        assert!(encoding_diag.is_none());
    }
}
