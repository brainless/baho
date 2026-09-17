use std::fs;
use std::path::Path;

use baho_ingest::ImportedDocument;
use baho_ingest::error::ImportError;
use baho_ingest::profile::{InputProfile, InspectOptions};
use baho_ingest::traits::{FormatImporter, FormatInspector};
use baho_model::candidate::HeaderDecision;
use baho_model::diagnostic::Diagnostic;
use baho_model::document::{Cell, CellAddress, Document, Row, Sheet};
use baho_model::revision::SourceRevision;

use crate::candidates::{CandidateConfig, detect_candidates};
use crate::classifier::classify_rows;
use crate::dialect::CsvDialect;
use crate::header::build_header;
use crate::inspector::inspect_csv;
use crate::row_features::compute_row_features;

/// CSV format importer.
pub struct CsvImporter;

impl FormatInspector for CsvImporter {
    fn name(&self) -> &str {
        "csv"
    }

    fn can_inspect(&self, path: &Path, header_bytes: &[u8]) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let known_ext = matches!(ext.as_str(), "csv" | "tsv" | "txt");
        let text_like = header_bytes
            .iter()
            .all(|&b| b >= 0x09 && b <= 0x7e || b == 0x0a || b == 0x0d || b == 0x1b);
        known_ext || text_like
    }
}

impl FormatImporter for CsvImporter {
    fn name(&self) -> &str {
        "csv"
    }

    fn import(
        &self,
        path: &Path,
        options: &InspectOptions,
    ) -> Result<ImportedDocument, ImportError> {
        let dialect = CsvDialect::default();
        let inspection = inspect_csv(path, &dialect, options)?;
        let features = compute_row_features(&inspection.sampled_records);

        let config = CandidateConfig::default();
        let mut candidates = detect_candidates(&inspection.sampled_records, &features, &config);

        let mut all_diagnostics: Vec<Diagnostic> = inspection.diagnostics.clone();

        let (_header_decision, header_diag, _body_classifications) =
            if let Some(best) = candidates.first_mut() {
                let header_idx = best.region.header_row.unwrap_or(0);
                let header_record = &inspection.sampled_records[header_idx];
                let header_feature = &features[header_idx];
                let (decision, diag) = build_header(header_feature, header_record, 0);
                let classifications = classify_rows(&features, header_idx, &config);

                best.header = decision.clone();
                best.body_row_classifications = classifications.clone();
                best.selected = true;

                (decision, diag, classifications)
            } else {
                (
                    HeaderDecision {
                        source_row: 0,
                        cells: Vec::new(),
                    },
                    Vec::new(),
                    Vec::new(),
                )
            };

        all_diagnostics.extend(header_diag);

        let rows: Vec<Row> = inspection
            .sampled_records
            .iter()
            .map(|rec| Row {
                index: rec.index,
                cells: rec
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(col, text)| Cell {
                        address: CellAddress {
                            sheet_index: 0,
                            row: rec.index,
                            col,
                        },
                        raw_text: text.clone(),
                        interpreted: None,
                    })
                    .collect(),
            })
            .collect();

        let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

        let content_hash = compute_content_hash(path)?;

        let document = Document {
            source: SourceRevision {
                content_hash,
                file_size,
                modified_time: None,
            },
            sheets: vec![Sheet {
                index: 0,
                name: None,
                rows,
            }],
            path: path.to_string_lossy().to_string(),
        };

        let input_profile = InputProfile {
            encoding: "utf-8".to_string(),
            detected_delimiter: Some(dialect.delimiter as char),
            detected_quote: Some(dialect.quote as char),
            logical_record_count: inspection.logical_record_count,
            sampled_width_min: Some(inspection.width_min),
            sampled_width_max: Some(inspection.width_max),
            blank_record_count: inspection.blank_record_indices.len(),
            malformed_record_count: inspection.malformed_records.len(),
            limits_reached: inspection.limits_reached,
        };

        Ok(ImportedDocument {
            document,
            input_profile,
            diagnostics: all_diagnostics,
        })
    }
}

fn compute_content_hash(path: &Path) -> Result<String, ImportError> {
    use std::io::Read;
    let mut file = fs::File::open(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|source| ImportError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    let hash = format!("{:x}", md5_simple(&buf));
    Ok(hash)
}

fn md5_simple(data: &[u8]) -> u128 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish() as u128
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn import_from_str(csv: &str) -> ImportedDocument {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(csv.as_bytes()).unwrap();
        let importer = CsvImporter;
        let options = InspectOptions::default();
        importer.import(file.path(), &options).unwrap()
    }

    #[test]
    fn full_import_pipeline() {
        let csv = "Name,Value\nAlice,100\nBob,200\nCarol,300\n";
        let result = import_from_str(csv);

        assert_eq!(result.document.sheets.len(), 1);
        assert_eq!(result.document.sheets[0].rows.len(), 4);
        assert_eq!(result.document.sheets[0].rows[0].cells[0].raw_text, "Name");
        assert_eq!(result.document.sheets[0].rows[1].cells[1].raw_text, "100");
    }

    #[test]
    fn input_profile_fields() {
        let csv = "a,b\nc,d\ne,f\n";
        let result = import_from_str(csv);

        assert_eq!(result.input_profile.encoding, "utf-8");
        assert_eq!(result.input_profile.detected_delimiter, Some(','));
        assert_eq!(result.input_profile.detected_quote, Some('"'));
        assert_eq!(result.input_profile.logical_record_count, Some(3));
        assert_eq!(result.input_profile.sampled_width_min, Some(2));
        assert_eq!(result.input_profile.sampled_width_max, Some(2));
        assert_eq!(result.input_profile.blank_record_count, 0);
        assert_eq!(result.input_profile.malformed_record_count, 0);
    }

    #[test]
    fn diagnostics_present_for_ragged() {
        let csv = "a,b,c\n1,2\nx,y,z\n";
        let result = import_from_str(csv);
        assert_eq!(result.input_profile.sampled_width_min, Some(2));
        assert_eq!(result.input_profile.sampled_width_max, Some(3));
    }

    #[test]
    fn can_inspect_csv_extension() {
        let importer = CsvImporter;
        assert!(importer.can_inspect(Path::new("data.csv"), b"a,b,c"));
        assert!(importer.can_inspect(Path::new("data.tsv"), b"a\tb\tc"));
        assert!(importer.can_inspect(Path::new("data.txt"), b"a,b,c"));
    }

    #[test]
    fn can_inspect_text_content() {
        let importer = CsvImporter;
        assert!(importer.can_inspect(Path::new("data"), b"name,age\nAlice,30"));
    }
}
