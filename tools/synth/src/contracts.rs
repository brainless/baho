//! Versioned serde contracts shared by `synth evaluate` and `synth plan-epic` (Epic 016).
//!
//! Every persisted structure carries a `schema_version`; readers reject versions they do
//! not know. Reports never hold source-derived row lists, only counts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::redaction::MappingLimitation;
use thiserror::Error;

pub const REPORT_SCHEMA_VERSION: u32 = 1;
pub const REVIEW_SCHEMA_VERSION: u32 = 1;
pub const SIDECAR_SCHEMA_VERSION: u32 = 1;
pub const MAX_RATIONALE_CHARS: usize = 500;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("unsupported {what} schema version {found} (expected {expected})")]
    Version {
        what: &'static str,
        found: u32,
        expected: u32,
    },
    #[error("invalid {what}: {reason}")]
    Invalid { what: &'static str, reason: String },
}

/// Independent assessment 1: human review of the prompt/SQL pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    #[default]
    Unreviewed,
    Confirmed,
    Ambiguous,
    Rejected,
}

/// Independent assessment 2: what Baho did, compared on this dataset only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedResult {
    RowsMatch,
    WrongRows,
    Refused,
    Clarification,
    Error,
    OracleError,
}

/// Independent assessment 3: structured plan versus the SQL oracle's predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanComparison {
    Equivalent,
    Different,
    Unavailable,
}

/// Row-count facts only; row lists are never persisted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowCounts {
    pub expected: usize,
    pub actual: usize,
    pub missing: usize,
    pub extra: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseReport {
    pub case_id: String,
    pub candidate_hash: String,
    pub prompt: String,
    pub review: ReviewStatus,
    pub review_rationale: Option<String>,
    pub observed: ObservedResult,
    pub plan_comparison: PlanComparison,
    pub diagnostic_codes: Vec<String>,
    pub row_counts: Option<RowCounts>,
    pub evidence: CaseEvidence,
}

/// Lexical hints computed by the harness from the raw prompt. Hints, not feature labels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LexicalFacts {
    pub words: usize,
    pub chars: usize,
    pub has_quote: bool,
    pub ends_with_punctuation: bool,
}

/// Bounded, sanitized supporting evidence for one case. Never holds row lists.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseEvidence {
    pub lexical: LexicalFacts,
    /// Volatile: the CLI run ID in the private evaluation directory.
    pub run_id: Option<String>,
    /// Stable oracle failure code when the case was not replayed.
    pub oracle_code: Option<String>,
    pub refusal_reason: Option<String>,
    /// Stable code (`spawn_*`) when the CLI process could not be started. Optional and
    /// serde-default, so reports written before it existed still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_failure: Option<String>,
    pub diagnostic_messages: Vec<String>,
    pub plan: Option<PlanEvidence>,
}

/// Derived evidence eligibility, independent of aggregate counts or model assertions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceEligibility {
    Observation,
    ConfirmedWrongPlan,
    ConfirmedRefusal,
}

impl EvaluationReport {
    pub fn evidence_eligibility(&self, case: &CaseReport) -> EvidenceEligibility {
        // Altered text cannot establish semantics for the affected case. Unrelated
        // stand-ins do not disqualify the rest of the batch.
        let prompt = case.prompt.to_lowercase();
        if self
            .limitations
            .iter()
            .any(|l| prompt.contains(&l.stand_in.to_lowercase()))
            || case.review != ReviewStatus::Confirmed
            || case.evidence.spawn_failure.is_some()
            || case.evidence.oracle_code.is_some()
        {
            return EvidenceEligibility::Observation;
        }
        if case.is_confirmed_wrong_plan() && case.evidence.plan.is_some() {
            EvidenceEligibility::ConfirmedWrongPlan
        } else if matches!(case.observed, ObservedResult::Refused)
            && (case.evidence.refusal_reason.is_some() || !case.diagnostic_codes.is_empty())
        {
            EvidenceEligibility::ConfirmedRefusal
        } else {
            EvidenceEligibility::Observation
        }
    }
}

