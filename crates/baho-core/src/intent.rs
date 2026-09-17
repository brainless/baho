use baho_model::column::ColumnDefinition;
use baho_plan::evidence::{
    ActionEvidence, ColumnPhraseEvidence, CompetingParseEvidence, MatchedColumn, ModifierEvidence,
    PromptToken, RecognitionEvidence,
};
use baho_plan::plan::{DistinctKeep, Expression, Plan, PlanSource, PlanStep};
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::IntentError;

const FILLER_TOKENS: &[&str] = &["all", "the", "a", "an", "of", "from", "values", "value"];

const MATCH_THRESHOLD: f64 = 0.5;
const AMBIGUITY_MARGIN: f64 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanonicalAction {
    Retrieve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanonicalOperation {
    Select,
    Distinct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchClass {
    Exact,
    TerminalSVariant,
}

impl MatchClass {
    fn score(self) -> f64 {
        match self {
            MatchClass::Exact => 1.0,
            MatchClass::TerminalSVariant => 0.9,
        }
    }

    fn label(self) -> &'static str {
        match self {
            MatchClass::Exact => "exact",
            MatchClass::TerminalSVariant => "terminal_s_variant",
        }
    }
}

#[derive(Debug, Clone)]
struct IndexedToken {
    #[allow(dead_code)]
    index: usize,
    text: String,
    #[allow(dead_code)]
    start: usize,
}

#[derive(Debug, Clone)]
struct CandidateParse {
    #[allow(dead_code)]
    action_span: (usize, usize),
    modifier: Option<CanonicalOperation>,
    #[allow(dead_code)]
    modifier_span: Option<(usize, usize)>,
    column_span: (usize, usize),
    column: ColumnDefinition,
    match_class: MatchClass,
    score: f64,
}

fn resolve_action(token: &str) -> Option<CanonicalAction> {
    match token {
        "extract" | "list" | "show" | "get" | "find" | "display" | "return" => {
            Some(CanonicalAction::Retrieve)
        }
        _ => None,
    }
}

fn resolve_modifier(token: &str) -> Option<CanonicalOperation> {
    match token {
        "unique" | "distinct" | "deduplicate" | "deduplicated" => {
            Some(CanonicalOperation::Distinct)
        }
        _ => None,
    }
}

fn tokens_match(a: &str, b: &str) -> Option<MatchClass> {
    if a == b {
        return Some(MatchClass::Exact);
    }
    if a.len() == b.len() + 1 && a.ends_with('s') && &a[..a.len() - 1] == b {
        return Some(MatchClass::TerminalSVariant);
    }
    if b.len() == a.len() + 1 && b.ends_with('s') && &b[..b.len() - 1] == a {
        return Some(MatchClass::TerminalSVariant);
    }
    None
}

fn match_column_span(
    prompt_tokens: &[IndexedToken],
    span_start: usize,
    span_end: usize,
    col: &ColumnDefinition,
) -> Option<MatchClass> {
    let normalized = normalize_for_match(&col.display_name);
    let col_tokens: Vec<&str> = normalized.split_whitespace().collect();
    if col_tokens.is_empty() {
        return None;
    }

    let span_len = span_end - span_start;
    if span_len != col_tokens.len() {
        return None;
    }

    let mut worst_class = MatchClass::Exact;
    for (i, col_token) in col_tokens.iter().enumerate() {
        match tokens_match(&prompt_tokens[span_start + i].text, col_token) {
            Some(cls) => {
                if cls == MatchClass::TerminalSVariant {
                    worst_class = MatchClass::TerminalSVariant;
                }
            }
            None => return None,
        }
    }
    Some(worst_class)
}

fn build_candidate_parses(
    tokens: &[IndexedToken],
    columns: &[ColumnDefinition],
) -> Vec<CandidateParse> {
    let mut candidates = Vec::new();

    if tokens.is_empty() {
        return candidates;
    }

    if resolve_action(&tokens[0].text).is_none() {
        return candidates;
    }

    let action_end = 1;

    let mut modifier_positions: Vec<Option<(usize, CanonicalOperation)>> = vec![None];
    let mut pos = action_end;
    while pos < tokens.len() && FILLER_TOKENS.contains(&tokens[pos].text.as_str()) {
        pos += 1;
    }
    if pos < tokens.len() {
        if let Some(op) = resolve_modifier(&tokens[pos].text) {
            modifier_positions.push(Some((pos, op)));
        }
    }

    for modifier_pos in &modifier_positions {
        let after_modifier = if let Some((idx, _)) = modifier_pos {
            let mut p = idx + 1;
            while p < tokens.len() && FILLER_TOKENS.contains(&tokens[p].text.as_str()) {
                p += 1;
            }
            p
        } else {
            pos
        };

        if after_modifier >= tokens.len() {
            continue;
        }

        let span_start = after_modifier;
        let span_end = tokens.len();

        for col in columns {
            if let Some(match_class) = match_column_span(tokens, span_start, span_end, col) {
                let score = match_class.score();
                if score >= MATCH_THRESHOLD {
                    candidates.push(CandidateParse {
                        action_span: (0, action_end),
                        modifier: modifier_pos.map(|(_, op)| op),
                        modifier_span: modifier_pos.map(|(idx, _)| (idx, idx + 1)),
                        column_span: (span_start, span_end),
                        column: col.clone(),
                        match_class,
                        score,
                    });
                }
            }
        }
    }

    candidates
}

/// When several tied candidates share one column span, that single prompt
/// phrase resolves equally to multiple columns and is a column-level
/// ambiguity; otherwise the tie is between distinct parses.
fn column_ambiguous_candidates(tied: &[&CandidateParse]) -> Option<Vec<String>> {
    let ambiguous_span = ambiguous_span(tied)?;

    // tied is already sorted by score desc then column id, so filtering
    // preserves deterministic ordering of the matched columns.
    let mut candidates: Vec<String> = tied
        .iter()
        .filter(|c| c.column_span == ambiguous_span)
        .map(|c| c.column.display_name.clone())
        .collect();
    candidates.dedup();
    Some(candidates)
}

/// The single column span shared by a tying group that resolves to more than
/// one distinct column, if any.
fn ambiguous_span(tied: &[&CandidateParse]) -> Option<(usize, usize)> {
    let mut by_span: HashMap<(usize, usize), Vec<&&CandidateParse>> = HashMap::new();
    for c in tied {
        by_span.entry(c.column_span).or_default().push(c);
    }

    by_span.iter().find_map(|(span, group)| {
        let mut ids: Vec<&str> = group.iter().map(|c| c.column.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() > 1 { Some(*span) } else { None }
    })
}

/// Deterministically ordered bounded evidence for a set of tied candidates.
fn competing_parse_evidence(tied: &[&CandidateParse]) -> Vec<CompetingParseEvidence> {
    let mut seen: Vec<(String, f64)> = Vec::new();
    for c in tied {
        let entry = (c.column.display_name.clone(), c.score);
        if !seen.contains(&entry) {
            seen.push(entry);
        }
    }
    seen.into_iter()
        .map(|(column_display_name, score)| CompetingParseEvidence {
            column_display_name,
            score,
        })
        .collect()
}

fn normalize_for_match(s: &str) -> String {
    let trimmed = s.trim().to_lowercase();
    let mut result = String::with_capacity(trimmed.len());
    let mut prev_was_space = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                result.push(' ');
            }
            prev_was_space = true;
        } else {
            result.push(ch);
            prev_was_space = false;
        }
    }
    result
}

