use baho_model::column::ColumnDefinition;
use baho_plan::evidence::{MatchedColumn, RecognitionEvidence};

use crate::error::IntentError;

const ACTION_TOKENS: &[&str] = &["extract", "list", "show", "get", "find"];
const OPERATION_TOKENS: &[&str] = &["unique", "distinct"];
const FILLER_TOKENS: &[&str] = &["all", "the", "a", "an", "of", "from", "values", "value"];

const MATCH_THRESHOLD: f64 = 0.5;
const AMBIGUITY_MARGIN: f64 = 0.1;

/// A recognized user intent.
#[derive(Debug, Clone)]
pub struct RecognizedIntent {
    pub operation: String,
    pub column_id: String,
    pub column_display_name: String,
    pub evidence: RecognitionEvidence,
}

/// Recognize a narrow intent from a prompt and available columns.
pub fn recognize_intent(
    prompt: &str,
    columns: &[ColumnDefinition],
) -> Result<RecognizedIntent, IntentError> {
    let tokens: Vec<String> = prompt
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .collect();

    if tokens.is_empty() {
        return Err(IntentError::Unsupported("empty prompt".to_string()));
    }

    let mut pos = 0;

    // First token must be an action verb.
    if pos >= tokens.len() || !ACTION_TOKENS.contains(&tokens[pos].as_str()) {
        return Err(IntentError::Unsupported(format!(
            "expected action verb, got '{}'",
            tokens.get(pos).map(|s| s.as_str()).unwrap_or("")
        )));
    }
    pos += 1;

    // Skip filler tokens.
    while pos < tokens.len() && FILLER_TOKENS.contains(&tokens[pos].as_str()) {
        pos += 1;
    }

    // Next token must be an operation.
    if pos >= tokens.len() || !OPERATION_TOKENS.contains(&tokens[pos].as_str()) {
        return Err(IntentError::NoOperation);
    }
    let operation = "distinct".to_string();
    pos += 1;

    // Skip more filler tokens.
    while pos < tokens.len() && FILLER_TOKENS.contains(&tokens[pos].as_str()) {
        pos += 1;
    }

    // Remaining tokens form the column mention.
    if pos >= tokens.len() {
        return Err(IntentError::Unsupported(
            "no column mention found".to_string(),
        ));
    }
    let column_tokens: Vec<&str> = tokens[pos..].iter().map(|s| s.as_str()).collect();

    // Match against columns.
    let mut scored: Vec<(f64, &ColumnDefinition)> = Vec::new();
    for col in columns {
        let normalized = normalize_for_match(&col.display_name);
        let col_tokens: Vec<&str> = normalized.split_whitespace().collect();
        if col_tokens.is_empty() {
            continue;
        }
        let score = score_match(&column_tokens, &col_tokens);
        if score >= MATCH_THRESHOLD {
            scored.push((score, col));
        }
    }

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    if scored.is_empty() {
        let term = column_tokens.join(" ");
        return Err(IntentError::ColumnNotFound { prompt_term: term });
    }

    // Check ambiguity: if top two are within margin, it's ambiguous.
    if scored.len() >= 2 {
        let top = scored[0].0;
        let second = scored[1].0;
        if (top - second).abs() < AMBIGUITY_MARGIN {
            let candidates = vec![
                scored[0].1.display_name.clone(),
                scored[1].1.display_name.clone(),
            ];
            return Err(IntentError::ColumnAmbiguous { candidates });
        }
    }

    let (best_score, best_col) = &scored[0];
    let evidence_str = format!(
        "token overlap: {}",
        column_tokens
            .iter()
            .map(|t| format!("'{}'", t))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let evidence = RecognitionEvidence {
        prompt_tokens: tokens.clone(),
        matched_column: Some(MatchedColumn {
            column_id: best_col.id.clone(),
            display_name: best_col.display_name.clone(),
            score: *best_score,
            evidence: evidence_str,
        }),
        operation: Some(operation.clone()),
        refusal_reason: None,
    };

    Ok(RecognizedIntent {
        operation,
        column_id: best_col.id.clone(),
        column_display_name: best_col.display_name.clone(),
        evidence,
    })
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

fn singularize(s: &str) -> &str {
    s.strip_suffix('s').unwrap_or(s)
}

fn score_match(prompt_tokens: &[&str], column_tokens: &[&str]) -> f64 {
    let mut matches = 0usize;
    for pt in prompt_tokens {
        let pt_s = singularize(pt);
        for ct in column_tokens {
            let ct_s = singularize(ct);
            if pt_s == ct_s {
                matches += 1;
                break;
            }
        }
    }
    matches as f64 / column_tokens.len() as f64
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

    #[test]
    fn extract_unique_floor_plans() {
        let cols = columns();
        let intent = recognize_intent("Extract all the unique floor plans", &cols).unwrap();
        assert_eq!(intent.operation, "distinct");
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
        assert_eq!(intent.operation, "distinct");
        assert_eq!(intent.column_id, "column-1");
    }

    #[test]
    fn show_unique_floor_plan_singular() {
        let cols = columns();
        let intent = recognize_intent("show unique floor plan", &cols).unwrap();
        assert_eq!(intent.operation, "distinct");
        assert_eq!(intent.column_id, "column-1");
    }

    #[test]
    fn get_the_unique_values_from_floor_plan() {
        let cols = columns();
        let intent = recognize_intent("get the unique values from floor plan", &cols).unwrap();
        assert_eq!(intent.operation, "distinct");
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
        assert!(matches!(err, IntentError::Unsupported(_)));
    }

    #[test]
    fn ambiguous_columns() {
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
        assert!(matches!(err, IntentError::ColumnAmbiguous { .. }));
    }

    #[test]
    fn evidence_has_correct_tokens_and_scores() {
        let cols = columns();
        let intent = recognize_intent("Extract all the unique floor plans", &cols).unwrap();
        let ev = &intent.evidence;
        assert!(ev.prompt_tokens.contains(&"extract".to_string()));
        assert!(ev.prompt_tokens.contains(&"unique".to_string()));
        assert!(ev.prompt_tokens.contains(&"floor".to_string()));
        assert!(ev.prompt_tokens.contains(&"plans".to_string()));
        let mc = ev.matched_column.as_ref().unwrap();
        assert!(mc.score >= 0.5);
        assert!(!mc.evidence.is_empty());
    }
}
