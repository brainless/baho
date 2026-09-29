//! Source-bound grounding and clarification contracts.
//!
//! These types describe an interpretation before plan compilation. Candidate
//! IDs identify predicate interpretations, since two interpretations may use
//! the same column with different predicate forms.

use baho_ingest::{
    RowReadLimits, SelectedRow, SelectedSourceReadError, SelectedSourceReader, SourceCellValue,
};
use baho_ingest_csv::{
    CompleteScanError, ExactValueLookup, FlagShapeLookup, SelectedRegionError,
    scan_grounding_evidence,
};
use baho_model::column::{ColumnDefinition, InferredColumnType, NumericParsePolicy};
use baho_model::{CellAddress, SourceRevision, TextMatchPolicy};
use baho_plan::{ComparisonOperator, Expression, Literal};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

use crate::intent::{RowFilterIntent, UngroundedKind};
use crate::orchestration::OpenedTable;

struct SelectedSourceRecords {
    reader: baho_ingest_csv::CsvSelectedSourceReader,
    rows: VecDeque<SelectedRow>,
    batch_complete: bool,
    max_bytes: usize,
}

impl SelectedSourceRecords {
    fn new(reader: baho_ingest_csv::CsvSelectedSourceReader, max_field_size: usize) -> Self {
        let columns = reader.metadata().columns.len();
        Self {
            reader,
            rows: VecDeque::new(),
            batch_complete: false,
            max_bytes: max_field_size.saturating_mul(columns).max(1),
        }
    }
}

impl Iterator for SelectedSourceRecords {
    type Item = Result<baho_ingest_csv::inspector::LogicalRecord, SelectedRegionError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(row) = self.rows.pop_front() {
                let fields = row
                    .cells
                    .into_iter()
                    .map(|cell| match cell {
                        SourceCellValue::Present(value) => value,
                        SourceCellValue::Missing => String::new(),
                    })
                    .collect();
                return Some(Ok(baho_ingest_csv::inspector::LogicalRecord {
                    index: row.source_row,
                    fields,
                    is_blank: false,
                }));
            }
            if self.batch_complete {
                return None;
            }
            let limits = match RowReadLimits::new(128, self.max_bytes) {
                Ok(limits) => limits,
                Err(error) => {
                    self.batch_complete = true;
                    return Some(Err(SelectedRegionError::Reader {
                        detail: error.to_string(),
                    }));
                }
            };
            match self.reader.read_next(limits) {
                Ok(batch) => {
                    self.rows.extend(batch.rows);
                    self.batch_complete = batch.complete;
                }
                Err(SelectedSourceReadError::RevisionChanged) => {
                    self.batch_complete = true;
                    return Some(Err(SelectedRegionError::RevisionChanged));
                }
                Err(SelectedSourceReadError::MalformedRecord { row, detail }) => {
                    self.batch_complete = true;
                    return Some(Err(SelectedRegionError::MalformedRecord { row, detail }));
                }
                Err(SelectedSourceReadError::FieldTooLarge {
                    row,
                    column,
                    size,
                    limit,
                }) => {
                    self.batch_complete = true;
                    return Some(Err(SelectedRegionError::FieldTooLarge {
                        row,
                        col: column,
                        size,
                        max_size: limit,
                    }));
                }
                Err(error) => {
                    self.batch_complete = true;
                    return Some(Err(SelectedRegionError::Reader {
                        detail: error.to_string(),
                    }));
                }
            }
        }
    }
}

/// First persisted grounding and clarification contract.
pub const GROUNDING_SCHEMA_VERSION: u32 = 2;
pub const CLARIFICATION_SCHEMA_VERSION: u32 = 1;

/// Complete result of attempting to ground a Boolean filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroundingResult {
    pub schema_version: u32,
    pub outcome: GroundingOutcome,
}

/// A partial predicate is never a successful grounding result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GroundingOutcome {
    Grounded {
        predicate: Expression,
        /// Source-bound decisions made before the predicate was compiled.
        evidence: Vec<GroundingClauseEvidence>,
    },
    NeedsClarification {
        request: ClarificationRequest,
    },
    Refused {
        code: GroundingRefusalCode,
        reason: String,
    },
}

