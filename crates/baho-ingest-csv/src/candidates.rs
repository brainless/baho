use serde::{Deserialize, Serialize};

use baho_model::candidate::{CandidateScore, ScoreComponent, TableCandidate};
use baho_model::grid::GridRegion;

use crate::inspector::LogicalRecord;
use crate::row_features::{ColumnShape, RowFeatures};

/// Configuration for candidate detection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateConfig {
    pub min_body_rows: usize,
    pub min_header_density: f64,
    pub max_header_to_body_gap: usize,
    pub blank_gap_lookahead: usize,
    pub footer_lookahead: usize,
    pub max_body_width_difference: usize,
    pub min_score_threshold: f64,
    pub ambiguity_margin: f64,
}

impl Default for CandidateConfig {
    fn default() -> Self {
        Self {
            min_body_rows: 1,
            min_header_density: 0.5,
            max_header_to_body_gap: 5,
            blank_gap_lookahead: 10,
            footer_lookahead: 10,
            max_body_width_difference: 2,
            min_score_threshold: 0.3,
            ambiguity_margin: 0.1,
        }
    }
}

fn is_header_like(feature: &RowFeatures, config: &CandidateConfig) -> bool {
    feature.density >= config.min_header_density
        && feature
            .column_shapes
            .iter()
            .all(|s| matches!(s, ColumnShape::Text | ColumnShape::Blank))
}

fn is_body_compatible(
    header_width: usize,
    feature: &RowFeatures,
    config: &CandidateConfig,
) -> bool {
    if feature.is_blank {
        return false;
    }
    let width_diff = if feature.physical_width > header_width {
        feature.physical_width - header_width
    } else {
        header_width - feature.physical_width
    };
    width_diff <= config.max_body_width_difference
}

fn score_candidate(
    header_idx: usize,
    body_rows: &[usize],
    features: &[RowFeatures],
    _records: &[LogicalRecord],
    scoring: &crate::config::CandidateScoringConfig,
) -> CandidateScore {
    let header = &features[header_idx];
    let _header_width = header.physical_width;

    let header_density = header.density;

    let body_widths: Vec<usize> = body_rows
        .iter()
        .map(|&i| features[i].physical_width)
        .collect();
    let body_width_stability = if body_widths.is_empty() {
        0.0
    } else {
        let avg = body_widths.iter().sum::<usize>() as f64 / body_widths.len() as f64;
        let variance = body_widths
            .iter()
            .map(|w| {
                let diff = *w as f64 - avg;
                diff * diff
            })
            .sum::<f64>()
            / body_widths.len() as f64;
        1.0 / (1.0 + variance.sqrt())
    };

    let body_shape_consistency = if body_rows.len() < 2 {
        1.0
    } else {
        let mut agreements = 0usize;
        let mut comparisons = 0usize;
        let max_col = body_rows
            .iter()
            .map(|&i| features[i].column_shapes.len())
            .max()
            .unwrap_or(0);
        for col in 0..max_col {
            let shapes: Vec<&ColumnShape> = body_rows
                .iter()
                .filter_map(|&i| features[i].column_shapes.get(col))
                .collect();
            if shapes.len() >= 2 {
                for i in 0..shapes.len() - 1 {
                    comparisons += 1;
                    if std::mem::discriminant(shapes[i]) == std::mem::discriminant(shapes[i + 1]) {
                        agreements += 1;
                    }
                }
            }
        }
        if comparisons == 0 {
            1.0
        } else {
            agreements as f64 / comparisons as f64
        }
    };

    let first_body = body_rows.first().copied().unwrap_or(header_idx + 1);
    let gap = first_body.saturating_sub(header_idx + 1);
    let header_body_distance = 1.0 / (1.0 + gap as f64);

    let remaining_rows = features.len().saturating_sub(header_idx);
    let body_row_count = if remaining_rows > 0 {
        body_rows.len() as f64 / remaining_rows as f64
    } else {
        0.0
    };

    let first_nonblank = features.iter().position(|f| !f.is_blank).unwrap_or(0);
    let header_position = 1.0 / (1.0 + header_idx.saturating_sub(first_nonblank) as f64);

    let weights = &scoring.weights;
    let total = weights.header_density * header_density
        + weights.body_width_stability * body_width_stability
        + weights.body_shape_consistency * body_shape_consistency
        + weights.header_body_distance * header_body_distance
        + weights.body_row_count * body_row_count
        + weights.header_position * header_position;

    CandidateScore {
        total,
        components: vec![
            ScoreComponent {
                name: "header_density".to_string(),
                value: header_density,
                evidence: Some(format!(
                    "{}/{} nonblank",
                    header.nonblank_count, header.physical_width
                )),
            },
            ScoreComponent {
                name: "body_width_stability".to_string(),
                value: body_width_stability,
                evidence: Some(format!("{} body rows", body_rows.len())),
            },
            ScoreComponent {
                name: "body_shape_consistency".to_string(),
                value: body_shape_consistency,
                evidence: None,
            },
            ScoreComponent {
                name: "header_body_distance".to_string(),
                value: header_body_distance,
                evidence: Some(format!("gap of {} rows", gap)),
            },
            ScoreComponent {
                name: "body_row_count".to_string(),
                value: body_row_count,
                evidence: Some(format!(
                    "{}/{} remaining rows",
                    body_rows.len(),
                    remaining_rows
                )),
            },
            ScoreComponent {
                name: "header_position".to_string(),
                value: header_position,
                evidence: Some(format!(
                    "header idx {} (first nonblank idx {})",
                    header_idx, first_nonblank
                )),
            },
        ],
    }
}