/// A recognized user intent.
#[derive(Debug, Clone)]
pub struct RecognizedIntent {
    pub action: CanonicalAction,
    pub operation: CanonicalOperation,
    pub column_id: String,
    pub column_display_name: String,
    pub evidence: RecognitionEvidence,
}

pub fn recognize_intent(
    prompt: &str,
    columns: &[ColumnDefinition],
) -> Result<RecognizedIntent, IntentError> {
    let raw_tokens: Vec<String> = prompt
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .collect();

    if raw_tokens.is_empty() {
        return Err(IntentError::Unsupported(
            "empty prompt".to_string(),
            Some(RecognitionEvidence {
                prompt_tokens: Vec::new(),
                action: None,
                modifier: None,
                column_phrase: None,
                matched_column: None,
                match_class: None,
                canonical_operation: None,
                refusal_reason: Some("intent.unsupported".to_string()),
                competing_parses: Vec::new(),
            }),
        ));
    }

    let mut offset = 0;
    let indexed_tokens: Vec<IndexedToken> = raw_tokens
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let start = offset;
            offset += text.len() + 1;
            IndexedToken {
                index: i,
                text: text.clone(),
                start,
            }
        })
        .collect();

    let prompt_token_evidence: Vec<PromptToken> = indexed_tokens
        .iter()
        .map(|t| PromptToken {
            index: t.index,
            text: t.text.clone(),
        })
        .collect();

    if resolve_action(&indexed_tokens[0].text).is_none() {
        return Err(IntentError::Unsupported(
            format!("expected action verb, got '{}'", indexed_tokens[0].text),
            Some(RecognitionEvidence {
                prompt_tokens: prompt_token_evidence,
                action: None,
                modifier: None,
                column_phrase: None,
                matched_column: None,
                match_class: None,
                canonical_operation: None,
                refusal_reason: Some("intent.unsupported".to_string()),
                competing_parses: Vec::new(),
            }),
        ));
    }

    let action_evidence = Some(ActionEvidence {
        alias: indexed_tokens[0].text.clone(),
        span: (0, 1),
    });

    let candidates = build_candidate_parses(&indexed_tokens, columns);

    if candidates.is_empty() {
        let col_start = {
            let mut p = 1;
            while p < indexed_tokens.len()
                && FILLER_TOKENS.contains(&indexed_tokens[p].text.as_str())
            {
                p += 1;
            }
            if p < indexed_tokens.len() {
                if resolve_modifier(&indexed_tokens[p].text).is_some() {
                    p += 1;
                    while p < indexed_tokens.len()
                        && FILLER_TOKENS.contains(&indexed_tokens[p].text.as_str())
                    {
                        p += 1;
                    }
                }
            }
            p
        };
        let term = indexed_tokens[col_start..]
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        return Err(IntentError::ColumnNotFound {
            prompt_term: term,
            evidence: Some(RecognitionEvidence {
                prompt_tokens: prompt_token_evidence,
                action: Some(ActionEvidence {
                    alias: indexed_tokens[0].text.clone(),
                    span: (0, 1),
                }),
                modifier: None,
                column_phrase: None,
                matched_column: None,
                match_class: None,
                canonical_operation: None,
                refusal_reason: Some("intent.column_not_found".to_string()),
                competing_parses: Vec::new(),
            }),
        });
    }

    let mut sorted = candidates;
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.column.id.cmp(&b.column.id))
    });

    let top_score = sorted[0].score;
    let tied: Vec<&CandidateParse> = sorted
        .iter()
        .filter(|c| (top_score - c.score).abs() < AMBIGUITY_MARGIN)
        .collect();

    if tied.len() > 1 {
        let competing_parses = competing_parse_evidence(&tied);
        if let Some(candidates) = column_ambiguous_candidates(&tied) {
            return Err(IntentError::ColumnAmbiguous {
                candidates,
                evidence: Some(RecognitionEvidence {
                    prompt_tokens: prompt_token_evidence,
                    action: Some(ActionEvidence {
                        alias: indexed_tokens[0].text.clone(),
                        span: (0, 1),
                    }),
                    modifier: None,
                    column_phrase: None,
                    matched_column: None,
                    match_class: None,
                    canonical_operation: None,
                    refusal_reason: Some("intent.column_ambiguous".to_string()),
                    competing_parses,
                }),
            });
        }
        let candidate_descs: Vec<String> = tied
            .iter()
            .map(|c| {
                format!(
                    "{} (score={}, match={})",
                    c.column.display_name,
                    c.score,
                    c.match_class.label()
                )
            })
            .collect();
        return Err(IntentError::ParseAmbiguous {
            candidates: candidate_descs,
            evidence: Some(RecognitionEvidence {
                prompt_tokens: prompt_token_evidence,
                action: Some(ActionEvidence {
                    alias: indexed_tokens[0].text.clone(),
                    span: (0, 1),
                }),
                modifier: None,
                column_phrase: None,
                matched_column: None,
                match_class: None,
                canonical_operation: None,
                refusal_reason: Some("intent.parse_ambiguous".to_string()),
                competing_parses,
            }),
        });
    }

    let best = &sorted[0];
    let action = CanonicalAction::Retrieve;
    let operation = best.modifier.unwrap_or(CanonicalOperation::Select);

    let modifier_evidence = best.modifier.map(|op| {
        let alias = match op {
            CanonicalOperation::Distinct => best
                .modifier_span
                .map(|(s, _e)| indexed_tokens[s].text.clone())
                .unwrap_or_else(|| "distinct".to_string()),
            CanonicalOperation::Select => "select".to_string(),
        };
        ModifierEvidence {
            alias,
            span: best.modifier_span.unwrap_or((0, 0)),
        }
    });

    let col_tokens: Vec<String> = indexed_tokens[best.column_span.0..best.column_span.1]
        .iter()
        .map(|t| t.text.clone())
        .collect();
    let column_phrase_evidence = Some(ColumnPhraseEvidence {
        tokens: col_tokens,
        span: best.column_span,
    });

    let match_class_label = best.match_class.label().to_string();
    let evidence_str = match best.match_class {
        MatchClass::Exact => "exact contiguous phrase".to_string(),
        MatchClass::TerminalSVariant => "terminal-s variant".to_string(),
    };

    let canonical_op_str = match operation {
        CanonicalOperation::Select => "select".to_string(),
        CanonicalOperation::Distinct => "distinct".to_string(),
    };

    let evidence = RecognitionEvidence {
        prompt_tokens: prompt_token_evidence,
        action: action_evidence,
        modifier: modifier_evidence,
        column_phrase: column_phrase_evidence,
        matched_column: Some(MatchedColumn {
            column_id: best.column.id.clone(),
            display_name: best.column.display_name.clone(),
            score: best.score,
            evidence: evidence_str,
        }),
        match_class: Some(match_class_label),
        canonical_operation: Some(canonical_op_str),
        refusal_reason: None,
        competing_parses: Vec::new(),
    };

    Ok(RecognizedIntent {
        action,
        operation,
        column_id: best.column.id.clone(),
        column_display_name: best.column.display_name.clone(),
        evidence,
    })
}

