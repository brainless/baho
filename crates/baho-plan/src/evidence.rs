use serde::{Deserialize, Serialize};

/// Evidence recorded during deterministic intent recognition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecognitionEvidence {
    /// Tokens extracted from the user prompt.
    pub prompt_tokens: Vec<String>,
    /// The column that was matched, if any.
    pub matched_column: Option<MatchedColumn>,
    /// The recognized operation (e.g. "distinct"), if any.
    pub operation: Option<String>,
    /// Why recognition failed, if it did.
    pub refusal_reason: Option<String>,
}

/// Details of a column matched during recognition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchedColumn {
    /// Stable column ID (e.g. "column-1").
    pub column_id: String,
    /// Display name (e.g. "Floor Plan").
    pub display_name: String,
    /// Match confidence score.
    pub score: f64,
    /// How the match was made (e.g. "token overlap: 'floor', 'plans'").
    pub evidence: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognition_evidence_with_matched_column() {
        let evidence = RecognitionEvidence {
            prompt_tokens: vec![
                "extract".into(),
                "all".into(),
                "floor".into(),
                "plans".into(),
            ],
            matched_column: Some(MatchedColumn {
                column_id: "column-1".into(),
                display_name: "Floor Plan".into(),
                score: 0.9,
                evidence: "token overlap: 'floor', 'plans'".into(),
            }),
            operation: Some("distinct".into()),
            refusal_reason: None,
        };
        assert!(evidence.matched_column.is_some());
        assert!(evidence.refusal_reason.is_none());
        assert_eq!(evidence.operation.as_deref(), Some("distinct"));
    }

    #[test]
    fn recognition_evidence_with_refusal() {
        let evidence = RecognitionEvidence {
            prompt_tokens: vec!["do".into(), "something".into()],
            matched_column: None,
            operation: None,
            refusal_reason: Some("unsupported intent phrasing".into()),
        };
        assert!(evidence.matched_column.is_none());
        assert!(evidence.refusal_reason.is_some());
    }

    #[test]
    fn serde_round_trip() {
        let evidence = RecognitionEvidence {
            prompt_tokens: vec!["unique".into(), "values".into()],
            matched_column: Some(MatchedColumn {
                column_id: "column-0".into(),
                display_name: "Name".into(),
                score: 1.0,
                evidence: "exact match".into(),
            }),
            operation: Some("distinct".into()),
            refusal_reason: None,
        };
        let json = serde_json::to_string(&evidence).unwrap();
        let back: RecognitionEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(evidence, back);
    }
}