fn find_body_rows(
    header_idx: usize,
    features: &[RowFeatures],
    _records: &[LogicalRecord],
    config: &CandidateConfig,
) -> Vec<usize> {
    let header_width = features[header_idx].physical_width;
    let mut body_rows = Vec::new();
    let mut blank_count = 0usize;
    let mut consecutive_incompatible = 0usize;

    let start = header_idx + 1;
    let mut rows_since_header = 0usize;

    for i in start..features.len() {
        let feat = &features[i];
        rows_since_header += 1;

        if feat.is_blank {
            blank_count += 1;
            if blank_count > config.blank_gap_lookahead {
                break;
            }
            continue;
        }

        if is_body_compatible(header_width, feat, config) {
            if body_rows.is_empty() && rows_since_header > config.max_header_to_body_gap + 1 {
                break;
            }
            body_rows.push(i);
            blank_count = 0;
            consecutive_incompatible = 0;
        } else {
            consecutive_incompatible += 1;
            if consecutive_incompatible >= config.footer_lookahead {
                break;
            }
        }
    }

    body_rows
}

/// Detect table candidates from records and their features.
pub fn detect_candidates(
    records: &[LogicalRecord],
    features: &[RowFeatures],
    config: &CandidateConfig,
) -> Vec<TableCandidate> {
    detect_candidates_with_config(
        records,
        features,
        config,
        &crate::config::CandidateScoringConfig::default(),
        &crate::config::CandidateOrderingConfig::default(),
    )
}

