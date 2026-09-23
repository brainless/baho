use baho_model::column::ColumnDefinition;
use baho_model::{ExactDecimal, NumericParsePolicy};
use baho_plan::evidence::{
    ActionEvidence, ColumnPhraseEvidence, CompetingParseEvidence, ConnectorEvidence,
    HeaderEvidence, LiteralEvidence, MatchedColumn, ModifierEvidence, OperatorEvidence,
    ParenthesisEvidence, PromptToken, RECOGNITION_EVIDENCE_SCHEMA_VERSION, RecognitionEvidence,
    RowFilterEvidence,
};
use baho_plan::plan::{
    ComparisonOperator, DistinctKeep, Expression, Literal, PLAN_SCHEMA_VERSION_2, Plan, PlanSource,
    PlanStep,
};
use baho_plan::validation::{MAX_PREDICATE_DEPTH, MAX_PREDICATE_NODES};
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::IntentError;

const FILLER_TOKENS: &[&str] = &["all", "the", "a", "an", "of", "from", "values", "value"];

const AMBIGUITY_MARGIN: f64 = 0.1;

/// First-token aliases that begin an explicit row-filter request. Every other
/// first token goes straight to Epic 002 retrieval recognition.
const ROW_ACTION_TOKENS: &[&str] = &["list", "show", "filter"];

const ROW_ACTION_KEYWORD_TOKENS: &[&str] = &["rows", "where"];

/// Boolean connectors, including unary negation; an unquoted literal run ends
/// at any of these.
const BOOLEAN_CONNECTOR_TOKENS: &[&str] = &["and", "but", "or", "not"];

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
    text: String,
}

#[derive(Debug, Clone)]
struct CandidateParse {
    modifier: Option<CanonicalOperation>,
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
        let span_lo = match modifier_pos {
            Some((idx, _)) => idx + 1,
            None => action_end,
        };