/// Bounded evidence for one grounded clause. Counts are kept in
/// source column order, including zero-match columns, so absence is auditable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroundingClauseEvidence {
    pub clause_id: String,
    pub selected: InterpretationCandidate,
    pub selection: GroundingSelection,
    pub column_match_counts: Vec<ColumnMatchCount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroundingSelection {
    Automatic,
    UserSelected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMatchCount {
    pub column_id: String,
    pub match_count: u64,
}

/// Stable refusal categories. The human-readable reason may evolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroundingRefusalCode {
    UnsupportedGrammar,
    InvalidLiteral,
    IncompatibleTypes,
    ValueNotFound,
    ColumnHeaderAmbiguous,
    InvalidClarificationResponse,
    StaleClarification,
    ResourceLimitExceeded,
}

/// A set of unresolved clauses bound to one source and parser configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClarificationRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub source_revision: SourceRevision,
    pub table_id: String,
    /// Identity of the complete parser configuration used for this request.
    pub parser_config_identity: String,
    /// Identity of the exact prompt used for this request.
    pub prompt_identity: String,
    pub unresolved: Vec<ChooseInterpretation>,
}

/// One clause whose interpretation must be chosen before execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChooseInterpretation {
    pub clause_id: String,
    pub rendered_condition: String,
    pub reason: ClarificationReason,
    /// Ordered interpretations; ordering is constructed by the grounding step.
    pub candidates: Vec<InterpretationCandidate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClarificationReason {
    MultipleValueColumns,
    MultipleInterpretations,
    ComparisonColumnRequired,
}

/// One exact selectable interpretation, identified independently of column ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterpretationCandidate {
    pub candidate_id: String,
    pub column_id: String,
    pub display_name: String,
    pub predicate_form: PredicateForm,
    pub evidence: InterpretationEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateForm {
    EqualsValue,
    FlagIsTrue,
    IsNotBlank,
    Compare,
}

/// Bounded evidence about an interpretation. No observed source value is stored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InterpretationEvidence {
    /// Total matching cells, if a full value scan established it.
    pub match_count: Option<u64>,
    /// Bounded source locations for value matches, never an unbounded index.
    pub match_locations: Vec<CellAddress>,
    /// Whether a complete flag-shape scan classified this column as a flag.
    pub flag_shaped: Option<bool>,
    /// Whether schema/type evidence supports the comparison literal.
    pub type_compatible: Option<bool>,
}

/// Candidate construction requires complete evidence for every supplied
/// header and every value-search column. The caller decides which headers
/// match the complete prompt phrase under the recognition grammar.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CandidateEvidenceError {
    #[error("complete flag-shape evidence is missing for column {0}")]
    MissingFlagShape(String),
    #[error("complete value-lookup evidence is missing for column {0}")]
    MissingValueLookup(String),
}

/// Build interpretations of a bare predicate term. Header interpretations
/// precede value interpretations, and each group follows source column order.
/// Candidate IDs refer to interpretations, so one column may appear twice.
pub fn bare_term_candidates(
    columns: &[ColumnDefinition],
    matching_header_ids: &[String],
    flag_shapes: &FlagShapeLookup,
    values: &ExactValueLookup,
) -> Result<Vec<InterpretationCandidate>, CandidateEvidenceError> {
    let mut ordered = columns.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|column| column.ordinal);
    let mut candidates = Vec::new();
    for column in &ordered {
        if !matching_header_ids.iter().any(|id| id == &column.id) {
            continue;
        }
        let shape = flag_shapes
            .columns
            .iter()
            .find(|shape| shape.ordinal == column.ordinal)
            .ok_or_else(|| CandidateEvidenceError::MissingFlagShape(column.id.clone()))?;
        candidates.push(InterpretationCandidate {
            candidate_id: format!("candidate-{}", candidates.len() + 1),
            column_id: column.id.clone(),
            display_name: column.display_name.clone(),
            predicate_form: if shape.flag_shaped {
                PredicateForm::FlagIsTrue
            } else {
                PredicateForm::IsNotBlank
            },
            evidence: InterpretationEvidence {
                flag_shaped: Some(shape.flag_shaped),
                ..Default::default()
            },
        });
    }
    for column in &ordered {
        let found = values
            .columns
            .iter()
            .find(|found| found.ordinal == column.ordinal && found.column_id == column.id)
            .ok_or_else(|| CandidateEvidenceError::MissingValueLookup(column.id.clone()))?;
        if found.match_count == 0 {
            continue;
        }
        candidates.push(InterpretationCandidate {
            candidate_id: format!("candidate-{}", candidates.len() + 1),
            column_id: column.id.clone(),
            display_name: column.display_name.clone(),
            predicate_form: PredicateForm::EqualsValue,
            evidence: InterpretationEvidence {
                match_count: Some(found.match_count),
                match_locations: found.sample_cells.clone(),
                ..Default::default()
            },
        });
    }
    Ok(candidates)
}

