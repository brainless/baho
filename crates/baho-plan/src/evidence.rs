use baho_model::{NumericParsePolicy, TextMatchPolicy};
use serde::{Deserialize, Serialize};

use crate::plan::{ComparisonOperator, Expression, Literal, PLAN_SCHEMA_VERSION_3};

/// Current recognition-evidence envelope schema version.
///
/// Version 1 is the historical unversioned Epic 002 retrieval-only shape;
/// older artifacts are never rewritten or reinterpreted. Version 2 adds the
/// explicit `schema_version` field and the row-filter evidence of Epic 006.
/// Version 3 (Epic 008 locked decision 14) covers the text-match policy,
/// deferred numeric literals, and the extended `NumericParsePolicy` value
/// space. Version 2 artifacts without `text_match` deserialize with
/// [`TextMatchPolicy::Exact`], their historical semantics.
pub const RECOGNITION_EVIDENCE_SCHEMA_VERSION: u32 = 3;

/// Evidence recorded during deterministic intent recognition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecognitionEvidence {
    /// Schema version of this evidence envelope.
    pub schema_version: u32,
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
    /// Row-filter recognition evidence for the explicit-column Boolean filter
    /// path; `None` on the Epic 002 retrieval path.
    pub row_filter: Option<RowFilterEvidence>,
}

/// Bounded, deterministic evidence for a recognized row-filter request.
///
/// Token spans are half-open `(start, end)` pairs of original prompt token
/// indices and never overlap across categories. Normalized prompt tokens with
/// original indices and the action span are additionally recorded in the
/// enclosing [`RecognitionEvidence`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowFilterEvidence {
    /// The action alias and token span.
    pub action: Option<ActionEvidence>,
    /// Header phrases with spans and their bound column identities.
    pub headers: Vec<HeaderEvidence>,
    /// Comparison operator tokens with spans.
    pub operators: Vec<OperatorEvidence>,
    /// Boolean connector and negation tokens with spans (`"and"`, `"but"`,
    /// `"or"`, or `"not"`).
    pub connectors: Vec<ConnectorEvidence>,
    /// Literal tokens with spans, typed values, and parser policies.
    pub literals: Vec<LiteralEvidence>,
    /// Parenthesis tokens with spans.
    pub parentheses: Vec<ParenthesisEvidence>,
    /// The single unambiguous compiled predicate tree.
    pub predicate: Option<Expression>,
    /// Plan schema version emitted for this recognition.
    pub plan_schema_version: u32,
    /// Text-match policy for text `=`/`!=` implied by `plan_schema_version`
    /// (Epic 008 locked decisions 1–3). Missing in version 2 artifacts, which
    /// deserialize as [`TextMatchPolicy::Exact`].
    #[serde(default)]
    pub text_match: TextMatchPolicy,
}

impl RowFilterEvidence {
    /// The plan schema version the row-filter path emits.
    pub const PLAN_SCHEMA_VERSION: u32 = PLAN_SCHEMA_VERSION_3;

    /// The text-match policy the row-filter path records. Matches
    /// [`crate::plan::Plan::text_match_policy()`] for
    /// [`Self::PLAN_SCHEMA_VERSION`].
    pub const TEXT_MATCH: TextMatchPolicy = TextMatchPolicy::UnicodeLowercase;
}

/// Evidence of a header phrase bound to one column during row-filter
/// recognition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeaderEvidence {
    /// The contiguous normalized prompt tokens forming the header phrase.
    pub tokens: Vec<String>,
    /// Token span (start, end) — end is exclusive.
    pub span: (usize, usize),
    /// Stable column ID of the bound column.
    pub column_id: String,
    /// Display name of the bound column.
    pub display_name: String,
}

/// Evidence of a comparison operator token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperatorEvidence {
    /// The comparison operator that was matched.
    pub operator: ComparisonOperator,
    /// Token span (start, end) — end is exclusive.
    pub span: (usize, usize),
}

