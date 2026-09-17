use baho_ingest_csv::candidates::CandidateConfig;
use baho_model::candidate::TableCandidate;

use crate::error::CoreError;

fn is_nested_suffix_of(candidate: &TableCandidate, table: &TableCandidate) -> bool {
    let (Some(candidate_header), Some(table_header)) =
        (candidate.region.header_row, table.region.header_row)
    else {
        return false;
    };

    candidate_header > table_header
        && candidate_header <= table.region.body_end_row
        && candidate.region.col_start == table.region.col_start
        && candidate.region.col_end == table.region.col_end
        && candidate.region.body_end_row == table.region.body_end_row
}

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

    // A text-only body row can look like another header. When that candidate
    // describes only a suffix of the best candidate's same column span and
    // body end, it is an alternate header interpretation of one table rather
    // than evidence for a separate table.
    if let Some(second) = candidates
        .iter()
        .skip(1)
        .find(|candidate| !is_nested_suffix_of(candidate, best))
    {
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
    use baho_ingest_csv::{LogicalRecord, compute_row_features, detect_candidates};
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

    #[test]
    fn all_text_body_rows_do_not_make_the_table_ambiguous() {
        let records = [
            ["Name", "City"],
            ["Ada", "London"],
            ["Bob", "Paris"],
            ["Eve", "Berlin"],
            ["Lin", "Taipei"],
            ["Sam", "Lagos"],
        ]
        .into_iter()
        .enumerate()
        .map(|(index, fields)| LogicalRecord {
            index,
            fields: fields.into_iter().map(str::to_owned).collect(),
            is_blank: false,
        })
        .collect::<Vec<_>>();
        let features = compute_row_features(&records);
        let config = CandidateConfig::default();
        let candidates = detect_candidates(&records, &features, &config);

        assert!(
            candidates[0].score.total - candidates[1].score.total < config.ambiguity_margin,
            "fixture must exercise close-scoring nested header candidates"
        );

        let selected = select_candidate(&candidates, &config).unwrap();
        assert_eq!(selected.region.header_row, Some(0));
        assert_eq!(selected.region.body_start_row, 1);
        assert_eq!(selected.region.body_end_row, 5);
    }
}