/// Selected numeric parse evidence for an unbound ordered comparison.
pub struct ComparisonColumn<'a> {
    pub column: &'a ColumnDefinition,
    pub inferred_type: InferredColumnType,
    pub numeric_policy: Option<NumericParsePolicy>,
}

/// Offer only columns for which the ordered literal can be interpreted under
/// the selected numeric policy. No threshold-value search is performed.
pub fn ordered_comparison_candidates(
    columns: &[ComparisonColumn<'_>],
    operator: ComparisonOperator,
    literal: &Literal,
) -> Vec<InterpretationCandidate> {
    if !operator.is_ordered() {
        return Vec::new();
    }
    let mut ordered = columns.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|entry| entry.column.ordinal);
    ordered
        .into_iter()
        .filter(|entry| {
            entry.inferred_type == InferredColumnType::Numeric
                && match literal {
                    Literal::Decimal(_) => entry.numeric_policy.is_some(),
                    Literal::Deferred(raw) => entry
                        .numeric_policy
                        .is_some_and(|policy| policy.parse_decimal(raw).is_ok()),
                    Literal::Text(_) => false,
                }
        })
        .enumerate()
        .map(|(index, entry)| InterpretationCandidate {
            candidate_id: format!("candidate-{}", index + 1),
            column_id: entry.column.id.clone(),
            display_name: entry.column.display_name.clone(),
            predicate_form: PredicateForm::Compare,
            evidence: InterpretationEvidence {
                type_compatible: Some(true),
                ..Default::default()
            },
        })
        .collect()
}

/// User selections for every unresolved clause in a prior request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClarificationResponse {
    pub schema_version: u32,
    pub request_id: String,
    pub choices: Vec<ClarificationChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClarificationChoice {
    pub clause_id: String,
    pub selected_candidate_id: String,
}

