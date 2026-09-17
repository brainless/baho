use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use baho_ingest::ImportedDocument;
use baho_ingest::error::ImportError;
use baho_ingest::profile::{InputProfile, InspectOptions};
use baho_ingest::traits::{FormatImporter, FormatInspector};
use baho_model::candidate::{HeaderDecision, RowClassification, RowKind};
use baho_model::diagnostic::Diagnostic;
use baho_model::document::{Cell, CellAddress, Document, Row, Sheet};
use baho_model::revision::SourceRevision;

use crate::candidates::detect_candidates_with_config;
use crate::classifier::classify_rows_with_config;
use crate::config::ParserConfig;
use crate::header::build_header_with_config;
use crate::inspector::inspect_csv_with_config;
use crate::row_features::compute_row_features_with_config;

/// Errors encountered while reparsing the selected source region.
#[derive(Debug, thiserror::Error)]
pub enum SelectedRegionError {
    #[error("I/O error reading `{path}`: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("record {row} is malformed: {detail}")]
    MalformedRecord { row: usize, detail: String },

    #[error(
        "record {row} field at column {col} exceeds max_field_size ({size} bytes > {max_size} bytes)"
    )]
    FieldTooLarge {
        row: usize,
        col: usize,
        size: usize,
        max_size: usize,
    },
}

/// Selected records and bounded classification evidence from a streaming pass.
#[derive(Debug)]
pub struct SelectedRegion {
    pub data_records: Vec<crate::inspector::LogicalRecord>,
    pub body_end_row: usize,
    pub classifications: Vec<RowClassification>,
    pub classification_count: usize,
}

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
        let config = ParserConfig::detect(path, options.clone()).map_err(|error| {
            ImportError::FormatDetectionFailed {
                detail: error.to_string(),
            }
        })?;
        self.import_with_config(path, &config)
    }
}