impl CaseReport {
    /// Requires all three independent conditions of the evidence contract.
    pub fn is_confirmed_wrong_plan(&self) -> bool {
        self.review == ReviewStatus::Confirmed
            && matches!(
                self.observed,
                ObservedResult::RowsMatch | ObservedResult::WrongRows
            )
            && self.plan_comparison == PlanComparison::Different
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub schema_version: u32,
    /// Volatile metadata, excluded from deterministic comparisons.
    pub evaluation_id: String,
    pub generation_id: String,
    pub generation_hash: String,
    pub candidates_hash: String,
    pub metadata_hash: String,
    pub dataset_hash: String,
    pub provenance: ToolProvenance,
    pub aggregates: Aggregates,
    /// What the neutral mapping cannot preserve; conclusions from those stand-ins are unsupported.
    pub limitations: Vec<MappingLimitation>,
    pub notes: Vec<String>,
    pub cases: Vec<CaseReport>,
}

/// A revision does not prove the binary was built from it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProvenance {
    pub binary_sha256: String,
    pub binary_version: String,
    pub repo_revision: String,
    pub repo_dirty: bool,
}

/// Counts for this candidate batch only; kept separate per assessment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregates {
    pub cases: usize,
    pub observed: BTreeMap<ObservedResult, usize>,
    pub review: BTreeMap<ReviewStatus, usize>,
    pub plan_comparison: BTreeMap<PlanComparison, usize>,
    pub confirmed_wrong_plan: usize,
    /// Cases where Baho materialized rows, the denominator of the agreement rate.
    pub materialized: usize,
    pub rows_match: usize,
}

impl EvaluationReport {
    pub fn check_version(&self) -> Result<(), ContractError> {
        check_version(
            "evaluation report",
            self.schema_version,
            REPORT_SCHEMA_VERSION,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Confirmed,
    Ambiguous,
    Rejected,
}

impl From<ReviewDecision> for ReviewStatus {
    fn from(value: ReviewDecision) -> Self {
        match value {
            ReviewDecision::Confirmed => Self::Confirmed,
            ReviewDecision::Ambiguous => Self::Ambiguous,
            ReviewDecision::Rejected => Self::Rejected,
        }
    }
}

/// One human decision, bound to a case and the exact candidate content reviewed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewEntry {
    pub case_id: String,
    pub candidate_hash: String,
    pub decision: ReviewDecision,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    pub schema_version: u32,
    pub generation_id: String,
    pub decisions: Vec<ReviewEntry>,
}

impl ReviewFile {
    pub fn parse(bytes: &[u8]) -> Result<Self, ContractError> {
        let file: Self = serde_json::from_slice(bytes).map_err(|e| ContractError::Invalid {
            what: "review file",
            reason: e.to_string(),
        })?;
        check_version("review file", file.schema_version, REVIEW_SCHEMA_VERSION)?;
        if file
            .decisions
            .iter()
            .any(|d| d.rationale.chars().count() > MAX_RATIONALE_CHARS)
        {
            return Err(ContractError::Invalid {
                what: "review file",
                reason: "rationale exceeds bound".into(),
            });
        }
        Ok(file)
    }

    /// Fails unless the file is bound to this generation and every decision matches a
    /// case with identical candidate content. Returns decisions keyed by case order.
    pub fn bind<'a>(
        &'a self,
        generation_id: &str,
        cases: impl Iterator<Item = (&'a str, &'a str)> + Clone,
    ) -> Result<(), ContractError> {
        if self.generation_id != generation_id {
            return Err(ContractError::Invalid {
                what: "review file",
                reason: "bound to a different generation".into(),
            });
        }
        let mut seen = std::collections::HashSet::new();
        for entry in &self.decisions {
            if !seen.insert(entry.case_id.as_str()) {
                return Err(ContractError::Invalid {
                    what: "review file",
                    reason: "duplicate decision for a case".into(),
                });
            }
            let hash = cases
                .clone()
                .find(|(id, _)| *id == entry.case_id)
                .map(|(_, hash)| hash)
                .ok_or_else(|| ContractError::Invalid {
                    what: "review file",
                    reason: "decision for unknown case".into(),
                })?;
            if hash != entry.candidate_hash {
                return Err(ContractError::Invalid {
                    what: "review file",
                    reason: "stale candidate hash".into(),
                });
            }
        }
        Ok(())
    }
}

