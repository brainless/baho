use std::path::Path;

use baho_ingest::InspectOptions;
use serde::{Deserialize, Serialize};

use crate::candidates::CandidateConfig;
use crate::dialect::{CsvDialect, DialectDetectionConfig, DialectDetectionError};

/// Complete, versioned configuration for one CSV parsing run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParserConfig {
    pub schema_version: u32,
    pub dialect: CsvDialect,
    pub dialect_detection: DialectDetectionConfig,
    pub inspection: InspectOptions,
    pub candidate_detection: CandidateConfig,
    pub row_classification: RowClassificationConfig,
    pub candidate_scoring: CandidateScoringConfig,
    pub normalization: NormalizationConfig,
    pub candidate_ordering: CandidateOrderingConfig,
    pub evidence_limits: EvidenceLimitsConfig,
}

impl ParserConfig {
    /// Build a configuration whose selected dialect is derived from the
    /// source content using the persisted detection settings.
    pub fn detect(path: &Path, inspection: InspectOptions) -> Result<Self, DialectDetectionError> {
        let mut config = Self {
            inspection,
            ..Self::default()
        };
        config.dialect = CsvDialect::detect(path, &config.dialect_detection)?;
        Ok(config)
    }
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            dialect: CsvDialect::default(),
            dialect_detection: DialectDetectionConfig::default(),
            inspection: InspectOptions::default(),
            candidate_detection: CandidateConfig::default(),
            row_classification: RowClassificationConfig::default(),
            candidate_scoring: CandidateScoringConfig::default(),
            normalization: NormalizationConfig::default(),
            candidate_ordering: CandidateOrderingConfig::default(),
            evidence_limits: EvidenceLimitsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLimitsConfig {
    pub max_blank_record_indices: usize,
    pub max_row_classifications: usize,
}

impl Default for EvidenceLimitsConfig {
    fn default() -> Self {
        Self {
            max_blank_record_indices: 10_000,
            max_row_classifications: 1_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowClassificationConfig {
    pub min_data_density: f64,
    pub max_body_width_difference: usize,
}

impl Default for RowClassificationConfig {
    fn default() -> Self {
        Self {
            min_data_density: 0.3,
            max_body_width_difference: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateScoringConfig {
    pub weights: CandidateScoreWeights,
}

impl Default for CandidateScoringConfig {
    fn default() -> Self {
        Self {
            weights: CandidateScoreWeights::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateScoreWeights {
    pub header_density: f64,
    pub body_width_stability: f64,
    pub body_shape_consistency: f64,
    pub header_body_distance: f64,
    pub body_row_count: f64,
    pub header_position: f64,
}

impl Default for CandidateScoreWeights {
    fn default() -> Self {
        Self {
            header_density: 0.22,
            body_width_stability: 0.10,
            body_shape_consistency: 0.10,
            header_body_distance: 0.10,
            body_row_count: 0.32,
            header_position: 0.16,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizationConfig {
    pub blank_values: BlankValueRule,
    pub header_whitespace: WhitespaceRule,
    pub feature_whitespace: WhitespaceRule,
    pub numeric_values: NumericValueRule,
}

impl Default for NormalizationConfig {
    fn default() -> Self {
        Self {
            blank_values: BlankValueRule::TrimmedUnicodeWhitespace,
            header_whitespace: WhitespaceRule::TrimAndCollapse,
            feature_whitespace: WhitespaceRule::TrimAndCollapse,
            numeric_values: NumericValueRule::RustF64,
        }
    }
}

impl NormalizationConfig {
    pub fn is_blank(&self, value: &str) -> bool {
        match self.blank_values {
            BlankValueRule::TrimmedUnicodeWhitespace => value.trim().is_empty(),
        }
    }

    pub(crate) fn normalize_header(&self, value: &str) -> String {
        normalize_whitespace(value, self.header_whitespace)
    }

    pub(crate) fn normalize_feature(&self, value: &str) -> String {
        normalize_whitespace(value, self.feature_whitespace)
    }

    pub(crate) fn is_numeric(&self, value: &str) -> bool {
        match self.numeric_values {
            NumericValueRule::RustF64 => value.trim().parse::<f64>().is_ok(),
        }
    }
}

fn normalize_whitespace(value: &str, rule: WhitespaceRule) -> String {
    match rule {
        WhitespaceRule::TrimAndCollapse => {
            let mut result = String::with_capacity(value.len());
            for (index, part) in value.split_whitespace().enumerate() {
                if index > 0 {
                    result.push(' ');
                }
                result.push_str(part);
            }
            result
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlankValueRule {
    TrimmedUnicodeWhitespace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhitespaceRule {
    TrimAndCollapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumericValueRule {
    RustF64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateOrderingConfig {
    pub primary: CandidatePrimaryOrder,
    pub tie_breaker: CandidateTieBreaker,
}

impl Default for CandidateOrderingConfig {
    fn default() -> Self {
        Self {
            primary: CandidatePrimaryOrder::ScoreDescending,
            tie_breaker: CandidateTieBreaker::SourceOrder,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidatePrimaryOrder {
    ScoreDescending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateTieBreaker {
    SourceOrder,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_configuration_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.tsv");
        std::fs::write(&path, "Name\tValue\nAlice\t1\nBob\t2\n").unwrap();
        let config = ParserConfig::detect(
            &path,
            InspectOptions {
                max_sample_records: 25,
                max_field_size: 512,
                force_encoding: Some("utf-8".to_string()),
            },
        )
        .unwrap();

        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["dialect"]["delimiter"], b'\t');
        assert_eq!(json["dialect_detection"]["max_bytes"], 65_536);
        assert_eq!(json["dialect_detection"]["max_records"], 64);
        assert_eq!(json["inspection"]["max_field_size"], 512);
        assert_eq!(json["candidate_scoring"]["weights"]["header_density"], 0.22);
        assert_eq!(json["candidate_ordering"]["tie_breaker"], "source_order");
        assert_eq!(json["evidence_limits"]["max_blank_record_indices"], 10_000);

        let decoded: ParserConfig = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, config);
    }
}