impl CsvImporter {
    pub fn import_with_config(
        &self,
        path: &Path,
        config: &ParserConfig,
    ) -> Result<ImportedDocument, ImportError> {
        let inspection = inspect_csv_with_config(
            path,
            &config.dialect,
            &config.inspection,
            &config.normalization,
            config.evidence_limits.max_blank_record_indices,
        )?;
        let features =
            compute_row_features_with_config(&inspection.sampled_records, &config.normalization);

        let mut candidates = detect_candidates_with_config(
            &inspection.sampled_records,
            &features,
            &config.candidate_detection,
            &config.candidate_scoring,
            &config.candidate_ordering,
        );

        let mut all_diagnostics: Vec<Diagnostic> = inspection.diagnostics.clone();

        let (_header_decision, header_diag, _body_classifications) = if let Some(best) =
            candidates.first_mut()
        {
            let header_idx = best.region.header_row.unwrap_or(0);
            let header_record = &inspection.sampled_records[header_idx];
            let header_feature = &features[header_idx];
            let (decision, diag) =
                build_header_with_config(header_feature, header_record, 0, &config.normalization);
            let classifications = classify_rows_with_config(
                &features,
                header_idx,
                &config.candidate_detection,
                &config.row_classification,
            );

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

        // The imported document is the bounded analysis surface, not a full
        // in-memory copy of the source. Core reparses only the selected region
        // after candidate selection.
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
            detected_delimiter: Some(config.dialect.delimiter as char),
            detected_quote: Some(config.dialect.quote as char),
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
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = fs::File::open(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| ImportError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Stream and materialize the rows belonging to a selected table candidate.
///
/// Classification evidence is capped independently of the selected data. The
/// latter is the only unbounded retained structure because it is the region
/// explicitly selected for execution.
pub fn read_selected_region(
    path: &Path,
    header_width: usize,
    body_start_row: usize,
    config: &ParserConfig,
) -> Result<SelectedRegion, SelectedRegionError> {
    let file = fs::File::open(path).map_err(|source| SelectedRegionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(config.dialect.delimiter)
        .quote(config.dialect.quote)
        .escape(Some(config.dialect.quote_escape))
        .has_headers(false)
        .flexible(true)
        .from_reader(BufReader::new(file));

    let mut data_records = Vec::new();
    let mut classifications = Vec::new();
    let mut classification_count = 0usize;
    let mut body_end_row = body_start_row;
    let mut blank_count = 0usize;
    let mut incompatible_count = 0usize;
    let mut classifier_footer_count = 0usize;

    for (index, result) in reader.records().enumerate() {
        if index < body_start_row {
            continue;
        }
        let record = result.map_err(|error| SelectedRegionError::MalformedRecord {
            row: index,
            detail: error.to_string(),
        })?;
        if let Some((col, field)) = record
            .iter()
            .enumerate()
            .find(|(_, field)| field.len() > config.inspection.max_field_size)
        {
            return Err(SelectedRegionError::FieldTooLarge {
                row: index,
                col,
                size: field.len(),
                max_size: config.inspection.max_field_size,
            });
        }
        let fields = record.iter().map(str::to_owned).collect::<Vec<_>>();
        let is_blank = fields
            .iter()
            .all(|field| config.normalization.is_blank(field));

        if is_blank {
            blank_count += 1;
            classifier_footer_count = 0;
            record_classification(
                &mut classifications,
                &mut classification_count,
                config.evidence_limits.max_row_classifications,
                RowClassification {
                    source_row: index,
                    kind: RowKind::BlankSeparator,
                    reason: Some("all fields blank".to_string()),
                },
            );
            if blank_count > config.candidate_detection.blank_gap_lookahead {
                break;
            }
            continue;
        }

        let width_diff = fields.len().abs_diff(header_width);
        let width_compatible = width_diff <= config.row_classification.max_body_width_difference;
        if width_compatible {
            body_end_row = index;
            blank_count = 0;
            incompatible_count = 0;
        } else {
            incompatible_count += 1;
            if incompatible_count >= config.candidate_detection.footer_lookahead {
                break;
            }
        }

        let nonblank_count = fields
            .iter()
            .filter(|field| !config.normalization.is_blank(field))
            .count();
        let density = if fields.is_empty() {
            0.0
        } else {
            nonblank_count as f64 / fields.len() as f64
        };
        let density_drop = density < config.row_classification.min_data_density;
        let classification = if !width_compatible || density_drop {
            classifier_footer_count += 1;
            RowClassification {
                source_row: index,
                kind: if classifier_footer_count >= config.candidate_detection.footer_lookahead {
                    RowKind::Footer
                } else {
                    RowKind::Note
                },
                reason: Some(if !width_compatible {
                    format!("width {} vs header width {}", fields.len(), header_width)
                } else {
                    format!("low density {density:.2}")
                }),
            }
        } else {
            classifier_footer_count = 0;
            RowClassification {
                source_row: index,
                kind: RowKind::Data,
                reason: None,
            }
        };

        if classification.kind == RowKind::Data {
            data_records.push(crate::inspector::LogicalRecord {
                index,
                fields,
                is_blank: false,
            });
        }
        record_classification(
            &mut classifications,
            &mut classification_count,
            config.evidence_limits.max_row_classifications,
            classification,
        );
    }

    Ok(SelectedRegion {
        data_records,
        body_end_row,
        classifications,
        classification_count,
    })
}

fn record_classification(
    retained: &mut Vec<RowClassification>,
    total: &mut usize,
    limit: usize,
    classification: RowClassification,
) {
    *total += 1;
    if retained.len() < limit {
        retained.push(classification);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::CsvDialect;
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

    #[test]
    fn retains_only_the_bounded_analysis_sample() {
        let mut csv = String::from("id,value\n");
        for i in 0..1500 {
            csv.push_str(&format!("{},v{}\n", i, i));
        }
        let result = import_from_str(&csv);

        assert_eq!(
            result.document.sheets[0].rows.len(),
            InspectOptions::default().max_sample_records
        );
        assert_eq!(result.input_profile.logical_record_count, Some(1501));
        assert!(
            result
                .input_profile
                .limits_reached
                .contains(&"max_sample_records".to_string())
        );
        assert_eq!(result.document.sheets[0].rows[0].cells[0].raw_text, "id");
        assert_eq!(result.document.sheets[0].rows[1].cells[0].raw_text, "0");
        assert_eq!(
            result.document.sheets[0].rows[999].cells[1].raw_text,
            "v998"
        );
    }

    #[test]
    fn selected_region_rejects_a_field_over_the_configured_limit() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "Name,Code").unwrap();
        writeln!(file, "Alice,A").unwrap();
        writeln!(file, "Bob,{}", "x".repeat(101)).unwrap();

        let error = read_selected_region(
            file.path(),
            2,
            1,
            &ParserConfig {
                dialect: CsvDialect::default(),
                inspection: InspectOptions {
                    max_field_size: 100,
                    ..InspectOptions::default()
                },
                ..ParserConfig::default()
            },
        )
        .expect_err("oversized fields must not reach execution");

        assert!(matches!(
            error,
            SelectedRegionError::FieldTooLarge {
                row: 2,
                col: 1,
                size: 101,
                max_size: 100,
            }
        ));
    }

    fn import_tsv_from_str(tsv: &str) -> ImportedDocument {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.tsv");
        std::fs::write(&path, tsv).unwrap();
        let importer = CsvImporter;
        let options = InspectOptions::default();
        importer.import(&path, &options).unwrap()
    }

    #[test]
    fn tsv_import_parses_tab_delimited() {
        let tsv = "Name\tValue\nAlice\t100\nBob\t200\n";
        let result = import_tsv_from_str(tsv);

        assert_eq!(result.document.sheets[0].rows.len(), 3);
        assert_eq!(result.document.sheets[0].rows[0].cells[0].raw_text, "Name");
        assert_eq!(result.document.sheets[0].rows[0].cells[1].raw_text, "Value");
        assert_eq!(result.document.sheets[0].rows[1].cells[0].raw_text, "Alice");
        assert_eq!(result.document.sheets[0].rows[1].cells[1].raw_text, "100");
        assert_eq!(result.input_profile.detected_delimiter, Some('\t'));
    }

    #[test]
    fn import_uses_content_detected_delimiter_despite_csv_extension() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.csv");
        std::fs::write(
            &path,
            "Name;Comment\nAlice;\"contains, punctuation\"\nBob;plain\n",
        )
        .unwrap();

        let result = CsvImporter
            .import(&path, &InspectOptions::default())
            .unwrap();

        assert_eq!(result.input_profile.detected_delimiter, Some(';'));
        assert_eq!(result.document.sheets[0].rows[0].cells.len(), 2);
        assert_eq!(
            result.document.sheets[0].rows[1].cells[1].raw_text,
            "contains, punctuation"
        );
    }
}