fn identity(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn refusal(code: GroundingRefusalCode, reason: impl Into<String>) -> GroundingResult {
    GroundingResult {
        schema_version: GROUNDING_SCHEMA_VERSION,
        outcome: GroundingOutcome::Refused {
            code,
            reason: reason.into(),
        },
    }
}

fn candidate_predicate(
    candidate: &InterpretationCandidate,
    kind: &UngroundedKind,
    term: &str,
) -> Expression {
    match candidate.predicate_form {
        PredicateForm::EqualsValue => Expression::Compare {
            column: candidate.column_id.clone(),
            operator: ComparisonOperator::Equal,
            literal: Literal::Text(term.to_owned()),
        },
        PredicateForm::FlagIsTrue => Expression::Compare {
            column: candidate.column_id.clone(),
            operator: ComparisonOperator::Equal,
            literal: Literal::Text("true".into()),
        },
        PredicateForm::IsNotBlank => Expression::IsNotBlank {
            column: candidate.column_id.clone(),
        },
        PredicateForm::Compare => match kind {
            UngroundedKind::Comparison { operator, literal } => Expression::Compare {
                column: candidate.column_id.clone(),
                operator: *operator,
                literal: literal.clone(),
            },
            _ => unreachable!("comparison candidate has comparison clause"),
        },
    }
}

fn replace_clause(expression: &mut Expression, id: &str, replacement: &Expression) {
    let marker = format!("__grounding_{id}");
    match expression {
        Expression::Compare { column, .. } | Expression::IsNotBlank { column }
            if column == &marker =>
        {
            *expression = replacement.clone()
        }
        Expression::And { predicates } | Expression::Or { predicates } => {
            for predicate in predicates {
                replace_clause(predicate, id, replacement);
            }
        }
        Expression::Not { predicate } => replace_clause(predicate, id, replacement),
        _ => {}
    }
}

/// Recompute complete source-bound candidates and compile only after every
/// clause is grounded. A response must match the freshly reconstructed request.
pub fn ground_row_filter(
    opened: &OpenedTable,
    prompt: &str,
    intent: &mut RowFilterIntent,
    prior_request: Option<&ClarificationRequest>,
    response: Option<&ClarificationResponse>,
) -> GroundingResult {
    if intent.ungrounded.is_empty() {
        return if response.is_some() {
            refusal(
                GroundingRefusalCode::InvalidClarificationResponse,
                "no clarification is pending",
            )
        } else {
            GroundingResult {
                schema_version: GROUNDING_SCHEMA_VERSION,
                outcome: GroundingOutcome::Grounded {
                    predicate: intent.predicate.clone(),
                    evidence: Vec::new(),
                },
            }
        };
    }
    let columns = &opened.columns;
    let budget = opened
        .parser_config
        .evidence_limits
        .max_grounding_cells_scanned;
    let literals = intent
        .ungrounded
        .iter()
        .filter_map(|clause| {
            matches!(clause.kind, UngroundedKind::Bare { .. }).then_some(clause.term.as_str())
        })
        .collect::<Vec<_>>();
    let reader = match opened.selected_source_reader() {
        Ok(reader) => reader,
        Err(SelectedSourceReadError::RevisionChanged) => {
            return refusal(
                GroundingRefusalCode::StaleClarification,
                "source revision changed while grounding",
            );
        }
        Err(error) => return refusal(GroundingRefusalCode::IncompatibleTypes, error.to_string()),
    };
    let stream = SelectedSourceRecords::new(reader, opened.parser_config.inspection.max_field_size);
    let scanned = match scan_grounding_evidence(
        stream,
        columns,
        &literals,
        TextMatchPolicy::UnicodeLowercase,
        &opened.parser_config.normalization,
        budget,
        3,
    ) {
        Ok(scanned) => scanned,
        Err(CompleteScanError::ResourceLimitExceeded { .. }) => {
            return refusal(
                GroundingRefusalCode::ResourceLimitExceeded,
                "complete selected-table verification exceeded the grounding cell limit",
            );
        }
        Err(CompleteScanError::Source(SelectedRegionError::RevisionChanged)) => {
            return refusal(
                GroundingRefusalCode::StaleClarification,
                "source revision changed while grounding",
            );
        }
        Err(error) => return refusal(GroundingRefusalCode::IncompatibleTypes, error.to_string()),
    };
    let shapes = FlagShapeLookup {
        scan: scanned.scan.clone(),
        columns: scanned
            .columns
            .iter()
            .map(|column| baho_ingest_csv::ColumnFlagShape {
                ordinal: column.ordinal,
                nonblank_count: column.nonblank_count,
                flag_shaped: column.flag_shaped,
            })
            .collect(),
    };
    let mut unresolved = Vec::new();
    let mut substitutions = Vec::new();
    let mut automatic_evidence = Vec::new();
    for clause in &intent.ungrounded {
        let mut column_match_counts = Vec::new();
        let candidates = match &clause.kind {
            UngroundedKind::Bare { header_ids } => {
                let literal_index = literals
                    .iter()
                    .position(|literal| *literal == clause.term)
                    .expect("bare literal included in scan");
                let values = ExactValueLookup {
                    scan: scanned.scan.clone(),
                    columns: scanned
                        .columns
                        .iter()
                        .map(|column| column.matches[literal_index].clone())
                        .collect(),
                };
                column_match_counts = values
                    .columns
                    .iter()
                    .map(|column| ColumnMatchCount {
                        column_id: column.column_id.clone(),
                        match_count: column.match_count,
                    })
                    .collect();
                match bare_term_candidates(columns, header_ids, &shapes, &values) {
                    Ok(candidates) => candidates,
                    Err(error) => {
                        return refusal(GroundingRefusalCode::IncompatibleTypes, error.to_string());
                    }
                }
            }
            UngroundedKind::Comparison { operator, literal } => {
                let evidence = columns
                    .iter()
                    .zip(&scanned.columns)
                    .map(|(column, scanned)| ComparisonColumn {
                        column,
                        inferred_type: scanned.inferred_type,
                        numeric_policy: scanned.numeric_policy,
                    })
                    .collect::<Vec<_>>();
                ordered_comparison_candidates(&evidence, *operator, literal)
            }
        };
        if candidates.is_empty() {
            return refusal(
                match clause.kind {
                    UngroundedKind::Bare { .. } => GroundingRefusalCode::ValueNotFound,
                    _ => GroundingRefusalCode::IncompatibleTypes,
                },
                format!("no grounded interpretation for {}", clause.id),
            );
        }
        if candidates.len() == 1 {
            automatic_evidence.push(GroundingClauseEvidence {
                clause_id: clause.id.clone(),
                selected: candidates[0].clone(),
                selection: GroundingSelection::Automatic,
                column_match_counts,
            });
            substitutions.push((
                clause.id.clone(),
                candidate_predicate(&candidates[0], &clause.kind, &clause.term),
            ));
        } else {
            unresolved.push(ChooseInterpretation {
                clause_id: clause.id.clone(),
                rendered_condition: clause.term.clone(),
                reason: match clause.kind {
                    UngroundedKind::Bare { ref header_ids } if !header_ids.is_empty() => {
                        ClarificationReason::MultipleInterpretations
                    }
                    UngroundedKind::Bare { .. } => ClarificationReason::MultipleValueColumns,
                    _ => ClarificationReason::ComparisonColumnRequired,
                },
                candidates,
            });
        }
    }
    let config_bytes = serde_json::to_vec(&opened.parser_config).expect("parser config serializes");
    let config_identity = identity(&config_bytes);
    let prompt_identity = identity(prompt.as_bytes());
    let request_id = identity(
        format!(
            "{}:{}:{}:{}:{:?}",
            opened.source_revision.content_hash,
            opened.selected_candidate.id,
            config_identity,
            prompt_identity,
            unresolved
        )
        .as_bytes(),
    );
    let request = ClarificationRequest {
        schema_version: CLARIFICATION_SCHEMA_VERSION,
        request_id,
        source_revision: opened.source_revision.clone(),
        table_id: opened.selected_candidate.id.clone(),
        parser_config_identity: config_identity,
        prompt_identity,
        unresolved,
    };
    if let Some(response) = response {
        if prior_request != Some(&request) {
            return refusal(
                GroundingRefusalCode::StaleClarification,
                "source, table, parser configuration, prompt, clauses, or candidates changed",
            );
        }
        if response.schema_version != CLARIFICATION_SCHEMA_VERSION
            || response.request_id != request.request_id
            || response.choices.len() != request.unresolved.len()
        {
            return refusal(
                GroundingRefusalCode::InvalidClarificationResponse,
                "response does not match the complete clarification request",
            );
        }
        for clause in &request.unresolved {
            let matching = response
                .choices
                .iter()
                .filter(|choice| choice.clause_id == clause.clause_id)
                .collect::<Vec<_>>();
            if matching.len() != 1 {
                return refusal(
                    GroundingRefusalCode::InvalidClarificationResponse,
                    "each unresolved clause needs exactly one choice",
                );
            }
            let Some(candidate) = clause
                .candidates
                .iter()
                .find(|candidate| candidate.candidate_id == matching[0].selected_candidate_id)
            else {
                return refusal(
                    GroundingRefusalCode::InvalidClarificationResponse,
                    "selected candidate was not presented for its clause",
                );
            };
            let Some(original) = intent
                .ungrounded
                .iter()
                .find(|original| original.id == clause.clause_id)
            else {
                return refusal(
                    GroundingRefusalCode::InvalidClarificationResponse,
                    "clause is missing from recognized intent",
                );
            };
            substitutions.push((
                clause.clause_id.clone(),
                candidate_predicate(candidate, &original.kind, &original.term),
            ));
            automatic_evidence.push(GroundingClauseEvidence {
                clause_id: clause.clause_id.clone(),
                selected: candidate.clone(),
                selection: GroundingSelection::UserSelected,
                column_match_counts: Vec::new(),
            });
        }
    } else if !request.unresolved.is_empty() {
        return GroundingResult {
            schema_version: GROUNDING_SCHEMA_VERSION,
            outcome: GroundingOutcome::NeedsClarification { request },
        };
    }
    for (id, predicate) in substitutions {
        replace_clause(&mut intent.predicate, &id, &predicate);
    }
    if let Some(evidence) = &mut intent.evidence.row_filter {
        evidence.predicate = Some(intent.predicate.clone());
    }
    GroundingResult {
        schema_version: GROUNDING_SCHEMA_VERSION,
        outcome: GroundingOutcome::Grounded {
            predicate: intent.predicate.clone(),
            evidence: automatic_evidence,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_ingest_csv::config::NormalizationConfig;
    use baho_ingest_csv::inspector::LogicalRecord;
    use baho_ingest_csv::{LookupColumn, lookup_exact_value, scan_flag_shapes};
    use baho_model::TextMatchPolicy;

    fn column(ordinal: usize, name: &str) -> ColumnDefinition {
        ColumnDefinition {
            id: format!("column-{ordinal}"),
            ordinal,
            source_header_raw: Some(name.into()),
            source_header_normalized: Some(name.to_lowercase()),
            display_name: name.into(),
        }
    }

    fn rows() -> Vec<Result<LogicalRecord, ()>> {
        vec![
            Ok(LogicalRecord {
                index: 1,
                fields: vec!["active".into(), "true".into(), "other".into()],
                is_blank: false,
            }),
            Ok(LogicalRecord {
                index: 2,
                fields: vec!["other".into(), "FALSE".into(), "active".into()],
                is_blank: false,
            }),
        ]
    }

    #[test]
    fn bare_candidates_put_headers_before_values_in_source_order() {
        let columns = vec![column(2, "Active"), column(0, "Active"), column(1, "Flag")];
        let normalization = NormalizationConfig::default();
        let shapes = scan_flag_shapes(
            rows(),
            &[2, 0],
            TextMatchPolicy::UnicodeLowercase,
            &normalization,
            4,
        )
        .unwrap();
        let lookup_columns = columns
            .iter()
            .map(|column| LookupColumn {
                column_id: &column.id,
                ordinal: column.ordinal,
                parsed: None,
            })
            .collect::<Vec<_>>();
        let values = lookup_exact_value(
            rows(),
            &lookup_columns,
            "active",
            TextMatchPolicy::UnicodeLowercase,
            &normalization,
            6,
            1,
        )
        .unwrap();
        let candidates = bare_term_candidates(
            &columns,
            &["column-2".into(), "column-0".into()],
            &shapes,
            &values,
        )
        .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|c| (c.column_id.as_str(), c.predicate_form))
                .collect::<Vec<_>>(),
            vec![
                ("column-0", PredicateForm::IsNotBlank),
                ("column-2", PredicateForm::IsNotBlank),
                ("column-0", PredicateForm::EqualsValue),
                ("column-2", PredicateForm::EqualsValue),
            ]
        );
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.candidate_id.as_str())
                .collect::<Vec<_>>(),
            vec!["candidate-1", "candidate-2", "candidate-3", "candidate-4"]
        );
        assert_eq!(candidates[2].evidence.match_count, Some(1));
        assert_eq!(candidates[2].evidence.match_locations.len(), 1);
    }

    #[test]
    fn flag_header_uses_flag_predicate_and_value_candidate_can_share_column() {
        let columns = vec![column(1, "Flag")];
        let normalization = NormalizationConfig::default();
        let shapes = scan_flag_shapes(
            rows(),
            &[1],
            TextMatchPolicy::UnicodeLowercase,
            &normalization,
            2,
        )
        .unwrap();
        let values = lookup_exact_value(
            rows(),
            &[LookupColumn {
                column_id: "column-1",
                ordinal: 1,
                parsed: None,
            }],
            "true",
            TextMatchPolicy::UnicodeLowercase,
            &normalization,
            2,
            1,
        )
        .unwrap();
        let candidates =
            bare_term_candidates(&columns, &["column-1".into()], &shapes, &values).unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.predicate_form)
                .collect::<Vec<_>>(),
            vec![PredicateForm::FlagIsTrue, PredicateForm::EqualsValue]
        );
        assert_eq!(candidates[0].evidence.flag_shaped, Some(true));
    }

    #[test]
    fn ordered_comparison_offers_only_compatible_columns_in_source_order() {
        let columns = [column(2, "Age"), column(0, "Name"), column(1, "Income")];
        let specs = [
            ComparisonColumn {
                column: &columns[0],
                inferred_type: InferredColumnType::Numeric,
                numeric_policy: Some(NumericParsePolicy::StrictDecimal),
            },
            ComparisonColumn {
                column: &columns[1],
                inferred_type: InferredColumnType::Text,
                numeric_policy: None,
            },
            ComparisonColumn {
                column: &columns[2],
                inferred_type: InferredColumnType::Numeric,
                numeric_policy: Some(NumericParsePolicy::StrictDecimal),
            },
        ];
        let candidates = ordered_comparison_candidates(
            &specs,
            ComparisonOperator::Less,
            &Literal::Deferred("10000".into()),
        );
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.column_id.as_str())
                .collect::<Vec<_>>(),
            vec!["column-1", "column-2"]
        );
        assert!(
            candidates
                .iter()
                .all(|c| c.predicate_form == PredicateForm::Compare
                    && c.evidence.type_compatible == Some(true))
        );
        assert!(
            ordered_comparison_candidates(
                &specs,
                ComparisonOperator::Less,
                &Literal::Text("abc".into())
            )
            .is_empty()
        );
    }

    fn request() -> ClarificationRequest {
        ClarificationRequest {
            schema_version: CLARIFICATION_SCHEMA_VERSION,
            request_id: "request-1".into(),
            source_revision: SourceRevision {
                content_hash: "abc".into(),
                file_size: 42,
                modified_time: None,
            },
            table_id: "table-0".into(),
            parser_config_identity: "config-hash".into(),
            prompt_identity: "prompt-hash".into(),
            unresolved: vec![ChooseInterpretation {
                clause_id: "clause-1".into(),
                rendered_condition: "active".into(),
                reason: ClarificationReason::MultipleInterpretations,
                candidates: vec![
                    InterpretationCandidate {
                        candidate_id: "candidate-1".into(),
                        column_id: "column-0".into(),
                        display_name: "Active".into(),
                        predicate_form: PredicateForm::FlagIsTrue,
                        evidence: InterpretationEvidence {
                            flag_shaped: Some(true),
                            ..Default::default()
                        },
                    },
                    InterpretationCandidate {
                        candidate_id: "candidate-2".into(),
                        column_id: "column-0".into(),
                        display_name: "Active".into(),
                        predicate_form: PredicateForm::EqualsValue,
                        evidence: InterpretationEvidence {
                            match_count: Some(2),
                            ..Default::default()
                        },
                    },
                ],
            }],
        }
    }

    #[test]
    fn request_preserves_distinct_interpretations_for_one_column() {
        let original = request();
        let json = serde_json::to_value(&original).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["source_revision"]["content_hash"], "abc");
        assert_eq!(
            json["unresolved"][0]["candidates"][0]["predicate_form"],
            "flag_is_true"
        );
        assert_eq!(
            json["unresolved"][0]["candidates"][1]["predicate_form"],
            "equals_value"
        );
        assert_eq!(
            serde_json::from_value::<ClarificationRequest>(json).unwrap(),
            original
        );
    }

    #[test]
    fn response_uses_clause_and_candidate_ids_only() {
        let response = ClarificationResponse {
            schema_version: CLARIFICATION_SCHEMA_VERSION,
            request_id: "request-1".into(),
            choices: vec![ClarificationChoice {
                clause_id: "clause-1".into(),
                selected_candidate_id: "candidate-2".into(),
            }],
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(
            json["choices"][0],
            serde_json::json!({
                "clause_id": "clause-1", "selected_candidate_id": "candidate-2"
            })
        );
        assert_eq!(
            serde_json::from_value::<ClarificationResponse>(json).unwrap(),
            response
        );
    }

    #[test]
    fn grounding_outcomes_have_stable_status_tags() {
        let outcomes = [
            GroundingOutcome::Grounded {
                predicate: Expression::IsNotBlank {
                    column: "column-0".into(),
                },
                evidence: Vec::new(),
            },
            GroundingOutcome::NeedsClarification { request: request() },
            GroundingOutcome::Refused {
                code: GroundingRefusalCode::ResourceLimitExceeded,
                reason: "scan limit reached".into(),
            },
        ];
        for (outcome, tag) in
            outcomes
                .into_iter()
                .zip(["grounded", "needs_clarification", "refused"])
        {
            let original = GroundingResult {
                schema_version: GROUNDING_SCHEMA_VERSION,
                outcome,
            };
            let json = serde_json::to_value(&original).unwrap();
            assert_eq!(json["schema_version"], 2);
            assert_eq!(json["outcome"]["status"], tag);
            assert_eq!(
                serde_json::from_value::<GroundingResult>(json).unwrap(),
                original
            );
        }
    }
}
