use serde::{Deserialize, Serialize};

/// Evidence recorded during deterministic intent recognition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecognitionEvidence {
    /// Normalized prompt tokens with their original indices.
    pub prompt_tokens: Vec<PromptToken>,
    /// The action alias and token span.
    pub action: Option<ActionEvidence>,
    /// The optional modifier alias and token span.
    pub modifier: Option<ModifierEvidence>,
    /// The selected column phrase and token span.
    pub column_phrase: Option<ColumnPhraseEvidence>,
    /// The stable column ID and display name.
    pub matched_column: Option<MatchedColumn>,
    /// Match class ("exact" or "terminal_s_variant") and score.
    pub match_class: Option<String>,
    /// Canonical operation ("select" or "distinct").
    pub canonical_operation: Option<String>,
    /// Why recognition failed, if it did.
    pub refusal_reason: Option<String>,
    /// Bounded competing parses when ambiguity causes refusal.
    pub competing_parses: Vec<CompetingParseEvidence>,
}

/// A normalized prompt token with its original index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptToken {
    /// Original token index in the prompt.
    pub index: usize,
    /// Normalized (lowercased) text.
    pub text: String,
}

/// Evidence of the matched action alias.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionEvidence {
    /// The surface alias that was matched (e.g. "list").
    pub alias: String,
    /// Token span (start, end) — end is exclusive.
    pub span: (usize, usize),
}

/// Evidence of the matched modifier alias.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModifierEvidence {
    /// The surface alias that was matched (e.g. "unique").
    pub alias: String,
    /// Token span (start, end).
    pub span: (usize, usize),
}

/// Evidence of the selected column phrase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnPhraseEvidence {
    /// The contiguous prompt tokens forming the column phrase.
    pub tokens: Vec<String>,
    /// Token span (start, end).
    pub span: (usize, usize),
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
    /// How the match was made (e.g. "exact contiguous phrase" or "terminal-s variant").
    pub evidence: String,
}

/// A competing parse when ambiguity causes refusal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompetingParseEvidence {
    /// Column display name of the competing parse.
    pub column_display_name: String,
    /// Score of the competing parse.
    pub score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_evidence() -> RecognitionEvidence {
        RecognitionEvidence {
            prompt_tokens: vec![
                PromptToken {
                    index: 0,
                    text: "extract".into(),
                },
                PromptToken {
                    index: 1,
                    text: "all".into(),
                },
                PromptToken {
                    index: 2,
                    text: "floor".into(),
                },
                PromptToken {
                    index: 3,
                    text: "plans".into(),
                },
            ],
            action: Some(ActionEvidence {
                alias: "extract".into(),
                span: (0, 1),
            }),
            modifier: Some(ModifierEvidence {
                alias: "unique".into(),
                span: (2, 3),
            }),
            column_phrase: Some(ColumnPhraseEvidence {
                tokens: vec!["floor".into(), "plans".into()],
                span: (2, 4),
            }),
            matched_column: Some(MatchedColumn {
                column_id: "column-1".into(),
                display_name: "Floor Plan".into(),
                score: 0.9,
                evidence: "terminal-s variant".into(),
            }),
            match_class: Some("terminal_s_variant".into()),
            canonical_operation: Some("distinct".into()),
            refusal_reason: None,
            competing_parses: Vec::new(),
        }
    }

    #[test]
    fn recognition_evidence_with_matched_column() {
        let evidence = sample_evidence();
        assert!(evidence.matched_column.is_some());
        assert!(evidence.refusal_reason.is_none());
        assert_eq!(evidence.canonical_operation.as_deref(), Some("distinct"));
        assert!(evidence.action.is_some());
        assert!(evidence.modifier.is_some());
        assert!(evidence.column_phrase.is_some());
    }

    #[test]
    fn recognition_evidence_with_refusal() {
        let evidence = RecognitionEvidence {
            prompt_tokens: vec![
                PromptToken {
                    index: 0,
                    text: "do".into(),
                },
                PromptToken {
                    index: 1,
                    text: "something".into(),
                },
            ],
            action: None,
            modifier: None,
            column_phrase: None,
            matched_column: None,
            match_class: None,
            canonical_operation: None,
            refusal_reason: Some("unsupported intent phrasing".into()),
            competing_parses: Vec::new(),
        };
        assert!(evidence.matched_column.is_none());
        assert!(evidence.refusal_reason.is_some());
    }

    #[test]
    fn recognition_evidence_with_competing_parses() {
        let evidence = RecognitionEvidence {
            prompt_tokens: vec![PromptToken {
                index: 0,
                text: "list".into(),
            }],
            action: None,
            modifier: None,
            column_phrase: None,
            matched_column: None,
            match_class: None,
            canonical_operation: None,
            refusal_reason: Some("ambiguous parse".into()),
            competing_parses: vec![
                CompetingParseEvidence {
                    column_display_name: "Floor".into(),
                    score: 1.0,
                },
                CompetingParseEvidence {
                    column_display_name: "floor".into(),
                    score: 1.0,
                },
            ],
        };
        assert_eq!(evidence.competing_parses.len(), 2);
    }

    #[test]
    fn serde_round_trip() {
        let evidence = sample_evidence();
        let json = serde_json::to_string(&evidence).unwrap();
        let back: RecognitionEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(evidence, back);
    }
}