/// Evidence of a Boolean connector or unary negation token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorEvidence {
    /// The surface alias that was matched (`"and"`, `"but"`, `"or"`, `"not"`).
    pub alias: String,
    /// Token span (start, end) — end is exclusive.
    pub span: (usize, usize),
}

/// Evidence of a literal token with its typed interpretation.
///
/// The raw prompt text is retained alongside the typed value; the [`Literal`]
/// variant tag records the literal kind (`"text"` or `"decimal"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiteralEvidence {
    /// Raw prompt text of the literal exactly as recognized.
    pub raw_text: String,
    /// Typed literal value.
    pub literal: Literal,
    /// Parser policy applied to a decimal literal; `None` for text literals.
    pub parser_policy: Option<NumericParsePolicy>,
    /// Token span (start, end) — end is exclusive.
    pub span: (usize, usize),
}

/// Evidence of a parenthesis token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParenthesisEvidence {
    /// The surface token that was matched (`"("` or `")"`).
    pub text: String,
    /// Token span (start, end) — end is exclusive.
    pub span: (usize, usize),
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
    /// Token span (start, end) of the competing parse's column phrase — end
    /// is exclusive.
    pub column_span: (usize, usize),
    /// The surface modifier alias this parse consumed (e.g. "unique"), or
    /// None when the parse reads those words as part of the column phrase.
    pub modifier: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ComparisonOperator, Expression, Literal};
    use baho_model::ExactDecimal;

    fn dec(text: &str) -> ExactDecimal {
        ExactDecimal::parse(text).unwrap()
    }

    fn sample_evidence() -> RecognitionEvidence {
        RecognitionEvidence {
            schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
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
            row_filter: None,
        }
    }

    /// Evidence for `List rows where Job = unemployed or Annual Income < 10000`.
    fn sample_row_filter_evidence() -> RecognitionEvidence {
        RecognitionEvidence {
            schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
            prompt_tokens: vec![
                PromptToken {
                    index: 0,
                    text: "list".into(),
                },
                PromptToken {
                    index: 1,
                    text: "rows".into(),
                },
                PromptToken {
                    index: 2,
                    text: "where".into(),
                },
                PromptToken {
                    index: 3,
                    text: "job".into(),
                },
                PromptToken {
                    index: 4,
                    text: "=".into(),
                },
                PromptToken {
                    index: 5,
                    text: "unemployed".into(),
                },
                PromptToken {
                    index: 6,
                    text: "or".into(),
                },
                PromptToken {
                    index: 7,
                    text: "annual".into(),
                },
                PromptToken {
                    index: 8,
                    text: "income".into(),
                },
                PromptToken {
                    index: 9,
                    text: "<".into(),
                },
                PromptToken {
                    index: 10,
                    text: "10000".into(),
                },
            ],
            action: Some(ActionEvidence {
                alias: "list".into(),
                span: (0, 1),
            }),
            modifier: None,
            column_phrase: None,
            matched_column: None,
            match_class: None,
            canonical_operation: Some("row_filter".into()),
            refusal_reason: None,
            competing_parses: Vec::new(),
            row_filter: Some(RowFilterEvidence {
                action: Some(ActionEvidence {
                    alias: "list".into(),
                    span: (0, 1),
                }),
                headers: vec![
                    HeaderEvidence {
                        tokens: vec!["job".into()],
                        span: (3, 4),
                        column_id: "column-0".into(),
                        display_name: "Job".into(),
                    },
                    HeaderEvidence {
                        tokens: vec!["annual".into(), "income".into()],
                        span: (7, 9),
                        column_id: "column-1".into(),
                        display_name: "Annual Income".into(),
                    },
                ],
                operators: vec![
                    OperatorEvidence {
                        operator: ComparisonOperator::Equal,
                        span: (4, 5),
                    },
                    OperatorEvidence {
                        operator: ComparisonOperator::Less,
                        span: (9, 10),
                    },
                ],
                connectors: vec![ConnectorEvidence {
                    alias: "or".into(),
                    span: (6, 7),
                }],
                literals: vec![
                    LiteralEvidence {
                        raw_text: "unemployed".into(),
                        literal: Literal::Text("unemployed".into()),
                        parser_policy: None,
                        span: (5, 6),
                    },
                    LiteralEvidence {
                        raw_text: "10000".into(),
                        literal: Literal::Decimal(dec("10000")),
                        parser_policy: Some(NumericParsePolicy::StrictDecimal),
                        span: (10, 11),
                    },
                ],
                parentheses: Vec::new(),
                predicate: Some(Expression::Or {
                    predicates: vec![
                        Expression::Compare {
                            column: "column-0".into(),
                            operator: ComparisonOperator::Equal,
                            literal: Literal::Text("unemployed".into()),
                        },
                        Expression::Compare {
                            column: "column-1".into(),
                            operator: ComparisonOperator::Less,
                            literal: Literal::Decimal(dec("10000")),
                        },
                    ],
                }),
                plan_schema_version: RowFilterEvidence::PLAN_SCHEMA_VERSION,
                text_match: RowFilterEvidence::TEXT_MATCH,
            }),
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
        assert!(evidence.row_filter.is_none());
    }

    #[test]
    fn recognition_evidence_with_refusal() {
        let evidence = RecognitionEvidence {
            schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
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
            row_filter: None,
        };
        assert!(evidence.matched_column.is_none());
        assert!(evidence.refusal_reason.is_some());
    }

    #[test]
    fn recognition_evidence_with_competing_parses() {
        let evidence = RecognitionEvidence {
            schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
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
                    column_span: (1, 2),
                    modifier: None,
                },
                CompetingParseEvidence {
                    column_display_name: "floor".into(),
                    score: 1.0,
                    column_span: (1, 2),
                    modifier: None,
                },
            ],
            row_filter: None,
        };
        assert_eq!(evidence.competing_parses.len(), 2);
    }

    #[test]
    fn serde_round_trip() {
        for evidence in [sample_evidence(), sample_row_filter_evidence()] {
            let json = serde_json::to_string(&evidence).unwrap();
            let back: RecognitionEvidence = serde_json::from_str(&json).unwrap();
            assert_eq!(evidence, back);
        }
    }

    #[test]
    fn row_filter_evidence_records_text_match_policy() {
        let evidence = sample_row_filter_evidence();
        let row_filter = evidence.row_filter.as_ref().unwrap();
        assert_eq!(row_filter.text_match, TextMatchPolicy::UnicodeLowercase);
        assert_eq!(
            RowFilterEvidence::TEXT_MATCH,
            TextMatchPolicy::UnicodeLowercase
        );
        let json = serde_json::to_string(&evidence).unwrap();
        assert!(json.contains(r#""text_match":"unicode_lowercase""#));
        let back: RecognitionEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.row_filter.unwrap().text_match,
            TextMatchPolicy::UnicodeLowercase
        );
    }

    #[test]
    fn version_2_row_filter_evidence_deserializes_with_exact_text_match() {
        // Compatibility coverage for locked decision 14: a version 2 artifact
        // predates `text_match` and must keep its historical exact-match
        // semantics. It is never rewritten.
        let json = serde_json::json!({
            "schema_version": 2,
            "prompt_tokens": [{ "index": 0, "text": "list" }],
            "action": null,
            "modifier": null,
            "column_phrase": null,
            "matched_column": null,
            "match_class": null,
            "canonical_operation": "row_filter",
            "refusal_reason": null,
            "competing_parses": [],
            "row_filter": {
                "action": null,
                "headers": [],
                "operators": [],
                "connectors": [],
                "literals": [
                    { "raw_text": "10000", "literal": {"decimal": "10000"}, "parser_policy": "strict_decimal", "span": [5, 6] }
                ],
                "parentheses": [],
                "predicate": null,
                "plan_schema_version": 2
            }
        });
        let back: RecognitionEvidence = serde_json::from_value(json).unwrap();
        assert_eq!(back.schema_version, 2);
        let row_filter = back.row_filter.unwrap();
        assert_eq!(row_filter.text_match, TextMatchPolicy::Exact);
        assert_eq!(row_filter.plan_schema_version, 2);
        assert_eq!(
            row_filter.literals[0].literal,
            Literal::Decimal(dec("10000"))
        );
    }

    #[test]
    fn retrieval_evidence_json_keeps_existing_fields() {
        let json = serde_json::to_value(sample_evidence()).unwrap();
        assert_eq!(json["schema_version"], 3);
        assert!(json["row_filter"].is_null());
        assert_eq!(json["canonical_operation"], "distinct");
        assert_eq!(json["action"]["alias"], "extract");
        assert_eq!(json["action"]["span"], serde_json::json!([0, 1]));
        assert_eq!(json["matched_column"]["display_name"], "Floor Plan");
    }

    #[test]
    fn row_filter_evidence_json_structure() {
        let json = serde_json::to_value(sample_row_filter_evidence()).unwrap();
        assert_eq!(json["schema_version"], 3);
        let row_filter = &json["row_filter"];
        assert_eq!(row_filter["plan_schema_version"], 3);
        assert_eq!(row_filter["text_match"], "unicode_lowercase");
        assert_eq!(row_filter["action"]["alias"], "list");

        let headers = row_filter["headers"].as_array().unwrap();
        assert_eq!(headers[0]["tokens"], serde_json::json!(["job"]));
        assert_eq!(headers[0]["span"], serde_json::json!([3, 4]));
        assert_eq!(headers[0]["column_id"], "column-0");
        assert_eq!(headers[0]["display_name"], "Job");
        assert_eq!(headers[1]["span"], serde_json::json!([7, 9]));
        assert_eq!(headers[1]["display_name"], "Annual Income");

        let operators = row_filter["operators"].as_array().unwrap();
        assert_eq!(operators[0]["operator"], "=");
        assert_eq!(operators[0]["span"], serde_json::json!([4, 5]));
        assert_eq!(operators[1]["operator"], "<");

        let connectors = row_filter["connectors"].as_array().unwrap();
        assert_eq!(connectors[0]["alias"], "or");
        assert_eq!(connectors[0]["span"], serde_json::json!([6, 7]));

        let literals = row_filter["literals"].as_array().unwrap();
        assert_eq!(literals[0]["raw_text"], "unemployed");
        assert_eq!(
            literals[0]["literal"],
            serde_json::json!({ "text": "unemployed" })
        );
        assert!(literals[0]["parser_policy"].is_null());
        assert_eq!(literals[1]["raw_text"], "10000");
        assert_eq!(
            literals[1]["literal"],
            serde_json::json!({ "decimal": "10000" })
        );
        assert_eq!(literals[1]["parser_policy"], "strict_decimal");

        assert!(row_filter["parentheses"].as_array().unwrap().is_empty());

        let predicate = &row_filter["predicate"];
        assert_eq!(predicate["op"], "or");
        assert_eq!(predicate["predicates"][0]["op"], "compare");
        assert_eq!(predicate["predicates"][0]["column"], "column-0");
    }

    #[test]
    fn row_filter_evidence_records_parenthesis_and_connector_spans() {
        let mut evidence = sample_row_filter_evidence();
        let row_filter = evidence.row_filter.as_mut().unwrap();
        row_filter.parentheses = vec![
            ParenthesisEvidence {
                text: "(".into(),
                span: (3, 4),
            },
            ParenthesisEvidence {
                text: ")".into(),
                span: (6, 7),
            },
        ];
        row_filter.connectors.push(ConnectorEvidence {
            alias: "not".into(),
            span: (4, 5),
        });
        let json = serde_json::to_string(&evidence).unwrap();
        let back: RecognitionEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(evidence, back);
        let row_filter = back.row_filter.unwrap();
        assert_eq!(row_filter.parentheses[0].text, "(");
        assert_eq!(row_filter.parentheses[0].span, (3, 4));
        assert_eq!(row_filter.connectors[1].alias, "not");
    }
}
