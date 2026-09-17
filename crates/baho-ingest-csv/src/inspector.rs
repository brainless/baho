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

fn is_blank_field(s: &str) -> bool {
    s.trim().is_empty()
}

fn make_logical_record(index: usize, fields: Vec<String>) -> LogicalRecord {
    let is_blank = fields.iter().all(|f| is_blank_field(f));
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
    let mut sampling_complete = true;

    let max_samples = options.max_sample_records;

    for result in reader.records() {
        match result {
            Ok(record) => {
                let fields: Vec<String> = record.iter().map(|s| s.to_string()).collect();
                let width = fields.len();
                if width < width_min {
                    width_min = width;
                }
                if width > width_max {
                    width_max = width;
                }

                let rec = make_logical_record(total_count, fields);

                if rec.is_blank {
                    blank_record_indices.push(rec.index);
                }

                if sampled_records.len() < max_samples {
                    sampled_records.push(rec);
                } else {
                    sampling_complete = false;
                }

                total_count += 1;
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

    let logical_record_count = if sampling_complete {
        Some(total_count)
    } else {
        None
    };

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
        assert_eq!(result.logical_record_count, None);
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
}