/// Detect candidates using the scoring weights and deterministic ordering
/// recorded for this parsing run.
pub fn detect_candidates_with_config(
    records: &[LogicalRecord],
    features: &[RowFeatures],
    config: &CandidateConfig,
    scoring: &crate::config::CandidateScoringConfig,
    ordering: &crate::config::CandidateOrderingConfig,
) -> Vec<TableCandidate> {
    let mut candidates = Vec::new();

    for (idx, feat) in features.iter().enumerate() {
        if !is_header_like(feat, config) {
            continue;
        }

        let body_rows = find_body_rows(idx, features, records, config);

        if body_rows.len() < config.min_body_rows {
            continue;
        }

        let score = score_candidate(idx, &body_rows, features, records, scoring);

        let last_body = *body_rows.last().unwrap();
        let max_col = feat.physical_width.saturating_sub(1);

        candidates.push(TableCandidate {
            id: format!("candidate-{}", candidates.len()),
            region: GridRegion {
                id: format!("region-{}", candidates.len()),
                header_row: Some(idx),
                body_start_row: *body_rows.first().unwrap(),
                body_end_row: last_body,
                col_start: 0,
                col_end: max_col,
            },
            header: baho_model::candidate::HeaderDecision {
                source_row: idx,
                cells: Vec::new(),
            },
            body_row_classifications: Vec::new(),
            score,
            selected: false,
        });
    }

    match (ordering.primary, ordering.tie_breaker) {
        (
            crate::config::CandidatePrimaryOrder::ScoreDescending,
            crate::config::CandidateTieBreaker::SourceOrder,
        ) => candidates.sort_by(|a, b| {
            b.score
                .total
                .partial_cmp(&a.score.total)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.region.header_row.cmp(&b.region.header_row))
        }),
    }

    candidates.retain(|c| c.score.total >= config.min_score_threshold);

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::LogicalRecord;

    fn make_record(index: usize, fields: &[&str]) -> LogicalRecord {
        let fields: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let is_blank = fields.iter().all(|f| f.trim().is_empty());
        LogicalRecord {
            index,
            fields,
            is_blank,
        }
    }

    fn features_from_records(records: &[LogicalRecord]) -> Vec<RowFeatures> {
        crate::row_features::compute_row_features(records)
    }

    #[test]
    fn single_table_with_preamble() {
        let records = vec![
            make_record(0, &["Report Title", ""]),
            make_record(1, &["Generated", "2025-01-01"]),
            make_record(2, &["", ""]),
            make_record(3, &["Name", "Value"]),
            make_record(4, &["Alice", "100"]),
            make_record(5, &["Bob", "200"]),
            make_record(6, &["Carol", "300"]),
        ];
        let features = features_from_records(&records);
        let config = CandidateConfig::default();
        let candidates = detect_candidates(&records, &features, &config);

        assert!(!candidates.is_empty());
        let best = &candidates[0];
        assert_eq!(best.region.header_row, Some(3));
        assert_eq!(best.region.body_start_row, 4);
        assert_eq!(best.region.body_end_row, 6);
    }

    #[test]
    fn blank_separators_within_body() {
        let records = vec![
            make_record(0, &["Name", "Value"]),
            make_record(1, &["Alice", "100"]),
            make_record(2, &["", ""]),
            make_record(3, &["Bob", "200"]),
            make_record(4, &["", ""]),
            make_record(5, &["Carol", "300"]),
        ];
        let features = features_from_records(&records);
        let config = CandidateConfig::default();
        let candidates = detect_candidates(&records, &features, &config);

        assert!(!candidates.is_empty());
        let best = &candidates[0];
        assert_eq!(best.region.header_row, Some(0));
        assert!(best.region.body_end_row >= 5);
    }

    #[test]
    fn footer_detection_by_structural_change() {
        let records = vec![
            make_record(0, &["Name", "Value", "Status"]),
            make_record(1, &["Alice", "100", "active"]),
            make_record(2, &["Bob", "200", "active"]),
            make_record(3, &["Carol", "300", "inactive"]),
            make_record(4, &["", "", ""]),
            make_record(5, &["Total:", "600", "", "", "extra"]),
            make_record(6, &["Note:", "values approximate", "", "", ""]),
            make_record(7, &["Note:", "more notes", "", "", ""]),
            make_record(8, &["Note:", "even more", "", "", ""]),
            make_record(9, &["Note:", "final note", "", "", ""]),
            make_record(10, &["Note:", "extra note", "", "", ""]),
            make_record(11, &["Note:", "another", "", "", ""]),
            make_record(12, &["Note:", "yet another", "", "", ""]),
            make_record(13, &["Note:", "keep going", "", "", ""]),
            make_record(14, &["Note:", "more", "", "", ""]),
            make_record(15, &["Note:", "last one", "", "", ""]),
        ];
        let features = features_from_records(&records);
        let config = CandidateConfig::default();
        let candidates = detect_candidates(&records, &features, &config);

        assert!(!candidates.is_empty());
    }

    #[test]
    fn score_ordering() {
        let records = vec![
            make_record(0, &["A", "B"]),
            make_record(1, &["1", "2"]),
            make_record(2, &["X", "Y", "Z", "W"]),
            make_record(3, &["a", "b", "c", "d"]),
            make_record(4, &["e", "f", "g", "h"]),
        ];
        let features = features_from_records(&records);
        let config = CandidateConfig::default();
        let candidates = detect_candidates(&records, &features, &config);

        for i in 0..candidates.len().saturating_sub(1) {
            assert!(candidates[i].score.total >= candidates[i + 1].score.total);
        }
    }

    #[test]
    fn configured_score_weights_determine_the_total() {
        let records = vec![
            make_record(0, &["Name", "Value"]),
            make_record(1, &["Ada", "42"]),
        ];
        let features = features_from_records(&records);
        let config = CandidateConfig::default();
        let scoring = crate::config::CandidateScoringConfig {
            weights: crate::config::CandidateScoreWeights {
                header_density: 1.0,
                body_width_stability: 0.0,
                body_shape_consistency: 0.0,
                header_body_distance: 0.0,
                body_row_count: 0.0,
                header_position: 0.0,
            },
        };
        let candidates = detect_candidates_with_config(
            &records,
            &features,
            &config,
            &scoring,
            &crate::config::CandidateOrderingConfig::default(),
        );

        assert_eq!(candidates[0].score.total, 1.0);
    }

    #[test]
    fn no_candidates_below_threshold() {
        let records = vec![make_record(0, &["a"]), make_record(1, &["b"])];
        let features = features_from_records(&records);
        let config = CandidateConfig {
            min_score_threshold: 0.99,
            ..Default::default()
        };
        let candidates = detect_candidates(&records, &features, &config);
        assert!(candidates.is_empty());
    }

    #[test]
    fn all_text_single_column_not_ambiguous() {
        let records = vec![
            make_record(0, &["Name"]),
            make_record(1, &["Ada"]),
            make_record(2, &["Bob"]),
        ];
        let features = features_from_records(&records);
        let config = CandidateConfig::default();
        let candidates = detect_candidates(&records, &features, &config);

        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].region.header_row, Some(0));
        assert_eq!(candidates[0].region.body_start_row, 1);
        assert_eq!(candidates[0].region.body_end_row, 2);

        if candidates.len() >= 2 {
            let gap = candidates[0].score.total - candidates[1].score.total;
            assert!(
                gap >= config.ambiguity_margin,
                "score gap {} must be >= ambiguity margin {}",
                gap,
                config.ambiguity_margin
            );
        }
    }

    #[test]
    fn all_text_multi_column_not_ambiguous() {
        let records = vec![
            make_record(0, &["Name", "City"]),
            make_record(1, &["Ada", "London"]),
            make_record(2, &["Bob", "Paris"]),
            make_record(3, &["Eve", "Berlin"]),
        ];
        let features = features_from_records(&records);
        let config = CandidateConfig::default();
        let candidates = detect_candidates(&records, &features, &config);

        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].region.header_row, Some(0));
        assert_eq!(candidates[0].region.body_start_row, 1);
        assert_eq!(candidates[0].region.body_end_row, 3);

        if candidates.len() >= 2 {
            let gap = candidates[0].score.total - candidates[1].score.total;
            assert!(
                gap >= config.ambiguity_margin,
                "score gap {} must be >= ambiguity margin {}",
                gap,
                config.ambiguity_margin
            );
        }
    }
}
