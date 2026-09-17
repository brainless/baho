use serde::{Deserialize, Serialize};

use crate::grid::GridRegion;

/// A detected table region within a sheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableCandidate {
    pub id: String,
    pub region: GridRegion,
    pub header: HeaderDecision,
    pub body_row_classifications: Vec<RowClassification>,
    pub score: CandidateScore,
    /// Whether this candidate was selected for materialization.
    pub selected: bool,
}

/// Which row was chosen as the header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeaderDecision {
    /// Zero-based row index in the source sheet.
    pub source_row: usize,
    pub cells: Vec<HeaderCell>,
}

/// Detail about a single header cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeaderCell {
    /// Zero-based column index.
    pub col: usize,
    pub raw_text: String,
    pub normalized_text: String,
    /// Stable column identifier derived from this header.
    pub column_id: String,
}

/// Classification of a body row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowClassification {
    /// Zero-based source row index.
    pub source_row: usize,
    pub kind: RowKind,
    pub reason: Option<String>,
}

/// The kind of a body row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RowKind {
    Data,
    BlankSeparator,
    Footer,
    Note,
}

/// Structured score for a table candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateScore {
    pub total: f64,
    pub components: Vec<ScoreComponent>,
}

/// Individual factor contributing to a candidate score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreComponent {
    pub name: String,
    pub value: f64,
    pub evidence: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::GridRegion;

    fn sample_candidate() -> TableCandidate {
        TableCandidate {
            id: "candidate-0".to_string(),
            region: GridRegion {
                id: "region-0".to_string(),
                header_row: Some(0),
                body_start_row: 1,
                body_end_row: 5,
                col_start: 0,
                col_end: 2,
            },
            header: HeaderDecision {
                source_row: 0,
                cells: vec![
                    HeaderCell {
                        col: 0,
                        raw_text: "Name".to_string(),
                        normalized_text: "name".to_string(),
                        column_id: "column-0".to_string(),
                    },
                    HeaderCell {
                        col: 1,
                        raw_text: "Age".to_string(),
                        normalized_text: "age".to_string(),
                        column_id: "column-1".to_string(),
                    },
                ],
            },
            body_row_classifications: vec![
                RowClassification {
                    source_row: 1,
                    kind: RowKind::Data,
                    reason: None,
                },
                RowClassification {
                    source_row: 2,
                    kind: RowKind::Data,
                    reason: None,
                },
            ],
            score: CandidateScore {
                total: 0.85,
                components: vec![ScoreComponent {
                    name: "header_consistency".to_string(),
                    value: 0.9,
                    evidence: Some("all cells non-empty".to_string()),
                }],
            },
            selected: true,
        }
    }

    #[test]
    fn construct_table_candidate() {
        let c = sample_candidate();
        assert_eq!(c.id, "candidate-0");
        assert!(c.selected);
        assert_eq!(c.header.cells.len(), 2);
    }

    #[test]
    fn row_classifications() {
        let c = sample_candidate();
        assert_eq!(c.body_row_classifications[0].kind, RowKind::Data);
        assert_eq!(c.body_row_classifications.len(), 2);
    }

    #[test]
    fn row_kind_variants() {
        let kinds = [
            RowKind::Data,
            RowKind::BlankSeparator,
            RowKind::Footer,
            RowKind::Note,
        ];
        assert_eq!(kinds.len(), 4);
    }

    #[test]
    fn score_components() {
        let c = sample_candidate();
        assert_eq!(c.score.total, 0.85);
        assert_eq!(c.score.components[0].name, "header_consistency");
        assert_eq!(c.score.components[0].value, 0.9);
    }

    #[test]
    fn serde_round_trip() {
        let c = sample_candidate();
        let json = serde_json::to_string(&c).unwrap();
        let back: TableCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }
}