        // The column span starts anywhere within the leading filler run so a
        // header phrase that begins with a filler word (e.g. a "Value"
        // column) stays matchable. Tokens skipped before the span start may
        // only be fillers plus the single optional modifier; the walk stops
        // at the first non-filler so control words are never silently
        // absorbed into the column phrase.
        let mut span_start = span_lo;
        while span_start < tokens.len() {
            let span_end = tokens.len();

            for col in columns {
                if let Some(match_class) = match_column_span(tokens, span_start, span_end, col) {
                    candidates.push(CandidateParse {
                        modifier: modifier_pos.map(|(_, op)| op),
                        modifier_span: modifier_pos.map(|(idx, _)| (idx, idx + 1)),
                        column_span: (span_start, span_end),
                        column: col.clone(),
                        match_class,
                        score: match_class.score(),
                    });
                }
            }

            if FILLER_TOKENS.contains(&tokens[span_start].text.as_str()) {
                span_start += 1;
            } else {
                break;
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
/// one distinct column, if any. When several spans qualify, the smallest
/// `(start, end)` wins; BTreeMap iteration is ascending, so the choice is
/// deterministic across invocations.
fn ambiguous_span(tied: &[&CandidateParse]) -> Option<(usize, usize)> {
    let mut by_span: BTreeMap<(usize, usize), Vec<&&CandidateParse>> = BTreeMap::new();
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
/// Rows collapse only when fully identical, so equally scored parses with
/// distinct spans or modifier assignments stay separately visible.
fn competing_parse_evidence(
    tied: &[&CandidateParse],
    tokens: &[IndexedToken],
) -> Vec<CompetingParseEvidence> {
    let mut seen: Vec<CompetingParseEvidence> = Vec::new();
    for c in tied {
        let entry = CompetingParseEvidence {
            column_display_name: c.column.display_name.clone(),
            score: c.score,
            column_span: c.column_span,
            modifier: c.modifier_span.map(|(start, _)| tokens[start].text.clone()),
        };
        if !seen.contains(&entry) {
            seen.push(entry);
        }
    }
    seen
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

/// Refusal-shaped recognition evidence: everything a failed parse can report
/// is the prompt tokens, the action seen so far, a stable refusal reason, and
/// any tied parses that prevented a unique choice.
fn refusal_evidence(
    prompt_tokens: Vec<PromptToken>,
    action: Option<ActionEvidence>,
    refusal_reason: &str,
    competing_parses: Vec<CompetingParseEvidence>,
) -> RecognitionEvidence {
    RecognitionEvidence {
        schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
        prompt_tokens,
        action,
        modifier: None,
        column_phrase: None,
        matched_column: None,
        match_class: None,
        canonical_operation: None,
        refusal_reason: Some(refusal_reason.to_string()),
        competing_parses,
        row_filter: None,
    }
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
            Some(refusal_evidence(
                Vec::new(),
                None,
                "intent.unsupported",
                Vec::new(),
            )),
        ));
    }

    let indexed_tokens: Vec<IndexedToken> = raw_tokens
        .into_iter()
        .map(|text| IndexedToken { text })
        .collect();

    let prompt_token_evidence: Vec<PromptToken> = indexed_tokens
        .iter()
        .enumerate()
        .map(|(index, t)| PromptToken {
            index,
            text: t.text.clone(),
        })
        .collect();

    if resolve_action(&indexed_tokens[0].text).is_none() {
        return Err(IntentError::Unsupported(
            format!("expected action verb, got '{}'", indexed_tokens[0].text),
            Some(refusal_evidence(
                prompt_token_evidence,
                None,
                "intent.unsupported",
                Vec::new(),
            )),
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
            evidence: Some(refusal_evidence(
                prompt_token_evidence,
                action_evidence,
                "intent.column_not_found",
                Vec::new(),
            )),
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
        let competing_parses = competing_parse_evidence(&tied, &indexed_tokens);
        if let Some(candidates) = column_ambiguous_candidates(&tied) {
            return Err(IntentError::ColumnAmbiguous {
                candidates,
                evidence: Some(refusal_evidence(
                    prompt_token_evidence,
                    action_evidence,
                    "intent.column_ambiguous",
                    competing_parses,
                )),
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
            evidence: Some(refusal_evidence(
                prompt_token_evidence,
                action_evidence,
                "intent.parse_ambiguous",
                competing_parses,
            )),
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
        schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
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
        row_filter: None,
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

// ==== Epic 006: constrained row-filter grammar ====

/// A recognized user request: an Epic 002 retrieval, or an Epic 006
/// explicit-column Boolean row filter.
#[derive(Debug, Clone)]
pub enum RecognizedRequest {
    Retrieval(RecognizedIntent),
    RowFilter(RowFilterIntent),
}

/// A recognized explicit-column row filter with its compiled predicate and
/// full recognition evidence.
#[derive(Debug, Clone)]
pub struct RowFilterIntent {
    pub predicate: Expression,
    pub evidence: RecognitionEvidence,
}

/// Recognize a prompt as either a row filter or a retrieval request.
///
/// A request is a row filter if and only if a predicate follows a
/// `list`/`show`/`filter` action. When no predicate is present the request
/// falls back to the Epic 002 retrieval recognizer, which retains its
/// projection and distinct meanings. When the action words `rows`/`where`
/// announce a row filter but the predicate fails to parse, the row-filter
/// refusal is reported instead of a retrieval reinterpretation.
pub fn recognize_request(
    prompt: &str,
    columns: &[ColumnDefinition],
) -> Result<RecognizedRequest, IntentError> {
    let words = tokenize_prompt(prompt);
    if let Some(first) = words.first() {
        if ROW_ACTION_TOKENS.contains(&first.normalized.as_str()) {
            if let Some(attempted) = attempt_row_filter(&words, columns) {
                return attempted.map(RecognizedRequest::RowFilter);
            }
        }
    }
    recognize_intent(prompt, columns).map(RecognizedRequest::Retrieval)
}

/// Compile a recognized request into a plan.
///
/// Retrieval requests compile to version 1 plans exactly as before; row
/// filters compile to a version 2 plan with a single filter step that
/// retains every source column in source order.
pub fn compile_request_to_plan(
    request: &RecognizedRequest,
    source_revision: &str,
    table_id: &str,
) -> Plan {
    match request {
        RecognizedRequest::Retrieval(intent) => {
            compile_intent_to_plan(intent, source_revision, table_id)
        }
        RecognizedRequest::RowFilter(intent) => Plan {
            schema_version: PLAN_SCHEMA_VERSION_2,
            source: PlanSource {
                revision: source_revision.to_string(),
                table_id: table_id.to_string(),
            },
            steps: vec![PlanStep::Filter {
                predicate: intent.predicate.clone(),
            }],
        },
    }
}

/// A prompt word after tokenization. `raw` preserves the recognized source
/// text (quotes stripped on quoted spans); `normalized` is the lowercased
/// form used for header matching and prompt-token evidence.
#[derive(Debug, Clone)]
struct PromptWord {
    raw: String,
    normalized: String,
    quoted: bool,
}

fn unquoted_word(raw: String) -> PromptWord {
    let normalized = raw.to_lowercase();
    PromptWord {
        raw,
        normalized,
        quoted: false,
    }
}

/// Tokenize a prompt into words. Whitespace separates words, parentheses are
/// split from adjacent text so `(job` and `1)` always yield separate tokens,
/// and a quoted span — including any interior whitespace — becomes one token
/// with its quotes stripped. An unterminated quote degrades to an ordinary
/// unquoted word so tokenization stays total over input-controlled text.
fn tokenize_prompt(prompt: &str) -> Vec<PromptWord> {
    let mut words = Vec::new();
    let mut chars = prompt.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            break;
        };
        if first == '(' || first == ')' {
            chars.next();
            words.push(unquoted_word(first.to_string()));
            continue;
        }
        if first == '"' {
            chars.next();
            let mut content = String::new();
            let mut closed = false;
            for c in chars.by_ref() {
                if c == '"' {
                    closed = true;
                    break;
                }
                content.push(c);
            }
            if closed {
                words.push(PromptWord {
                    normalized: content.to_lowercase(),
                    raw: content,
                    quoted: true,
                });
            } else {
                words.push(unquoted_word(format!("\"{content}")));
            }
            continue;
        }
        let mut raw = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == '(' || c == ')' || c == '"' {
                break;
            }
            raw.push(c);
            chars.next();
        }
        if !raw.is_empty() {
            words.push(unquoted_word(raw));
        }
    }
    words
}

fn comparison_operator_token(normalized: &str) -> Option<ComparisonOperator> {
    match normalized {
        "=" => Some(ComparisonOperator::Equal),
        "!=" => Some(ComparisonOperator::NotEqual),
        "<" => Some(ComparisonOperator::Less),
        "<=" => Some(ComparisonOperator::LessOrEqual),
        ">" => Some(ComparisonOperator::Greater),
        ">=" => Some(ComparisonOperator::GreaterOrEqual),
        _ => None,
    }
}

fn is_boundary_token(word: &PromptWord) -> bool {
    !word.quoted
        && (BOOLEAN_CONNECTOR_TOKENS.contains(&word.normalized.as_str())
            || word.normalized == "("
            || word.normalized == ")"
            || comparison_operator_token(&word.normalized).is_some())
}

struct HeaderPhrase {
    tokens: Vec<String>,
    column_id: String,
    display_name: String,
}

/// The complete normalized header phrases for binding, ordered longest first
/// and then by stable column ID, so candidate discovery is deterministic.
fn header_phrases(columns: &[ColumnDefinition]) -> Vec<HeaderPhrase> {
    let mut phrases: Vec<HeaderPhrase> = columns
        .iter()
        .filter_map(|column| {
            let normalized = normalize_for_match(&column.display_name);
            let tokens: Vec<String> = normalized
                .split(' ')
                .filter(|token| !token.is_empty())
                .map(str::to_string)
                .collect();
            (!tokens.is_empty()).then(|| HeaderPhrase {
                tokens,
                column_id: column.id.clone(),
                display_name: column.display_name.clone(),
            })
        })
        .collect();
    phrases.sort_by(|a, b| {
        b.tokens
            .len()
            .cmp(&a.tokens.len())
            .then_with(|| a.column_id.cmp(&b.column_id))
    });
    phrases
}

enum HeaderBinding {
    None,
    Unique {
        column_id: String,
        display_name: String,
        token_count: usize,
    },
    /// One normalized phrase claimed by more than one column: a duplicate
    /// header that cannot be bound to a single identity.
    Duplicate {
        display_names: Vec<String>,
        span: (usize, usize),
    },
    /// Complete header phrases of different lengths start at the same
    /// position, so more than one parse survives.
    Overlapping {
        display_names: Vec<String>,
        matches: Vec<(usize, String)>,
    },
}

enum Bail {
    NoPredicate,
    Unsupported(String),
    ColumnNotFound { prompt_term: String },
    LiteralInvalid { detail: String },
    Refused(IntentError),
}

struct RowFilterParser<'a> {
    words: &'a [PromptWord],
    phrases: &'a [HeaderPhrase],
    start: usize,
    pos: usize,
    group_depth: usize,
    guard_depth: usize,
    node_count: usize,
    atomics_parsed: usize,
    headers: Vec<HeaderEvidence>,
    operators: Vec<OperatorEvidence>,
    connectors: Vec<ConnectorEvidence>,
    literals: Vec<LiteralEvidence>,
    parentheses: Vec<ParenthesisEvidence>,
    prompt_tokens: Vec<PromptToken>,
    action: Option<ActionEvidence>,
}

impl<'a> RowFilterParser<'a> {
    fn new(
        words: &'a [PromptWord],
        phrases: &'a [HeaderPhrase],
        start: usize,
        prompt_tokens: Vec<PromptToken>,
        action: Option<ActionEvidence>,
    ) -> Self {
        Self {
            words,
            phrases,
            start,
            pos: start,
            group_depth: 0,
            guard_depth: 0,
            node_count: 0,
            atomics_parsed: 0,
            headers: Vec::new(),
            operators: Vec::new(),
            connectors: Vec::new(),
            literals: Vec::new(),
            parentheses: Vec::new(),
            prompt_tokens,
            action,
        }
    }

    fn normalized_at(&self, pos: usize) -> Option<&str> {
        self.words.get(pos).map(|word| word.normalized.as_str())
    }

    fn push_node(&mut self) -> Result<(), Bail> {
        self.node_count += 1;
        if self.node_count > MAX_PREDICATE_NODES {
            return Err(Bail::Unsupported(format!(
                "predicate exceeds the maximum of {MAX_PREDICATE_NODES} nodes"
            )));
        }
        Ok(())
    }

    fn enter_guard(&mut self) -> Result<(), Bail> {
        self.guard_depth += 1;
        if self.guard_depth > MAX_PREDICATE_DEPTH {
            return Err(Bail::Unsupported(format!(
                "predicate exceeds the maximum depth of {MAX_PREDICATE_DEPTH}"
            )));
        }
        Ok(())
    }

    fn refusal(&self, reason: &str, competing: Vec<CompetingParseEvidence>) -> RecognitionEvidence {
        refusal_evidence(
            self.prompt_tokens.clone(),
            self.action.clone(),
            reason,
            competing,
        )
    }

    fn bind_header(&self, pos: usize) -> HeaderBinding {
        let mut matches: Vec<&HeaderPhrase> = Vec::new();
        for phrase in self.phrases {
            let end = pos + phrase.tokens.len();
            if end > self.words.len() {
                continue;
            }
            if (0..phrase.tokens.len())
                .all(|offset| self.words[pos + offset].normalized == phrase.tokens[offset])
            {
                matches.push(phrase);
            }
        }
        if matches.is_empty() {
            return HeaderBinding::None;
        }
        let last = matches.len() - 1;
        if matches[0].tokens.len() != matches[last].tokens.len() {
            let display_names = display_names_deduped(matches.iter().map(|m| &m.display_name));
            let match_spans = matches
                .iter()
                .map(|m| (m.tokens.len(), m.display_name.clone()))
                .collect();
            return HeaderBinding::Overlapping {
                display_names,
                matches: match_spans,
            };
        }
        if matches.len() > 1 {
            let display_names = display_names_deduped(matches.iter().map(|m| &m.display_name));
            return HeaderBinding::Duplicate {
                display_names,
                span: (pos, pos + matches[0].tokens.len()),
            };
        }
        HeaderBinding::Unique {
            column_id: matches[0].column_id.clone(),
            display_name: matches[0].display_name.clone(),
            token_count: matches[0].tokens.len(),
        }
    }

    fn column_ambiguous_error(
        &self,
        display_names: Vec<String>,
        span: (usize, usize),
    ) -> IntentError {
        let competing: Vec<CompetingParseEvidence> = display_names
            .iter()
            .map(|display_name| CompetingParseEvidence {
                column_display_name: display_name.clone(),
                score: 1.0,
                column_span: span,
                modifier: None,
            })
            .collect();
        IntentError::ColumnAmbiguous {
            candidates: display_names,
            evidence: Some(self.refusal("intent.column_ambiguous", competing)),
        }
    }

    fn parse_ambiguous_error(
        &self,
        display_names: Vec<String>,
        matches: &[(usize, String)],
    ) -> IntentError {
        let competing: Vec<CompetingParseEvidence> = matches
            .iter()
            .map(|(token_count, display_name)| CompetingParseEvidence {
                column_display_name: display_name.clone(),
                score: 1.0,
                column_span: (self.pos, self.pos + token_count),
                modifier: None,
            })
            .collect();
        IntentError::ParseAmbiguous {
            candidates: display_names,
            evidence: Some(self.refusal("intent.parse_ambiguous", competing)),
        }
    }

    fn parse_or(&mut self) -> Result<Expression, Bail> {
        let mut predicates = vec![self.parse_and()?];
        while self.normalized_at(self.pos) == Some("or") {
            self.connectors.push(ConnectorEvidence {
                alias: "or".to_string(),
                span: (self.pos, self.pos + 1),
            });
            self.pos += 1;
            predicates.push(self.parse_and()?);
        }
        self.finish_nary(predicates, false)
    }

    fn parse_and(&mut self) -> Result<Expression, Bail> {
        let mut predicates = vec![self.parse_unary()?];
        while matches!(self.normalized_at(self.pos), Some("and") | Some("but")) {
            let alias = self.normalized_at(self.pos).unwrap_or_default().to_string();
            self.connectors.push(ConnectorEvidence {
                alias,
                span: (self.pos, self.pos + 1),
            });
            self.pos += 1;
            predicates.push(self.parse_unary()?);
        }
        self.finish_nary(predicates, true)
    }

    fn finish_nary(
        &mut self,
        predicates: Vec<Expression>,
        is_and: bool,
    ) -> Result<Expression, Bail> {
        let Some(first) = predicates.first() else {
            return Err(Bail::Unsupported("empty boolean expression".to_string()));
        };
        if predicates.len() == 1 {
            return Ok(first.clone());
        }
        self.push_node()?;
        Ok(if is_and {
            Expression::And { predicates }
        } else {
            Expression::Or { predicates }
        })
    }

    fn parse_unary(&mut self) -> Result<Expression, Bail> {
        let mut negations = 0;
        while self.normalized_at(self.pos) == Some("not") {
            self.connectors.push(ConnectorEvidence {
                alias: "not".to_string(),
                span: (self.pos, self.pos + 1),
            });
            self.pos += 1;
            self.enter_guard()?;
            negations += 1;
        }
        if self.pos >= self.words.len() {
            if negations == 0 && self.atomics_parsed == 0 && self.pos == self.start {
                return Err(Bail::NoPredicate);
            }
            return Err(Bail::Unsupported("expected a condition".to_string()));
        }
        let mut expression = if self.normalized_at(self.pos) == Some("(") {
            self.parse_group()?
        } else {
            self.parse_atomic()?
        };
        for _ in 0..negations {
            self.push_node()?;
            expression = Expression::Not {
                predicate: Box::new(expression),
            };
        }
        Ok(expression)
    }

    fn parse_group(&mut self) -> Result<Expression, Bail> {
        self.parentheses.push(ParenthesisEvidence {
            text: "(".to_string(),
            span: (self.pos, self.pos + 1),
        });
        self.pos += 1;
        self.group_depth += 1;
        self.enter_guard()?;
        let parsed = self.parse_or();
        self.group_depth -= 1;
        let expression = parsed?;
        if self.normalized_at(self.pos) != Some(")") {
            return Err(Bail::Unsupported(
                "expected ')' to close the parenthesized predicate".to_string(),
            ));
        }
        self.parentheses.push(ParenthesisEvidence {
            text: ")".to_string(),
            span: (self.pos, self.pos + 1),
        });
        self.pos += 1;
        Ok(expression)
    }

    /// Whether the parse so far is exactly one bare header phrase at the
    /// predicate start: nothing predicate-shaped is present and the request
    /// belongs to the retrieval grammar.
    fn at_bare_start(&self) -> bool {
        self.atomics_parsed == 1
            && self.headers.len() == 1
            && self.operators.is_empty()
            && self.literals.is_empty()
            && self.connectors.is_empty()
            && self.parentheses.is_empty()
            && self.group_depth == 0
    }

    fn parse_atomic(&mut self) -> Result<Expression, Bail> {
        match self.bind_header(self.pos) {
            HeaderBinding::None => {
                let prompt_term = self.words[self.pos..]
                    .iter()
                    .map(|word| word.normalized.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                Err(Bail::ColumnNotFound { prompt_term })
            }
            HeaderBinding::Duplicate {
                display_names,
                span,
            } => Err(Bail::Refused(
                self.column_ambiguous_error(display_names, span),
            )),
            HeaderBinding::Overlapping {
                display_names,
                matches,
            } => Err(Bail::Refused(
                self.parse_ambiguous_error(display_names, &matches),
            )),
            HeaderBinding::Unique {
                column_id,
                display_name,
                token_count,
            } => {
                self.push_node()?;
                let span = (self.pos, self.pos + token_count);
                self.headers.push(HeaderEvidence {
                    tokens: (0..token_count)
                        .map(|offset| self.words[self.pos + offset].normalized.clone())
                        .collect(),
                    span,
                    column_id: column_id.clone(),
                    display_name,
                });
                self.pos += token_count;
                self.atomics_parsed += 1;

                if let Some(operator) = self
                    .normalized_at(self.pos)
                    .and_then(comparison_operator_token)
                {
                    self.operators.push(OperatorEvidence {
                        operator,
                        span: (self.pos, self.pos + 1),
                    });
                    self.pos += 1;
                    let literal = self.parse_literal(false)?;
                    return Ok(Expression::Compare {
                        column: column_id,
                        operator,
                        literal,
                    });
                }
                if self.pos >= self.words.len() {
                    if self.at_bare_start() {
                        return Err(Bail::NoPredicate);
                    }
                    return Err(Bail::Unsupported(
                        "expected a comparison or a text literal after the column".to_string(),
                    ));
                }
                if is_boundary_token(&self.words[self.pos]) {
                    return Err(Bail::Unsupported(
                        "expected a comparison operator or a text literal after the column"
                            .to_string(),
                    ));
                }
                let literal = self.parse_literal(true)?;
                Ok(Expression::Compare {
                    column: column_id,
                    operator: ComparisonOperator::Equal,
                    literal,
                })
            }
        }
    }

    /// Parse one literal at the current position and record its evidence.
    ///
    /// A quoted span is one text token. An unquoted literal run ends at the
    /// next Boolean connector, parenthesis, or comparison operator. With the
    /// explicit comparison forms an unquoted single token containing an ASCII
    /// digit must be a strict exact decimal, so `10,000` or `1.2.3` refuse as
    /// invalid literals. The implicit-equality form is sugar for `=` with a
    /// quoted text literal, so it never produces decimals or literal
    /// refusals.
    fn parse_literal(&mut self, force_text: bool) -> Result<Literal, Bail> {
        let start = self.pos;
        if start >= self.words.len() {
            return Err(Bail::Unsupported(
                "expected a literal after the comparison operator".to_string(),
            ));
        }
        if is_boundary_token(&self.words[start]) {
            return Err(Bail::Unsupported(
                "expected a literal after the comparison operator".to_string(),
            ));
        }
        let end = if self.words[start].quoted {
            start + 1
        } else {
            let mut run_end = start;
            while run_end < self.words.len() && !is_boundary_token(&self.words[run_end]) {
                run_end += 1;
            }
            run_end
        };
        let raw_text = self.words[start..end]
            .iter()
            .map(|word| word.raw.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let first = &self.words[start];
        let (literal, parser_policy) = if first.quoted || force_text {
            (Literal::Text(raw_text.clone()), None)
        } else if first.raw.bytes().any(|byte| byte.is_ascii_digit()) {
            if end != start + 1 {
                return Err(Bail::LiteralInvalid {
                    detail: format!("numeric literal '{raw_text}' must be a single token"),
                });
            }
            match ExactDecimal::parse(&first.raw) {
                Ok(value) => (
                    Literal::Decimal(value),
                    Some(NumericParsePolicy::StrictDecimal),
                ),
                Err(error) => {
                    return Err(Bail::LiteralInvalid {
                        detail: format!("decimal literal '{raw_text}': {error}"),
                    });
                }
            }
        } else {
            (Literal::Text(raw_text.clone()), None)
        };
        self.literals.push(LiteralEvidence {
            raw_text,
            literal: literal.clone(),
            parser_policy,
            span: (start, end),
        });
        self.pos = end;
        Ok(literal)
    }
}

fn display_names_deduped<'a, I: Iterator<Item = &'a String>>(names: I) -> Vec<String> {
    let mut deduped: Vec<String> = Vec::new();
    for name in names {
        let normalized = normalize_for_match(name);
        if !deduped
            .iter()
            .any(|existing: &String| normalize_for_match(existing) == normalized)
        {
            deduped.push(name.clone());
        }
    }
    deduped
}

/// Attempt recognition of an explicit row-filter request.
///
/// Returns `None` when the tokens after the action hold nothing
/// predicate-shaped (no `rows`/`where` keywords and only a bare header
/// phrase), so the caller must fall back to retrieval recognition. A `Some`
/// result is the row-filter outcome, including every refusal.
fn attempt_row_filter(
    words: &[PromptWord],
    columns: &[ColumnDefinition],
) -> Option<Result<RowFilterIntent, IntentError>> {
    let prompt_tokens: Vec<PromptToken> = words
        .iter()
        .enumerate()
        .map(|(index, word)| PromptToken {
            index,
            text: word.normalized.clone(),
        })
        .collect();
    let action = Some(ActionEvidence {
        alias: words[0].normalized.clone(),
        span: (0, 1),
    });

    let mut pos = 1;
    let mut keywords = 0;
    while pos < words.len() && ROW_ACTION_KEYWORD_TOKENS.contains(&words[pos].normalized.as_str()) {
        keywords += 1;
        pos += 1;
    }

    let phrases = header_phrases(columns);
    let mut parser =
        RowFilterParser::new(words, &phrases, pos, prompt_tokens.clone(), action.clone());
    let predicate = match parser.parse_or() {
        Ok(predicate) => predicate,
        Err(Bail::NoPredicate) => {
            if keywords > 0 {
                return Some(Err(IntentError::PredicateUnsupported(
                    "expected a predicate condition after 'rows' or 'where'".to_string(),
                    Some(refusal_evidence(
                        prompt_tokens,
                        action,
                        "intent.predicate_unsupported",
                        Vec::new(),
                    )),
                )));
            }
            return None;
        }
        Err(bail) => {
            // Without the `rows`/`where` keywords the row-filter attempt is
            // silent: any refusal defers to retrieval recognition so the
            // Epic 002 projection meanings are never reinterpreted.
            if keywords > 0 {
                return Some(Err(intent_error_from_bail(bail, prompt_tokens, action)));
            }
            return None;
        }
    };
    if parser.pos < words.len() {
        return Some(Err(IntentError::PredicateUnsupported(
            format!(
                "unexpected token '{}' after the predicate",
                words[parser.pos].raw
            ),
            Some(refusal_evidence(
                prompt_tokens,
                action,
                "intent.predicate_unsupported",
                Vec::new(),
            )),
        )));
    }

    let RowFilterParser {
        headers,
        operators,
        connectors,
        literals,
        parentheses,
        ..
    } = parser;
    let evidence = RecognitionEvidence {
        schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
        prompt_tokens,
        action: action.clone(),
        modifier: None,
        column_phrase: None,
        matched_column: None,
        match_class: None,
        canonical_operation: Some("row_filter".to_string()),
        refusal_reason: None,
        competing_parses: Vec::new(),
        row_filter: Some(RowFilterEvidence {
            action,
            headers,
            operators,
            connectors,
            literals,
            parentheses,
            predicate: Some(predicate.clone()),
            plan_schema_version: PLAN_SCHEMA_VERSION_2,
        }),
    };
    Some(Ok(RowFilterIntent {
        predicate,
        evidence,
    }))
}

fn intent_error_from_bail(
    bail: Bail,
    prompt_tokens: Vec<PromptToken>,
    action: Option<ActionEvidence>,
) -> IntentError {
    match bail {
        Bail::Unsupported(detail) => IntentError::PredicateUnsupported(
            detail,
            Some(refusal_evidence(
                prompt_tokens,
                action,
                "intent.predicate_unsupported",
                Vec::new(),
            )),
        ),
        Bail::ColumnNotFound { prompt_term } => IntentError::ColumnNotFound {
            prompt_term,
            evidence: Some(refusal_evidence(
                prompt_tokens,
                action,
                "intent.column_not_found",
                Vec::new(),
            )),
        },
        Bail::LiteralInvalid { detail } => IntentError::LiteralInvalid {
            detail,
            evidence: Some(refusal_evidence(
                prompt_tokens,
                action,
                "intent.literal_invalid",
                Vec::new(),
            )),
        },
        Bail::Refused(error) => error,
        Bail::NoPredicate => IntentError::PredicateUnsupported(
            "expected a predicate condition".to_string(),
            Some(refusal_evidence(
                prompt_tokens,
                action,
                "intent.predicate_unsupported",
                Vec::new(),
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_model::ExactDecimal;
    use baho_plan::plan::PLAN_SCHEMA_VERSION_1;
    use baho_plan::validation::{validate_plan_references, validate_plan_structure};

    fn dec(text: &str) -> ExactDecimal {
        ExactDecimal::parse(text).unwrap()
    }

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

    fn income_columns() -> Vec<ColumnDefinition> {
        vec![ColumnDefinition {
            id: "col-income".to_string(),
            ordinal: 0,
            source_header_raw: Some("Income".to_string()),
            source_header_normalized: Some("income".to_string()),
            display_name: "Income".to_string(),
        }]
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
    fn every_action_alias_maps_to_canonical_retrieval() {
        let cols = income_columns();
        for alias in [
            "extract", "list", "show", "get", "find", "display", "return",
        ] {
            let intent = recognize_intent(&format!("{alias} income"), &cols)
                .unwrap_or_else(|e| panic!("action alias '{alias}' failed: {e:?}"));
            assert_eq!(intent.action, CanonicalAction::Retrieve, "alias {alias}");
            assert_eq!(
                intent.operation,
                CanonicalOperation::Select,
                "alias {alias}"
            );
            assert_eq!(intent.column_id, "col-income", "alias {alias}");
            assert_eq!(
                intent.evidence.canonical_operation.as_deref(),
                Some("select"),
                "alias {alias}"
            );
        }
    }

    #[test]
    fn every_distinct_modifier_alias_sets_distinct_operation() {
        let cols = income_columns();
        for alias in ["unique", "distinct", "deduplicate", "deduplicated"] {
            let intent = recognize_intent(&format!("list {alias} income"), &cols)
                .unwrap_or_else(|e| panic!("modifier alias '{alias}' failed: {e:?}"));
            assert_eq!(intent.action, CanonicalAction::Retrieve, "alias {alias}");
            assert_eq!(
                intent.operation,
                CanonicalOperation::Distinct,
                "alias {alias}"
            );
            assert_eq!(intent.column_id, "col-income", "alias {alias}");
            assert_eq!(
                intent.evidence.canonical_operation.as_deref(),
                Some("distinct"),
                "alias {alias}"
            );
            let modifier = intent.evidence.modifier.as_ref().unwrap();
            assert_eq!(modifier.alias, alias, "alias {alias}");
            assert_eq!(modifier.span, (1, 2), "alias {alias}");
        }
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
    fn multiple_qualifying_spans_report_smallest_span_deterministically() {
        let cols = vec![
            ColumnDefinition {
                id: "col-income-a".to_string(),
                ordinal: 0,
                source_header_raw: Some("Income".to_string()),
                source_header_normalized: Some("income".to_string()),
                display_name: "Income".to_string(),
            },
            ColumnDefinition {
                id: "col-income-b".to_string(),
                ordinal: 1,
                source_header_raw: Some("Income".to_string()),
                source_header_normalized: Some("income".to_string()),
                display_name: "Income".to_string(),
            },
            ColumnDefinition {
                id: "col-unique-income-a".to_string(),
                ordinal: 2,
                source_header_raw: Some("Unique Income".to_string()),
                source_header_normalized: Some("unique income".to_string()),
                display_name: "Unique Income".to_string(),
            },
            ColumnDefinition {
                id: "col-unique-income-b".to_string(),
                ordinal: 3,
                source_header_raw: Some("Unique Income".to_string()),
                source_header_normalized: Some("unique income".to_string()),
                display_name: "Unique Income".to_string(),
            },
        ];
        // Two spans qualify: "unique income" (1, 3) matches both Unique Income
        // columns and "income" (2, 3) matches both Income columns. Span
        // selection previously depended on HashMap iteration order with a
        // per-instance RandomState, so the reported candidates varied between
        // calls in the same process. The smallest qualifying span now wins:
        // "unique income" sorts first, and its two columns share a display
        // name, which dedups the candidates list to one entry.
        let mut expected: Option<(Vec<String>, Vec<(String, f64)>)> = None;
        for _ in 0..25 {
            let err = recognize_intent("list unique income", &cols).unwrap_err();
            match err {
                IntentError::ColumnAmbiguous {
                    candidates,
                    evidence,
                } => {
                    let ev = evidence.expect("column_ambiguous must carry evidence");
                    let (reason, competing) = as_refusal(&ev);
                    assert_eq!(reason.as_deref(), Some("intent.column_ambiguous"));
                    assert_eq!(candidates, vec!["Unique Income".to_string()]);
                    let rows: Vec<(String, f64)> = competing
                        .iter()
                        .map(|c| (c.column_display_name.clone(), c.score))
                        .collect();
                    let observed = (candidates, rows);
                    if let Some(expected) = &expected {
                        assert_eq!(
                            expected, &observed,
                            "repeated recognition must produce identical evidence"
                        );
                    } else {
                        expected = Some(observed);
                    }
                }
                other => panic!("expected ColumnAmbiguous, got {other:?}"),
            }
        }
        assert!(expected.is_some());
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
                schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
                prompt_tokens: Vec::new(),
                action: None,
                modifier: None,
                column_phrase: None,
                matched_column: None,
                match_class: None,
                canonical_operation: None,
                refusal_reason: None,
                competing_parses: Vec::new(),
                row_filter: None,
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

    #[test]
    fn filler_word_header_is_selectable() {
        let cols = vec![ColumnDefinition {
            id: "col-value".to_string(),
            ordinal: 0,
            source_header_raw: Some("Value".to_string()),
            source_header_normalized: Some("value".to_string()),
            display_name: "Value".to_string(),
        }];
        // "value" is itself a filler token; the span must still be able to
        // start on it rather than skipping past the whole filler run.
        let intent = recognize_intent("list value", &cols).unwrap();
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "col-value");
        assert_eq!(intent.column_display_name, "Value");
    }

    #[test]
    fn filler_word_header_after_filler_run_is_selectable() {
        let cols = vec![ColumnDefinition {
            id: "col-values".to_string(),
            ordinal: 0,
            source_header_raw: Some("Values".to_string()),
            source_header_normalized: Some("values".to_string()),
            display_name: "Values".to_string(),
        }];
        let intent = recognize_intent("get the values", &cols).unwrap();
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "col-values");
        let col_phrase = intent.evidence.column_phrase.as_ref().unwrap();
        assert_eq!(col_phrase.tokens, vec!["values"]);
        assert_eq!(col_phrase.span, (2, 3));
    }

    #[test]
    fn filler_starting_header_phrases_are_parse_ambiguous() {
        let cols = vec![
            ColumnDefinition {
                id: "col-values".to_string(),
                ordinal: 0,
                source_header_raw: Some("Values".to_string()),
                source_header_normalized: Some("values".to_string()),
                display_name: "Values".to_string(),
            },
            ColumnDefinition {
                id: "col-the-values".to_string(),
                ordinal: 1,
                source_header_raw: Some("The Values".to_string()),
                source_header_normalized: Some("the values".to_string()),
                display_name: "The Values".to_string(),
            },
        ];
        // Both "the values" and "values" exact-match distinct columns, so the
        // two complete parses tie and recognition must refuse.
        let err = recognize_intent("list the values", &cols).unwrap_err();
        match err {
            IntentError::ParseAmbiguous { evidence, .. } => {
                let ev = evidence.expect("parse_ambiguous must carry evidence");
                let rows: Vec<(&str, (usize, usize))> = ev
                    .competing_parses
                    .iter()
                    .map(|c| (c.column_display_name.as_str(), c.column_span))
                    .collect();
                assert_eq!(rows, vec![("The Values", (1, 3)), ("Values", (2, 3))]);
            }
            other => panic!("expected ParseAmbiguous, got {other:?}"),
        }
    }

    #[test]
    fn mid_phrase_filler_header_matches_exactly() {
        let cols = vec![ColumnDefinition {
            id: "col-total-sales".to_string(),
            ordinal: 0,
            source_header_raw: Some("Total of Sales".to_string()),
            source_header_normalized: Some("total of sales".to_string()),
            display_name: "Total of Sales".to_string(),
        }];
        let intent = recognize_intent("list total of sales", &cols).unwrap();
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "col-total-sales");
        assert_eq!(intent.evidence.column_phrase.as_ref().unwrap().span, (1, 4));
    }

    #[test]
    fn leading_fillers_without_modifier_still_resolve() {
        let cols = columns();
        let intent = recognize_intent("extract all the floor plans", &cols).unwrap();
        assert_eq!(intent.operation, CanonicalOperation::Select);
        assert_eq!(intent.column_id, "column-1");
        assert_eq!(intent.column_display_name, "Floor Plan");
        assert_eq!(intent.evidence.column_phrase.as_ref().unwrap().span, (3, 5));
    }

    // ==== Epic 006: constrained grammar, exact header binding, and plans ====

    fn job_columns() -> Vec<ColumnDefinition> {
        vec![
            ColumnDefinition {
                id: "column-0".to_string(),
                ordinal: 0,
                source_header_raw: Some("Job".to_string()),
                source_header_normalized: Some("job".to_string()),
                display_name: "Job".to_string(),
            },
            ColumnDefinition {
                id: "column-1".to_string(),
                ordinal: 1,
                source_header_raw: Some("Annual Income".to_string()),
                source_header_normalized: Some("annual income".to_string()),
                display_name: "Annual Income".to_string(),
            },
        ]
    }

    fn income_annual_income_columns() -> Vec<ColumnDefinition> {
        vec![
            ColumnDefinition {
                id: "column-0".to_string(),
                ordinal: 0,
                source_header_raw: Some("Income".to_string()),
                source_header_normalized: Some("income".to_string()),
                display_name: "Income".to_string(),
            },
            ColumnDefinition {
                id: "column-1".to_string(),
                ordinal: 1,
                source_header_raw: Some("Annual Income".to_string()),
                source_header_normalized: Some("annual income".to_string()),
                display_name: "Annual Income".to_string(),
            },
        ]
    }

    fn predicate_columns() -> Vec<ColumnDefinition> {
        vec![
            ColumnDefinition {
                id: "column-0".to_string(),
                ordinal: 0,
                source_header_raw: Some("Job".to_string()),
                source_header_normalized: Some("job".to_string()),
                display_name: "Job".to_string(),
            },
            ColumnDefinition {
                id: "column-1".to_string(),
                ordinal: 1,
                source_header_raw: Some("Status".to_string()),
                source_header_normalized: Some("status".to_string()),
                display_name: "Status".to_string(),
            },
            ColumnDefinition {
                id: "column-2".to_string(),
                ordinal: 2,
                source_header_raw: Some("City".to_string()),
                source_header_normalized: Some("city".to_string()),
                display_name: "City".to_string(),
            },
        ]
    }

    fn expect_row_filter(
        request: Result<RecognizedRequest, IntentError>,
    ) -> Result<RowFilterIntent, IntentError> {
        match request {
            Ok(RecognizedRequest::RowFilter(intent)) => Ok(intent),
            Err(error) => Err(error),
            Ok(RecognizedRequest::Retrieval(intent)) => {
                panic!("expected a row filter request, got retrieval {intent:?}")
            }
        }
    }

    fn expect_row_filter_ok(request: Result<RecognizedRequest, IntentError>) -> RowFilterIntent {
        expect_row_filter(request).unwrap_or_else(|e| panic!("expected success, got {e:?}"))
    }

    fn refusal_reason_of(error: &IntentError) -> String {
        match error {
            IntentError::Unsupported(_, evidence)
            | IntentError::PredicateUnsupported(_, evidence) => {
                evidence.as_ref().and_then(|e| e.refusal_reason.clone())
            }
            IntentError::ColumnNotFound { evidence, .. }
            | IntentError::ColumnAmbiguous { evidence, .. }
            | IntentError::ParseAmbiguous { evidence, .. }
            | IntentError::LiteralInvalid { evidence, .. } => {
                evidence.as_ref().and_then(|e| e.refusal_reason.clone())
            }
        }
        .unwrap_or_else(|| panic!("refusal must carry evidence"))
    }

    fn canonical_row_filter_expected_evidence() -> RecognitionEvidence {
        RecognitionEvidence {
            schema_version: RECOGNITION_EVIDENCE_SCHEMA_VERSION,
            prompt_tokens: [
                "list",
                "rows",
                "where",
                "job",
                "=",
                "unemployed",
                "or",
                "annual",
                "income",
                "<",
                "10000",
            ]
            .iter()
            .enumerate()
            .map(|(index, text)| PromptToken {
                index,
                text: text.to_string(),
            })
            .collect(),
            action: Some(ActionEvidence {
                alias: "list".to_string(),
                span: (0, 1),
            }),
            modifier: None,
            column_phrase: None,
            matched_column: None,
            match_class: None,
            canonical_operation: Some("row_filter".to_string()),
            refusal_reason: None,
            competing_parses: Vec::new(),
            row_filter: Some(RowFilterEvidence {
                action: Some(ActionEvidence {
                    alias: "list".to_string(),
                    span: (0, 1),
                }),
                headers: vec![
                    HeaderEvidence {
                        tokens: vec!["job".to_string()],
                        span: (3, 4),
                        column_id: "column-0".to_string(),
                        display_name: "Job".to_string(),
                    },
                    HeaderEvidence {
                        tokens: vec!["annual".to_string(), "income".to_string()],
                        span: (7, 9),
                        column_id: "column-1".to_string(),
                        display_name: "Annual Income".to_string(),
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
                    alias: "or".to_string(),
                    span: (6, 7),
                }],
                literals: vec![
                    LiteralEvidence {
                        raw_text: "unemployed".to_string(),
                        literal: Literal::Text("unemployed".to_string()),
                        parser_policy: None,
                        span: (5, 6),
                    },
                    LiteralEvidence {
                        raw_text: "10000".to_string(),
                        literal: Literal::Decimal(dec("10000")),
                        parser_policy: Some(NumericParsePolicy::StrictDecimal),
                        span: (10, 11),
                    },
                ],
                parentheses: Vec::new(),
                predicate: Some(Expression::Or {
                    predicates: vec![
                        Expression::Compare {
                            column: "column-0".to_string(),
                            operator: ComparisonOperator::Equal,
                            literal: Literal::Text("unemployed".to_string()),
                        },
                        Expression::Compare {
                            column: "column-1".to_string(),
                            operator: ComparisonOperator::Less,
                            literal: Literal::Decimal(dec("10000")),
                        },
                    ],
                }),
                plan_schema_version: RowFilterEvidence::PLAN_SCHEMA_VERSION,
            }),
        }
    }

    #[test]
    fn canonical_row_filter_prompt_compiles_to_version_2_plan() {
        let cols = job_columns();
        let request = recognize_request(
            "List rows where Job = unemployed or Annual Income < 10000",
            &cols,
        )
        .unwrap();
        let intent = expect_row_filter_ok(Ok(request.clone()));

        assert_eq!(
            intent.predicate,
            Expression::Or {
                predicates: vec![
                    Expression::Compare {
                        column: "column-0".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Text("unemployed".to_string()),
                    },
                    Expression::Compare {
                        column: "column-1".to_string(),
                        operator: ComparisonOperator::Less,
                        literal: Literal::Decimal(dec("10000")),
                    },
                ],
            }
        );
        assert_eq!(intent.evidence, canonical_row_filter_expected_evidence());

        let plan = compile_request_to_plan(&request, "rev-1", "table-0");
        assert_eq!(plan.schema_version, PLAN_SCHEMA_VERSION_2);
        assert_eq!(
            plan.steps,
            vec![PlanStep::Filter {
                predicate: intent.predicate.clone()
            }]
        );
        assert!(validate_plan_structure(&plan).is_ok());
        assert!(
            validate_plan_references(&plan, &["column-0".to_string(), "column-1".to_string()])
                .is_ok()
        );
        assert!(
            !plan
                .steps
                .iter()
                .any(|step| matches!(step, PlanStep::Select { .. })),
            "row filter retains all columns and needs no select step"
        );
    }

    #[test]
    fn compact_implicit_equality_form_is_a_row_filter() {
        let cols = job_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List job unemployed or annual income < 10000",
            &cols,
        ));

        assert_eq!(
            intent.predicate,
            Expression::Or {
                predicates: vec![
                    Expression::Compare {
                        column: "column-0".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Text("unemployed".to_string()),
                    },
                    Expression::Compare {
                        column: "column-1".to_string(),
                        operator: ComparisonOperator::Less,
                        literal: Literal::Decimal(dec("10000")),
                    },
                ],
            }
        );

        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(row_filter.headers[0].tokens, vec!["job".to_string()]);
        assert_eq!(row_filter.headers[0].span, (1, 2));
        assert_eq!(row_filter.headers[0].display_name, "Job");
        assert_eq!(
            row_filter.headers[1].tokens,
            vec!["annual".to_string(), "income".to_string()]
        );
        assert_eq!(row_filter.headers[1].span, (4, 6));
        assert_eq!(row_filter.headers[1].display_name, "Annual Income");
        assert_eq!(
            row_filter.connectors,
            vec![ConnectorEvidence {
                alias: "or".to_string(),
                span: (3, 4),
            }]
        );
        assert_eq!(
            row_filter.operators,
            vec![OperatorEvidence {
                operator: ComparisonOperator::Less,
                span: (6, 7),
            }]
        );
        assert_eq!(row_filter.literals[0].raw_text, "unemployed");
        assert_eq!(row_filter.literals[0].span, (2, 3));
        assert_eq!(row_filter.literals[0].parser_policy, None);
        assert_eq!(row_filter.literals[1].raw_text, "10000");
        assert_eq!(row_filter.literals[1].span, (7, 8));
        assert_eq!(
            row_filter.literals[1].parser_policy,
            Some(NumericParsePolicy::StrictDecimal)
        );
        assert_eq!(
            intent.evidence.canonical_operation.as_deref(),
            Some("row_filter")
        );
    }

    #[test]
    fn row_action_aliases_recognize_filters() {
        let cols = predicate_columns();
        for alias in ["list", "show", "filter"] {
            let intent = expect_row_filter_ok(recognize_request(
                &format!("{alias} rows where job = retired"),
                &cols,
            ));
            assert_eq!(
                intent.predicate,
                Expression::Compare {
                    column: "column-0".to_string(),
                    operator: ComparisonOperator::Equal,
                    literal: Literal::Text("retired".to_string()),
                },
                "alias {alias}"
            );
            assert_eq!(
                intent.evidence.action.as_ref().unwrap().alias,
                alias,
                "alias {alias}"
            );
        }
    }

    #[test]
    fn or_binds_looser_than_and() {
        let cols = predicate_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where job = 1 or status = 2 and city = 3",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::Or {
                predicates: vec![
                    Expression::Compare {
                        column: "column-0".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Decimal(dec("1")),
                    },
                    Expression::And {
                        predicates: vec![
                            Expression::Compare {
                                column: "column-1".to_string(),
                                operator: ComparisonOperator::Equal,
                                literal: Literal::Decimal(dec("2")),
                            },
                            Expression::Compare {
                                column: "column-2".to_string(),
                                operator: ComparisonOperator::Equal,
                                literal: Literal::Decimal(dec("3")),
                            },
                        ],
                    },
                ],
            }
        );
    }

    #[test]
    fn not_binds_tighter_than_and() {
        let cols = predicate_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where not job = 1 and status = 2",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::And {
                predicates: vec![
                    Expression::Not {
                        predicate: Box::new(Expression::Compare {
                            column: "column-0".to_string(),
                            operator: ComparisonOperator::Equal,
                            literal: Literal::Decimal(dec("1")),
                        }),
                    },
                    Expression::Compare {
                        column: "column-1".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Decimal(dec("2")),
                    },
                ],
            }
        );
    }

    #[test]
    fn but_is_and_and_but_not_negates_the_second_condition() {
        let cols = predicate_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where job = 1 but not status = 2",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::And {
                predicates: vec![
                    Expression::Compare {
                        column: "column-0".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Decimal(dec("1")),
                    },
                    Expression::Not {
                        predicate: Box::new(Expression::Compare {
                            column: "column-1".to_string(),
                            operator: ComparisonOperator::Equal,
                            literal: Literal::Decimal(dec("2")),
                        }),
                    },
                ],
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(
            row_filter.connectors,
            vec![
                ConnectorEvidence {
                    alias: "but".to_string(),
                    span: (6, 7),
                },
                ConnectorEvidence {
                    alias: "not".to_string(),
                    span: (7, 8),
                },
            ]
        );
    }

    #[test]
    fn parentheses_override_precedence() {
        let cols = predicate_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where ( job = 1 or status = 2 ) and city = 3",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::And {
                predicates: vec![
                    Expression::Or {
                        predicates: vec![
                            Expression::Compare {
                                column: "column-0".to_string(),
                                operator: ComparisonOperator::Equal,
                                literal: Literal::Decimal(dec("1")),
                            },
                            Expression::Compare {
                                column: "column-1".to_string(),
                                operator: ComparisonOperator::Equal,
                                literal: Literal::Decimal(dec("2")),
                            },
                        ],
                    },
                    Expression::Compare {
                        column: "column-2".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Decimal(dec("3")),
                    },
                ],
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(
            row_filter.parentheses,
            vec![
                ParenthesisEvidence {
                    text: "(".to_string(),
                    span: (3, 4),
                },
                ParenthesisEvidence {
                    text: ")".to_string(),
                    span: (11, 12),
                },
            ]
        );
    }

    #[test]
    fn tokenizer_splits_parens_attached_to_words() {
        let cols = predicate_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where (job = 1) and status = 2",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::And {
                predicates: vec![
                    Expression::Compare {
                        column: "column-0".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Decimal(dec("1")),
                    },
                    Expression::Compare {
                        column: "column-1".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Decimal(dec("2")),
                    },
                ],
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(row_filter.parentheses[0].span, (3, 4));
        assert_eq!(row_filter.parentheses[1].span, (7, 8));
    }

    #[test]
    fn longest_complete_header_binds_before_shorter() {
        let cols = job_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where Annual Income < 10000",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::Compare {
                column: "column-1".to_string(),
                operator: ComparisonOperator::Less,
                literal: Literal::Decimal(dec("10000")),
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(row_filter.headers.len(), 1);
        assert_eq!(row_filter.headers[0].display_name, "Annual Income");
        assert_eq!(row_filter.headers[0].span, (3, 5));
    }

    #[test]
    fn shorter_header_binds_alone_when_no_longer_header_covers_the_phrase() {
        let cols = income_annual_income_columns();
        let intent =
            expect_row_filter_ok(recognize_request("List rows where Income < 10000", &cols));
        assert_eq!(
            intent.predicate,
            Expression::Compare {
                column: "column-0".to_string(),
                operator: ComparisonOperator::Less,
                literal: Literal::Decimal(dec("10000")),
            }
        );
    }

    #[test]
    fn row_prompt_without_bindable_predicate_falls_back_to_retrieval() {
        let cols = income_columns();
        let request = recognize_request("List rows where annual income < 10000", &cols);
        match request {
            Err(IntentError::ColumnNotFound { prompt_term, .. }) => {
                assert_eq!(prompt_term, "annual income < 10000");
            }
            other => panic!("expected retrieval fallback column_not_found, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_normalized_headers_refuse_in_row_filter() {
        let cols = vec![
            ColumnDefinition {
                id: "col-job".to_string(),
                ordinal: 0,
                source_header_raw: Some("Job".to_string()),
                source_header_normalized: Some("job".to_string()),
                display_name: "Job".to_string(),
            },
            ColumnDefinition {
                id: "col-job-2".to_string(),
                ordinal: 1,
                source_header_raw: Some("job".to_string()),
                source_header_normalized: Some("job".to_string()),
                display_name: "job".to_string(),
            },
        ];
        let err =
            expect_row_filter(recognize_request("List rows where Job = x", &cols)).unwrap_err();
        match err {
            IntentError::ColumnAmbiguous { ref candidates, .. } => {
                assert_eq!(candidates.as_slice(), ["Job".to_string()]);
            }
            other => panic!("expected ColumnAmbiguous, got {other:?}"),
        }
        let err = err;
        assert_eq!(refusal_reason_of(&err), "intent.column_ambiguous");
        let IntentError::ColumnAmbiguous { evidence, .. } = err else {
            unreachable!()
        };
        let evidence = evidence.unwrap();
        let competing: Vec<(&str, (usize, usize))> = evidence
            .competing_parses
            .iter()
            .map(|c| (c.column_display_name.as_str(), c.column_span))
            .collect();
        assert_eq!(competing, vec![("Job", (3, 4))]);
    }

    #[test]
    fn unknown_column_inside_predicate_refuses_column_not_found() {
        let cols = job_columns();
        let err = expect_row_filter(recognize_request(
            "List rows where Job = x or Nonsense = 2",
            &cols,
        ))
        .unwrap_err();
        assert!(matches!(err, IntentError::ColumnNotFound { .. }));
        assert_eq!(refusal_reason_of(&err), "intent.column_not_found");
        let IntentError::ColumnNotFound { prompt_term, .. } = err else {
            unreachable!()
        };
        assert_eq!(prompt_term, "nonsense = 2");
    }

    #[test]
    fn malformed_predicate_grammar_refuses_predicate_unsupported() {
        let cols = job_columns();
        for prompt in [
            "List rows where Job =",
            "List rows where ( Job = 1",
            "List rows where not",
            "List rows where Job = 1 or",
        ] {
            let err = expect_row_filter(recognize_request(prompt, &cols)).unwrap_err();
            assert!(
                matches!(err, IntentError::PredicateUnsupported(_, _)),
                "expected PredicateUnsupported for {prompt}, got {err:?}"
            );
            assert_eq!(
                refusal_reason_of(&err),
                "intent.predicate_unsupported",
                "prompt {prompt}"
            );
        }
    }

    #[test]
    fn grouped_numeric_literals_refuse_as_invalid() {
        let cols = job_columns();
        for prompt in [
            "List rows where Annual Income < 10,000",
            "List rows where Job = 10,000",
            "List rows where Annual Income < 1.2.3",
        ] {
            let err = expect_row_filter(recognize_request(prompt, &cols)).unwrap_err();
            assert!(
                matches!(err, IntentError::LiteralInvalid { .. }),
                "expected LiteralInvalid for {prompt}, got {err:?}"
            );
            assert_eq!(
                refusal_reason_of(&err),
                "intent.literal_invalid",
                "prompt {prompt}"
            );
        }
    }

    #[test]
    fn overlapping_header_prefixes_refuse_as_parse_ambiguous() {
        let cols = vec![
            ColumnDefinition {
                id: "col-annual".to_string(),
                ordinal: 0,
                source_header_raw: Some("Annual".to_string()),
                source_header_normalized: Some("annual".to_string()),
                display_name: "Annual".to_string(),
            },
            ColumnDefinition {
                id: "col-annual-income".to_string(),
                ordinal: 1,
                source_header_raw: Some("Annual Income".to_string()),
                source_header_normalized: Some("annual income".to_string()),
                display_name: "Annual Income".to_string(),
            },
        ];
        let err = expect_row_filter(recognize_request(
            "List rows where Annual Income < 5",
            &cols,
        ))
        .unwrap_err();
        match err {
            IntentError::ParseAmbiguous { ref candidates, .. } => {
                assert_eq!(
                    candidates.as_slice(),
                    ["Annual Income".to_string(), "Annual".to_string()]
                );
            }
            other => panic!("expected ParseAmbiguous, got {other:?}"),
        }
        assert_eq!(refusal_reason_of(&err), "intent.parse_ambiguous");
    }

    #[test]
    fn signed_and_fractional_ordered_literals_are_decimal() {
        let cols = job_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where Annual Income >= -2.5",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::Compare {
                column: "column-1".to_string(),
                operator: ComparisonOperator::GreaterOrEqual,
                literal: Literal::Decimal(dec("-2.5")),
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(
            row_filter.literals[0].parser_policy,
            Some(NumericParsePolicy::StrictDecimal)
        );
    }

    #[test]
    fn quoted_text_literal_can_contain_connectors() {
        let cols = job_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where Job = \"unemployed or retired\"",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::Compare {
                column: "column-0".to_string(),
                operator: ComparisonOperator::Equal,
                literal: Literal::Text("unemployed or retired".to_string()),
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(row_filter.literals[0].raw_text, "unemployed or retired");
        assert_eq!(row_filter.literals[0].parser_policy, None);
    }

    #[test]
    fn ordered_comparison_with_text_literal_compiles_but_fails_type_validation() {
        let cols = job_columns();
        let request = recognize_request("List rows where Annual Income < retirement", &cols)
            .unwrap_or_else(|e| panic!("expected a row filter, got {e:?}"));
        let intent = expect_row_filter_ok(Ok(request.clone()));
        assert_eq!(
            intent.predicate,
            Expression::Compare {
                column: "column-1".to_string(),
                operator: ComparisonOperator::Less,
                literal: Literal::Text("retirement".to_string()),
            }
        );
        let plan = compile_request_to_plan(&request, "rev-1", "table-0");
        let err = validate_plan_structure(&plan).unwrap_err();
        assert_eq!(err.diagnostic_code(), "plan.type_mismatch");
    }

    #[test]
    fn excessive_predicate_depth_refuses_predicate_unsupported() {
        let cols = job_columns();
        let mut prompt = String::from("List rows where ");
        for _ in 0..9 {
            prompt.push_str("not ( ");
        }
        prompt.push_str("job = 1");
        for _ in 0..9 {
            prompt.push_str(" )");
        }
        let err = expect_row_filter(recognize_request(&prompt, &cols)).unwrap_err();
        assert!(matches!(err, IntentError::PredicateUnsupported(_, _)));
        assert_eq!(refusal_reason_of(&err), "intent.predicate_unsupported");
    }

    #[test]
    fn excessive_predicate_node_count_refuses_predicate_unsupported() {
        let cols = job_columns();
        let chain = (0..64).map(|_| "job = 1").collect::<Vec<_>>().join(" and ");
        let prompt = format!("List rows where {chain}");
        let err = expect_row_filter(recognize_request(&prompt, &cols)).unwrap_err();
        assert!(matches!(err, IntentError::PredicateUnsupported(_, _)));
    }

    #[test]
    fn epic_002_prompts_keep_their_meaning_through_request_recognition() {
        let income_cols = income_time_columns();
        let request = recognize_request("List income", &income_cols).unwrap();
        let retrieval = match request {
            RecognizedRequest::Retrieval(intent) => intent,
            other => panic!("expected retrieval, got {other:?}"),
        };
        assert_eq!(retrieval.operation, CanonicalOperation::Select);
        assert_eq!(retrieval.column_id, "col-income");
        assert!(retrieval.evidence.row_filter.is_none());
        assert_eq!(
            retrieval.evidence.canonical_operation.as_deref(),
            Some("select")
        );
        let plan = compile_intent_to_plan(&retrieval, "rev-1", "table-0");
        assert_eq!(plan.schema_version, PLAN_SCHEMA_VERSION_1);
        assert_eq!(plan.steps.len(), 1);

        let plan_cols = columns();
        let request = recognize_request("Extract all the unique floor plans", &plan_cols).unwrap();
        let retrieval = match request {
            RecognizedRequest::Retrieval(intent) => intent,
            other => panic!("expected retrieval, got {other:?}"),
        };
        assert_eq!(retrieval.operation, CanonicalOperation::Distinct);
        assert_eq!(retrieval.column_id, "column-1");
        let plan = compile_intent_to_plan(&retrieval, "rev-1", "table-0");
        assert_eq!(plan.schema_version, PLAN_SCHEMA_VERSION_1);
        assert_eq!(plan.steps.len(), 3);

        let plan_cols = columns();
        let err = recognize_request("do something random", &plan_cols).unwrap_err();
        assert!(matches!(err, IntentError::Unsupported(_, _)));
    }

    #[test]
    fn epic_002_ambiguities_are_not_reinterpreted_as_predicates() {
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
        let err = recognize_request("list unique income", &cols).unwrap_err();
        assert!(matches!(err, IntentError::ParseAmbiguous { .. }));

        let dup_cols = vec![ColumnDefinition {
            id: "col-income".to_string(),
            ordinal: 0,
            source_header_raw: Some("Income".to_string()),
            source_header_normalized: Some("income".to_string()),
            display_name: "Income".to_string(),
        }];
        let request = recognize_request("List incomes", &dup_cols).unwrap();
        let RecognizedRequest::Retrieval(retrieval) = request else {
            panic!("expected retrieval")
        };
        assert_eq!(retrieval.operation, CanonicalOperation::Select);
    }

    #[test]
    fn remaining_comparison_operators_recognize() {
        let cols = predicate_columns();
        for (symbol, expected) in [
            ("!=", ComparisonOperator::NotEqual),
            ("<=", ComparisonOperator::LessOrEqual),
            (">", ComparisonOperator::Greater),
            (">=", ComparisonOperator::GreaterOrEqual),
        ] {
            let intent = expect_row_filter_ok(recognize_request(
                &format!("List rows where job {symbol} 2"),
                &cols,
            ));
            assert_eq!(
                intent.predicate,
                Expression::Compare {
                    column: "column-0".to_string(),
                    operator: expected,
                    literal: Literal::Decimal(dec("2")),
                },
                "symbol {symbol}"
            );
            let row_filter = intent.evidence.row_filter.as_ref().unwrap();
            assert_eq!(
                row_filter.operators[0].operator, expected,
                "symbol {symbol}"
            );
            assert_eq!(row_filter.operators[0].span, (4, 5), "symbol {symbol}");
        }
    }

    #[test]
    fn bare_row_action_keywords_refuse_predicate_unsupported() {
        let cols = job_columns();
        for prompt in ["List rows", "List rows where", "Show rows where"] {
            let err = expect_row_filter(recognize_request(prompt, &cols)).unwrap_err();
            assert!(
                matches!(err, IntentError::PredicateUnsupported(_, _)),
                "prompt {prompt}, got {err:?}"
            );
            assert_eq!(
                refusal_reason_of(&err),
                "intent.predicate_unsupported",
                "prompt {prompt}"
            );
        }
    }

    #[test]
    fn unquoted_text_literal_extends_to_the_next_connector() {
        let cols = job_columns();
        let intent = expect_row_filter_ok(recognize_request(
            "List rows where job = x extra or job = y",
            &cols,
        ));
        assert_eq!(
            intent.predicate,
            Expression::Or {
                predicates: vec![
                    Expression::Compare {
                        column: "column-0".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Text("x extra".to_string()),
                    },
                    Expression::Compare {
                        column: "column-0".to_string(),
                        operator: ComparisonOperator::Equal,
                        literal: Literal::Text("y".to_string()),
                    },
                ],
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(row_filter.literals[0].raw_text, "x extra");
        assert_eq!(row_filter.literals[0].span, (5, 7));
        assert_eq!(row_filter.connectors[0].span, (7, 8));
    }

    #[test]
    fn unmatched_closing_paren_refuses_predicate_unsupported() {
        let cols = predicate_columns();
        let err = expect_row_filter(recognize_request(
            "List rows where job = 1 ) and status = 2",
            &cols,
        ))
        .unwrap_err();
        assert!(matches!(err, IntentError::PredicateUnsupported(_, _)));
        assert_eq!(refusal_reason_of(&err), "intent.predicate_unsupported");
    }

    #[test]
    fn compact_quoted_implicit_equality_literal() {
        let cols = job_columns();
        let intent = expect_row_filter_ok(recognize_request("List job \"retired worker\"", &cols));
        assert_eq!(
            intent.predicate,
            Expression::Compare {
                column: "column-0".to_string(),
                operator: ComparisonOperator::Equal,
                literal: Literal::Text("retired worker".to_string()),
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(row_filter.literals[0].raw_text, "retired worker");
        assert_eq!(row_filter.literals[0].parser_policy, None);
        assert_eq!(row_filter.literals[0].span, (2, 3));
    }

    #[test]
    fn compact_numeric_literal_stays_text_like_the_equality_sugar() {
        let cols = job_columns();
        let intent = expect_row_filter_ok(recognize_request("List job 5", &cols));
        assert_eq!(
            intent.predicate,
            Expression::Compare {
                column: "column-0".to_string(),
                operator: ComparisonOperator::Equal,
                literal: Literal::Text("5".to_string()),
            }
        );
        let row_filter = intent.evidence.row_filter.as_ref().unwrap();
        assert_eq!(row_filter.literals[0].parser_policy, None);
    }

    #[test]
    fn row_filter_prompt_tokens_preserve_index_and_lowercase() {
        let cols = job_columns();
        let intent =
            expect_row_filter_ok(recognize_request("List rows where Job = unemployed", &cols));
        let tokens: Vec<(usize, &str)> = intent
            .evidence
            .prompt_tokens
            .iter()
            .map(|token| (token.index, token.text.as_str()))
            .collect();
        assert_eq!(
            tokens,
            vec![
                (0, "list"),
                (1, "rows"),
                (2, "where"),
                (3, "job"),
                (4, "="),
                (5, "unemployed"),
            ]
        );
    }
}