pub fn compile_intent_to_plan(
    intent: &RecognizedIntent,
    source_revision: &str,
    table_id: &str,
) -> Plan {
    let source = PlanSource {
        revision: source_revision.to_string(),
        table_id: table_id.to_string(),
    };
    match intent.operation {
        CanonicalOperation::Select => Plan {
            schema_version: 1,
            source,
            steps: vec![PlanStep::Select {
                columns: vec![intent.column_id.clone()],
            }],
        },
        CanonicalOperation::Distinct => Plan {
            schema_version: 1,
            source,
            steps: vec![
                PlanStep::Filter {
                    predicate: Expression::IsNotBlank {
                        column: intent.column_id.clone(),
                    },
                },
                PlanStep::Select {
                    columns: vec![intent.column_id.clone()],
                },
                PlanStep::Distinct {
                    columns: vec![intent.column_id.clone()],
                    keep: DistinctKeep::First,
                },
            ],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns() -> Vec<ColumnDefinition> {
        vec![
            ColumnDefinition {
                id: "column-0".to_string(),
                ordinal: 0,
                source_header_raw: Some("ID".to_string()),
                source_header_normalized: Some("id".to_string()),
                display_name: "ID".to_string(),
            },
            ColumnDefinition {
                id: "column-1".to_string(),
                ordinal: 1,
                source_header_raw: Some("Floor Plan".to_string()),
                source_header_normalized: Some("floor plan".to_string()),
                display_name: "Floor Plan".to_string(),
            },
            ColumnDefinition {
                id: "column-2".to_string(),
                ordinal: 2,
                source_header_raw: Some("Color".to_string()),
                source_header_normalized: Some("color".to_string()),
                display_name: "Color".to_string(),
            },
        ]
    }

    fn income_time_columns() -> Vec<ColumnDefinition> {
        vec![
            ColumnDefinition {
                id: "col-income".to_string(),
                ordinal: 0,
                source_header_raw: Some("Income".to_string()),
                source_header_normalized: Some("income".to_string()),
                display_name: "Income".to_string(),
            },
            ColumnDefinition {
                id: "col-time".to_string(),
                ordinal: 1,
                source_header_raw: Some("Time".to_string()),
                source_header_normalized: Some("time".to_string()),
                display_name: "Time".to_string(),
            },
            ColumnDefinition {
                id: "col-show-time".to_string(),
                ordinal: 2,
                source_header_raw: Some("Show Time".to_string()),
                source_header_normalized: Some("show time".to_string()),
                display_name: "Show Time".to_string(),
            },
        ]
    }

    #[test]
    fn extract_unique_floor_plans() {
        let cols = columns();
        let intent = recognize_intent("Extract all the unique floor plans", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Distinct);
        assert_eq!(intent.column_id, "column-1");
        assert_eq!(intent.column_display_name, "Floor Plan");
        assert!(intent.evidence.matched_column.is_some());
        assert_eq!(
            intent.evidence.matched_column.as_ref().unwrap().column_id,
            "column-1"
        );
    }

    #[test]
    fn list_distinct_floor_plans() {
        let cols = columns();
        let intent = recognize_intent("list distinct floor plans", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Distinct);
        assert_eq!(intent.column_id, "column-1");
    }

    #[test]
    fn show_unique_floor_plan_singular() {
        let cols = columns();
        let intent = recognize_intent("show unique floor plan", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Distinct);
        assert_eq!(intent.column_id, "column-1");
    }

    #[test]
    fn get_the_unique_values_from_floor_plan() {
        let cols = columns();
        let intent = recognize_intent("get the unique values from floor plan", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Distinct);
        assert_eq!(intent.column_id, "column-1");
    }

    #[test]
    fn select_only_retrieval() {
        let cols = columns();
        let intent = recognize_intent("show floor plans", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "column-1");
    }

    #[test]
    fn column_not_found() {
        let cols = columns();
        let err = recognize_intent("find all distinct sizes", &cols).unwrap_err();
        assert!(matches!(err, IntentError::ColumnNotFound { .. }));
    }

    #[test]
    fn unsupported_intent() {
        let cols = columns();
        let err = recognize_intent("do something random", &cols).unwrap_err();
        assert!(matches!(err, IntentError::Unsupported(_, _)));
    }

    #[test]
    fn partial_column_match_refused() {
        let cols = vec![
            ColumnDefinition {
                id: "column-0".to_string(),
                ordinal: 0,
                source_header_raw: Some("Floor Plan Type".to_string()),
                source_header_normalized: Some("floor plan type".to_string()),
                display_name: "Floor Plan Type".to_string(),
            },
            ColumnDefinition {
                id: "column-1".to_string(),
                ordinal: 1,
                source_header_raw: Some("Floor Plan Name".to_string()),
                source_header_normalized: Some("floor plan name".to_string()),
                display_name: "Floor Plan Name".to_string(),
            },
        ];
        let err = recognize_intent("extract unique floor plan", &cols).unwrap_err();
        assert!(matches!(err, IntentError::ColumnNotFound { .. }));
    }

    #[test]
    fn evidence_has_correct_tokens_and_scores() {
        let cols = columns();
        let intent = recognize_intent("Extract all the unique floor plans", &cols).unwrap();
        let ev = &intent.evidence;
        let token_texts: Vec<&str> = ev.prompt_tokens.iter().map(|t| t.text.as_str()).collect();
        assert!(token_texts.contains(&"extract"));
        assert!(token_texts.contains(&"unique"));
        assert!(token_texts.contains(&"floor"));
        assert!(token_texts.contains(&"plans"));
        let mc = ev.matched_column.as_ref().unwrap();
        assert!(mc.score >= 0.5);
        assert!(!mc.evidence.is_empty());
        assert!(ev.action.is_some());
        assert!(ev.match_class.is_some());
        assert!(ev.canonical_operation.is_some());
    }

    #[test]
    fn list_income_select_only() {
        let cols = income_time_columns();
        let intent = recognize_intent("List income", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "col-income");
        assert_eq!(intent.column_display_name, "Income");
        let mc = intent.evidence.matched_column.as_ref().unwrap();
        assert!((mc.score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn list_incomes_terminal_s() {
        let cols = vec![ColumnDefinition {
            id: "col-income".to_string(),
            ordinal: 0,
            source_header_raw: Some("Income".to_string()),
            source_header_normalized: Some("income".to_string()),
            display_name: "Income".to_string(),
        }];
        let intent = recognize_intent("List incomes", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "col-income");
        let mc = intent.evidence.matched_column.as_ref().unwrap();
        assert!((mc.score - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn show_time_exact_column() {
        let cols = income_time_columns();
        let intent = recognize_intent("Show time", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "col-time");
        assert_eq!(intent.column_display_name, "Time");
    }

    #[test]
    fn list_show_time_full_column() {
        let cols = income_time_columns();
        let intent = recognize_intent("List Show Time", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "col-show-time");
        assert_eq!(intent.column_display_name, "Show Time");
    }

    #[test]
    fn show_show_time() {
        let cols = income_time_columns();
        let intent = recognize_intent("Show Show Time", &cols).unwrap();
        assert_eq!(intent.action, CanonicalAction::Retrieve);
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "col-show-time");
        assert_eq!(intent.column_display_name, "Show Time");
    }

    #[test]
    fn show_time_no_partial_match_on_show_time_only() {
        let cols = vec![ColumnDefinition {
            id: "col-show-time".to_string(),
            ordinal: 0,
            source_header_raw: Some("Show Time".to_string()),
            source_header_normalized: Some("show time".to_string()),
            display_name: "Show Time".to_string(),
        }];
        let err = recognize_intent("Show time", &cols).unwrap_err();
        assert!(matches!(err, IntentError::ColumnNotFound { .. }));
    }

    #[test]
    fn no_token_role_overlap() {
        let cols = income_time_columns();
        let intent = recognize_intent("Show time", &cols).unwrap();
        let mc = intent.evidence.matched_column.as_ref().unwrap();
        assert_eq!(mc.evidence, "exact contiguous phrase");
        let col_phrase = intent.evidence.column_phrase.as_ref().unwrap();
        assert_eq!(col_phrase.tokens, vec!["time"]);
        assert_eq!(intent.action, CanonicalAction::Retrieve);
    }

    #[test]
    fn words_ending_in_s_not_corrupted() {
        let cols = vec![ColumnDefinition {
            id: "col-status".to_string(),
            ordinal: 0,
            source_header_raw: Some("Status".to_string()),
            source_header_normalized: Some("status".to_string()),
            display_name: "Status".to_string(),
        }];
        let intent = recognize_intent("list status", &cols).unwrap();
        assert_eq!(intent.column_id, "col-status");
        let mc = intent.evidence.matched_column.as_ref().unwrap();
        assert!((mc.score - 1.0).abs() < f64::EPSILON);
        assert_eq!(mc.evidence, "exact contiguous phrase");

        let cols2 = vec![ColumnDefinition {
            id: "col-address".to_string(),
            ordinal: 0,
            source_header_raw: Some("Address".to_string()),
            source_header_normalized: Some("address".to_string()),
            display_name: "Address".to_string(),
        }];
        let err = recognize_intent("list addresses", &cols2).unwrap_err();
        assert!(matches!(err, IntentError::ColumnNotFound { .. }));
    }

    #[test]
    fn single_span_matching_duplicate_headers_is_column_ambiguous() {
        let cols = vec![
            ColumnDefinition {
                id: "col-floor".to_string(),
                ordinal: 0,
                source_header_raw: Some("Floor".to_string()),
                source_header_normalized: Some("floor".to_string()),
                display_name: "Floor".to_string(),
            },
            ColumnDefinition {
                id: "col-floor-2".to_string(),
                ordinal: 1,
                source_header_raw: Some("floor".to_string()),
                source_header_normalized: Some("floor".to_string()),
                display_name: "floor".to_string(),
            },
        ];
        let err = recognize_intent("list floor", &cols).unwrap_err();
        assert!(matches!(
            err,
            IntentError::ColumnAmbiguous { ref candidates, .. }
                if candidates == &vec!["Floor".to_string(), "floor".to_string()]
        ));
    }

    #[test]
    fn single_span_matching_three_duplicate_headers_lists_them_deterministically() {
        let cols = vec![
            ColumnDefinition {
                id: "col-b".to_string(),
                ordinal: 1,
                source_header_raw: Some("status".to_string()),
                source_header_normalized: Some("status".to_string()),
                display_name: "status".to_string(),
            },
            ColumnDefinition {
                id: "col-a".to_string(),
                ordinal: 0,
                source_header_raw: Some("Status".to_string()),
                source_header_normalized: Some("status".to_string()),
                display_name: "Status".to_string(),
            },
            ColumnDefinition {
                id: "col-c".to_string(),
                ordinal: 2,
                source_header_raw: Some("STATUS".to_string()),
                source_header_normalized: Some("status".to_string()),
                display_name: "STATUS".to_string(),
            },
        ];
        let err = recognize_intent("list status", &cols).unwrap_err();
        match err {
            IntentError::ColumnAmbiguous { candidates, .. } => {
                assert_eq!(candidates.len(), 3);
                assert!(candidates.contains(&"Status".to_string()));
                assert!(candidates.contains(&"status".to_string()));
                assert!(candidates.contains(&"STATUS".to_string()));
            }
            other => panic!("expected ColumnAmbiguous, got {other:?}"),
        }
    }

    #[test]
    fn parse_ambiguous_distinct_parses_tied() {
        let cols = vec![
            ColumnDefinition {
                id: "col-unique-income".to_string(),
                ordinal: 0,
                source_header_raw: Some("Unique Income".to_string()),
                source_header_normalized: Some("unique income".to_string()),
                display_name: "Unique Income".to_string(),
            },
            ColumnDefinition {
                id: "col-income".to_string(),
                ordinal: 1,
                source_header_raw: Some("Income".to_string()),
                source_header_normalized: Some("income".to_string()),
                display_name: "Income".to_string(),
            },
        ];
        // "list unique income" yields two distinct 1.0 parses: select of
        // "Unique Income" (span "unique income") and distinct of "Income"
        // (span "income"). These are different parses, not one span mapping to
        // multiple columns, so this is parse ambiguity.
        let err = recognize_intent("list unique income", &cols).unwrap_err();
        assert!(matches!(err, IntentError::ParseAmbiguous { .. }));
    }

    fn make_intent(operation: CanonicalOperation, column_id: &str) -> RecognizedIntent {
        RecognizedIntent {
            action: CanonicalAction::Retrieve,
            operation,
            column_id: column_id.to_string(),
            column_display_name: column_id.to_string(),
            evidence: RecognitionEvidence {
                prompt_tokens: Vec::new(),
                action: None,
                modifier: None,
                column_phrase: None,
                matched_column: None,
                match_class: None,
                canonical_operation: None,
                refusal_reason: None,
                competing_parses: Vec::new(),
            },
        }
    }

    #[test]
    fn select_only_plan_has_one_step() {
        let intent = make_intent(CanonicalOperation::Select, "col-0");
        let plan = compile_intent_to_plan(&intent, "rev-1", "table-0");
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(&plan.steps[0], PlanStep::Select { columns } if columns == &["col-0"]));
    }

    #[test]
    fn distinct_plan_has_three_steps() {
        let intent = make_intent(CanonicalOperation::Distinct, "col-0");
        let plan = compile_intent_to_plan(&intent, "rev-1", "table-0");
        assert_eq!(plan.steps.len(), 3);
        assert!(matches!(&plan.steps[0], PlanStep::Filter { .. }));
        assert!(matches!(&plan.steps[1], PlanStep::Select { .. }));
        assert!(matches!(&plan.steps[2], PlanStep::Distinct { .. }));
    }

    #[test]
    fn plan_source_uses_provided_revision_and_table() {
        let intent = make_intent(CanonicalOperation::Select, "col-0");
        let plan = compile_intent_to_plan(&intent, "rev-abc", "table-9");
        assert_eq!(plan.source.revision, "rev-abc");
        assert_eq!(plan.source.table_id, "table-9");
    }

    #[test]
    fn select_plan_preserves_blanks() {
        let intent = make_intent(CanonicalOperation::Select, "col-0");
        let plan = compile_intent_to_plan(&intent, "rev-1", "table-0");
        assert!(
            !plan
                .steps
                .iter()
                .any(|s| matches!(s, PlanStep::Filter { .. })),
            "select-only plan must not contain a filter step"
        );
    }

    fn as_refusal(
        evidence: &RecognitionEvidence,
    ) -> (&Option<String>, &Vec<CompetingParseEvidence>) {
        (&evidence.refusal_reason, &evidence.competing_parses)
    }

    #[test]
    fn refusal_evidence_present_on_all_error_variants() {
        let cols = income_time_columns();

        let err = recognize_intent("do something", &cols).unwrap_err();
        match err {
            IntentError::Unsupported(_, evidence) => {
                let ev = evidence.expect("unsupported must carry evidence");
                let (reason, competing) = as_refusal(&ev);
                assert_eq!(reason.as_deref(), Some("intent.unsupported"));
                assert!(competing.is_empty());
                assert_eq!(ev.prompt_tokens.len(), 2);
                assert!(ev.action.is_none());
                assert!(ev.matched_column.is_none());
                assert!(ev.canonical_operation.is_none());
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }

        let no_match_cols = vec![ColumnDefinition {
            id: "col-show-time".to_string(),
            ordinal: 0,
            source_header_raw: Some("Show Time".to_string()),
            source_header_normalized: Some("show time".to_string()),
            display_name: "Show Time".to_string(),
        }];
        let err = recognize_intent("Show time", &no_match_cols).unwrap_err();
        match err {
            IntentError::ColumnNotFound { evidence, .. } => {
                let ev = evidence.expect("column_not_found must carry evidence");
                let (reason, competing) = as_refusal(&ev);
                assert_eq!(reason.as_deref(), Some("intent.column_not_found"));
                assert!(competing.is_empty());
                assert!(ev.matched_column.is_none());
                assert!(ev.action.is_some());
            }
            other => panic!("expected ColumnNotFound, got {other:?}"),
        }
    }

    #[test]
    fn column_ambiguous_refusal_has_competing_parses() {
        let cols = vec![
            ColumnDefinition {
                id: "col-floor".to_string(),
                ordinal: 0,
                source_header_raw: Some("Floor".to_string()),
                source_header_normalized: Some("floor".to_string()),
                display_name: "Floor".to_string(),
            },
            ColumnDefinition {
                id: "col-floor-2".to_string(),
                ordinal: 1,
                source_header_raw: Some("floor".to_string()),
                source_header_normalized: Some("floor".to_string()),
                display_name: "floor".to_string(),
            },
        ];
        let err = recognize_intent("list floor", &cols).unwrap_err();
        match err {
            IntentError::ColumnAmbiguous { evidence, .. } => {
                let ev = evidence.expect("column_ambiguous must carry evidence");
                let (reason, competing) = as_refusal(&ev);
                assert_eq!(reason.as_deref(), Some("intent.column_ambiguous"));
                let rows: Vec<(&str, f64)> = competing
                    .iter()
                    .map(|c| (c.column_display_name.as_str(), c.score))
                    .collect();
                assert_eq!(
                    rows,
                    vec![("Floor", 1.0), ("floor", 1.0)],
                    "competing parses must be deterministic and include scores"
                );
                assert!(ev.matched_column.is_none());
                assert!(ev.canonical_operation.is_none());
            }
            other => panic!("expected ColumnAmbiguous, got {other:?}"),
        }
    }

    #[test]
    fn parse_ambiguous_refusal_has_competing_parses() {
        let cols = vec![
            ColumnDefinition {
                id: "col-unique-income".to_string(),
                ordinal: 0,
                source_header_raw: Some("Unique Income".to_string()),
                source_header_normalized: Some("unique income".to_string()),
                display_name: "Unique Income".to_string(),
            },
            ColumnDefinition {
                id: "col-income".to_string(),
                ordinal: 1,
                source_header_raw: Some("Income".to_string()),
                source_header_normalized: Some("income".to_string()),
                display_name: "Income".to_string(),
            },
        ];
        let err = recognize_intent("list unique income", &cols).unwrap_err();
        match err {
            IntentError::ParseAmbiguous { evidence, .. } => {
                let ev = evidence.expect("parse_ambiguous must carry evidence");
                let (reason, competing) = as_refusal(&ev);
                assert_eq!(reason.as_deref(), Some("intent.parse_ambiguous"));
                let rows: Vec<(&str, f64)> = competing
                    .iter()
                    .map(|c| (c.column_display_name.as_str(), c.score))
                    .collect();
                assert_eq!(
                    rows,
                    vec![("Income", 1.0), ("Unique Income", 1.0)],
                    "competing parses must be deterministic and include scores"
                );
                assert!(ev.matched_column.is_none());
                assert!(ev.canonical_operation.is_none());
            }
            other => panic!("expected ParseAmbiguous, got {other:?}"),
        }
    }
}