/// Private neutral mapping (source literal to stand-in). Never reachable by agent tools.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionSidecar {
    pub schema_version: u32,
    pub evaluation_hash: String,
    pub entries: Vec<SidecarEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarEntry {
    pub source: String,
    pub stand_in: String,
}

impl RedactionSidecar {
    pub fn parse_for(bytes: &[u8], evaluation_hash: &str) -> Result<Self, ContractError> {
        let sidecar: Self = serde_json::from_slice(bytes).map_err(|e| ContractError::Invalid {
            what: "redaction sidecar",
            reason: e.to_string(),
        })?;
        check_version(
            "redaction sidecar",
            sidecar.schema_version,
            SIDECAR_SCHEMA_VERSION,
        )?;
        if sidecar.evaluation_hash != evaluation_hash {
            return Err(ContractError::Invalid {
                what: "redaction sidecar",
                reason: "does not match the evaluation hash".into(),
            });
        }
        Ok(sidecar)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanOperator {
    Equal,
    NotEqual,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMatchPolicy {
    UnicodeLowercase,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum PlanLiteral {
    Text(String),
    Other,
}

/// The structured Baho plan fields this harness can compare (single equality).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEvidence {
    pub column: String,
    pub operator: PlanOperator,
    pub literal: PlanLiteral,
    pub text_match_policy: TextMatchPolicy,
}

/// Compares the oracle's `lower("Role") = lower('<literal>')` with structured plan evidence.
///
/// Text uses full Unicode lowercase equality (`TextMatchPolicy::UnicodeLowercase`), so
/// spelling of case does not matter, but outer whitespace does. Absent evidence, an
/// unknown policy, or a non-text literal is `Unavailable`, never inferred from rows.
pub fn compare_plan(
    oracle_column: &str,
    oracle_literal: &str,
    evidence: Option<&PlanEvidence>,
) -> PlanComparison {
    let Some(plan) = evidence else {
        return PlanComparison::Unavailable;
    };
    if plan.text_match_policy != TextMatchPolicy::UnicodeLowercase {
        return PlanComparison::Unavailable;
    }
    let PlanLiteral::Text(literal) = &plan.literal else {
        return PlanComparison::Unavailable;
    };
    if plan.operator == PlanOperator::Other {
        return PlanComparison::Unavailable;
    }
    let same = plan.column == oracle_column
        && plan.operator == PlanOperator::Equal
        && literal.to_lowercase() == oracle_literal.to_lowercase();
    if same {
        PlanComparison::Equivalent
    } else {
        PlanComparison::Different
    }
}

fn check_version(what: &'static str, found: u32, expected: u32) -> Result<(), ContractError> {
    if found == expected {
        Ok(())
    } else {
        Err(ContractError::Version {
            what,
            found,
            expected,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(op: PlanOperator, literal: &str) -> PlanEvidence {
        PlanEvidence {
            column: "Role".into(),
            operator: op,
            literal: PlanLiteral::Text(literal.into()),
            text_match_policy: TextMatchPolicy::UnicodeLowercase,
        }
    }

    #[test]
    fn plan_comparison_cases() {
        let eq = |p: Option<&PlanEvidence>| compare_plan("Role", "Analyst", p);
        assert_eq!(
            eq(Some(&plan(PlanOperator::Equal, "ANALYST"))),
            PlanComparison::Equivalent
        );
        assert_eq!(
            eq(Some(&plan(PlanOperator::Equal, "'Analyst'"))),
            PlanComparison::Different
        );
        assert_eq!(
            eq(Some(&plan(PlanOperator::Equal, " Analyst"))),
            PlanComparison::Different
        );
        assert_eq!(
            eq(Some(&plan(PlanOperator::NotEqual, "Analyst"))),
            PlanComparison::Different
        );
        assert_eq!(eq(None), PlanComparison::Unavailable);
        let mut other = plan(PlanOperator::Equal, "Analyst");
        other.text_match_policy = TextMatchPolicy::Other;
        assert_eq!(eq(Some(&other)), PlanComparison::Unavailable);
        let mut wrong_column = plan(PlanOperator::Equal, "Analyst");
        wrong_column.column = "Name".into();
        assert_eq!(eq(Some(&wrong_column)), PlanComparison::Different);
    }

    fn case(review: ReviewStatus, observed: ObservedResult, plan: PlanComparison) -> CaseReport {
        CaseReport {
            case_id: "c".into(),
            candidate_hash: "h".into(),
            prompt: "p".into(),
            review,
            review_rationale: None,
            observed,
            plan_comparison: plan,
            diagnostic_codes: vec![],
            row_counts: None,
            evidence: CaseEvidence::default(),
        }
    }

    #[test]
    fn confirmed_wrong_plan_needs_all_three_conditions() {
        use ObservedResult::*;
        use PlanComparison::*;
        use ReviewStatus::*;
        assert!(case(Confirmed, RowsMatch, Different).is_confirmed_wrong_plan());
        assert!(case(Confirmed, WrongRows, Different).is_confirmed_wrong_plan());
        assert!(!case(Unreviewed, WrongRows, Different).is_confirmed_wrong_plan());
        assert!(!case(Confirmed, Refused, Different).is_confirmed_wrong_plan());
        assert!(!case(Confirmed, RowsMatch, Equivalent).is_confirmed_wrong_plan());
        assert!(!case(Confirmed, WrongRows, Unavailable).is_confirmed_wrong_plan());
        assert_eq!(ReviewStatus::default(), Unreviewed);
    }

    #[test]
    fn review_file_versions_bounds_and_binding() {
        let good = br#"{"schema_version":1,"generation_id":"g","decisions":[{"case_id":"g-1","candidate_hash":"abc","decision":"confirmed","rationale":"ok"}]}"#;
        let file = ReviewFile::parse(good).unwrap();
        let cases = [("g-1", "abc"), ("g-2", "def")];
        assert!(file.bind("g", cases.iter().copied()).is_ok());
        assert!(file.bind("other", cases.iter().copied()).is_err());
        let stale = [("g-1", "zzz")];
        assert!(file.bind("g", stale.iter().copied()).is_err());
        let unknown = [("g-9", "abc")];
        assert!(file.bind("g", unknown.iter().copied()).is_err());
        assert!(
            ReviewFile::parse(br#"{"schema_version":2,"generation_id":"g","decisions":[]}"#)
                .is_err()
        );
        let long = format!(
            r#"{{"schema_version":1,"generation_id":"g","decisions":[{{"case_id":"a","candidate_hash":"b","decision":"rejected","rationale":"{}"}}]}}"#,
            "x".repeat(501)
        );
        assert!(ReviewFile::parse(long.as_bytes()).is_err());
        let dup = br#"{"schema_version":1,"generation_id":"g","decisions":[{"case_id":"g-1","candidate_hash":"abc","decision":"confirmed"},{"case_id":"g-1","candidate_hash":"abc","decision":"rejected"}]}"#;
        assert!(
            ReviewFile::parse(dup)
                .unwrap()
                .bind("g", cases.iter().copied())
                .is_err()
        );
    }

    #[test]
    fn sidecar_must_match_evaluation_hash() {
        let bytes = br#"{"schema_version":1,"evaluation_hash":"e1","entries":[{"source":"a","stand_in":"b"}]}"#;
        assert!(RedactionSidecar::parse_for(bytes, "e1").is_ok());
        assert!(RedactionSidecar::parse_for(bytes, "e2").is_err());
        assert!(RedactionSidecar::parse_for(b"{}", "e1").is_err());
    }
}
