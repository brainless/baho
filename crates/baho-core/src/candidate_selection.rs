use baho_ingest_csv::candidates::CandidateConfig;
use baho_model::candidate::TableCandidate;

use crate::error::CoreError;

/// Select the best table candidate from detected candidates.
pub fn select_candidate<'a>(
    candidates: &'a [TableCandidate],
    config: &CandidateConfig,
) -> Result<&'a TableCandidate, CoreError> {
    if candidates.is_empty() {
        return Err(CoreError::NoTableFound);
    }

    // Candidates are already sorted by score descending, then source order.
    let best = &candidates[0];

    if best.score.total < config.min_score_threshold {
        return Err(CoreError::NoTableFound);
    }

    if candidates.len() >= 2 {
        let second = &candidates[1];
        if (best.score.total - second.score.total).abs() < config.ambiguity_margin {
            return Err(CoreError::AmbiguousTable {
                candidate_count: candidates.len(),
            });
        }
    }

    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_model::candidate::{CandidateScore, HeaderDecision};
    use baho_model::grid::GridRegion;

    fn make_candidate(id: &str, score: f64) -> TableCandidate {
        TableCandidate {
            id: id.to_string(),
            region: GridRegion {
                id: format!("region-{}", id),
                header_row: Some(0),
                body_start_row: 1,
                body_end_row: 5,
                col_start: 0,
                col_end: 2,
            },
            header: HeaderDecision {
                source_row: 0,
                cells: Vec::new(),
            },
            body_row_classifications: Vec::new(),
            score: CandidateScore {
                total: score,
                components: Vec::new(),
            },
            selected: false,
        }
    }

    #[test]
    fn single_high_score_selected() {
        let candidates = vec![make_candidate("c0", 0.9)];
        let config = CandidateConfig::default();
        let result = select_candidate(&candidates, &config).unwrap();
        assert_eq!(result.id, "c0");
    }

    #[test]
    fn no_candidates_returns_error() {
        let candidates = [];
        let config = CandidateConfig::default();
        let err = select_candidate(&candidates, &config).unwrap_err();
        assert!(matches!(err, CoreError::NoTableFound));
    }

    #[test]
    fn low_score_returns_no_table_found() {
        let candidates = vec![make_candidate("c0", 0.1)];
        let config = CandidateConfig::default();
        let err = select_candidate(&candidates, &config).unwrap_err();
        assert!(matches!(err, CoreError::NoTableFound));
    }

    #[test]
    fn two_close_scores_returns_ambiguous() {
        let candidates = vec![make_candidate("c0", 0.85), make_candidate("c1", 0.82)];
        let config = CandidateConfig::default();
        let err = select_candidate(&candidates, &config).unwrap_err();
        assert!(matches!(err, CoreError::AmbiguousTable { .. }));
    }

    #[test]
    fn two_distinct_scores_top_selected() {
        let candidates = vec![make_candidate("c0", 0.95), make_candidate("c1", 0.5)];
        let config = CandidateConfig::default();
        let result = select_candidate(&candidates, &config).unwrap();
        assert_eq!(result.id, "c0");
    }
}
