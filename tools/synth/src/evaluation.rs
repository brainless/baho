//! Row comparison, outcome classification, and hash-bound review import (Epic 016, T3).
//!
//! Three assessments stay independent: the human review of the prompt/SQL pair, the
//! observed row agreement on this dataset, and the structured plan comparison. Only counts
//! leave this module, never row lists.

use std::collections::{BTreeSet, HashMap};

use crate::cli_run::{CaseRun, ParsedRun, RunOutcome};
use crate::contracts::{
    CaseEvidence, CaseReport, ContractError, ObservedResult, PlanComparison, ReviewFile,
    ReviewStatus, RowCounts, compare_plan,
};
use crate::intake::{IntakeCase, LoadedGeneration, OracleCheck};

/// Compares row sets (order and duplicates do not matter; both sides are source rows).
pub fn compare_rows(expected: &[u64], actual: &[u64]) -> RowCounts {
    let expected: BTreeSet<u64> = expected.iter().copied().collect();
    let actual: BTreeSet<u64> = actual.iter().copied().collect();
    RowCounts {
        expected: expected.len(),
        actual: actual.len(),
        missing: expected.difference(&actual).count(),
        extra: actual.difference(&expected).count(),
    }
}

impl RowCounts {
    pub fn agrees(&self) -> bool {
        self.missing == 0 && self.extra == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classification {
    pub observed: ObservedResult,
    pub plan_comparison: PlanComparison,
    pub diagnostic_codes: Vec<String>,
    pub row_counts: Option<RowCounts>,
}

/// Classifies one case. `run` and `parsed` are `None` when the case was not replayed or
/// its artifacts could not be read; an unverified oracle is never replayed.
pub fn classify(
    oracle: &OracleCheck,
    oracle_column: &str,
    expected_rows: &[u64],
    run: Option<&CaseRun>,
    parsed: Option<&ParsedRun>,
) -> Classification {
    let OracleCheck::Verified { literal } = oracle else {
        return Classification {
            observed: ObservedResult::OracleError,
            plan_comparison: PlanComparison::Unavailable,
            diagnostic_codes: Vec::new(),
            row_counts: None,
        };
    };
    let diagnostic_codes: Vec<String> = parsed
        .map(|p| {
            p.diagnostics
                .iter()
                .map(|d| d.code.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        })
        .unwrap_or_default();
    let plan_comparison = compare_plan(
        oracle_column,
        literal,
        parsed.and_then(|p| p.plan_evidence.as_ref()),
    );
    let error = |codes| Classification {
        observed: ObservedResult::Error,
        plan_comparison,
        diagnostic_codes: codes,
        row_counts: None,
    };
    let Some(parsed) = parsed else {
        return error(diagnostic_codes);
    };
    if run.is_some_and(|r| r.timed_out) {
        return error(diagnostic_codes);
    }
    let (observed, row_counts) = match (&parsed.outcome, &parsed.result_rows) {
        (RunOutcome::Materialized, Some(rows)) => {
            let counts = compare_rows(expected_rows, rows);
            let observed = if counts.agrees() {
                ObservedResult::RowsMatch
            } else {
                ObservedResult::WrongRows
            };
            (observed, Some(counts))
        }
        (RunOutcome::NeedsClarification, _) => (ObservedResult::Clarification, None),
        // A refusal is a recognized "cannot do this", not a crash.
        (RunOutcome::Materialized, None) => (ObservedResult::Error, None),
        _ if parsed.refusal_reason.is_some() => (ObservedResult::Refused, None),
        _ => (ObservedResult::Error, None),
    };
    Classification {
        observed,
        plan_comparison,
        diagnostic_codes,
        row_counts,
    }
}

/// Validates a review file against the loaded generation (generation ID, case IDs, and
/// candidate hashes) and returns decisions keyed by case ID. `None` means no reviews.
pub fn import_reviews(
    file: Option<&ReviewFile>,
    generation: &LoadedGeneration,
) -> Result<HashMap<String, (ReviewStatus, Option<String>)>, ContractError> {
    let Some(file) = file else {
        return Ok(HashMap::new());
    };
    file.bind(
        &generation.id,
        generation
            .cases
            .iter()
            .map(|c| (c.case_id.as_str(), c.candidate_hash.as_str())),
    )?;
    Ok(file
        .decisions
        .iter()
        .map(|d| {
            let rationale = (!d.rationale.is_empty()).then(|| d.rationale.clone());
            (d.case_id.clone(), (d.decision.into(), rationale))
        })
        .collect())
}

pub fn build_case_report(
    case: &IntakeCase,
    classification: Classification,
    reviews: &HashMap<String, (ReviewStatus, Option<String>)>,
) -> CaseReport {
    let (review, review_rationale) = reviews
        .get(&case.case_id)
        .cloned()
        .unwrap_or((ReviewStatus::Unreviewed, None));
    CaseReport {
        case_id: case.case_id.clone(),
        candidate_hash: case.candidate_hash.clone(),
        prompt: case.prompt.clone(),
        review,
        review_rationale,
        observed: classification.observed,
        plan_comparison: classification.plan_comparison,
        diagnostic_codes: classification.diagnostic_codes,
        row_counts: classification.row_counts,
        evidence: CaseEvidence::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_run::DiagnosticSummary;
    use crate::contracts::{PlanEvidence, PlanLiteral, PlanOperator, TextMatchPolicy};

    fn verified() -> OracleCheck {
        OracleCheck::Verified {
            literal: "Analyst".into(),
        }
    }

    fn evidence(literal: &str) -> PlanEvidence {
        PlanEvidence {
            column: "Role".into(),
            operator: PlanOperator::Equal,
            literal: PlanLiteral::Text(literal.into()),
            text_match_policy: TextMatchPolicy::UnicodeLowercase,
        }
    }

    fn parsed(outcome: RunOutcome, rows: Option<Vec<u64>>, ev: Option<PlanEvidence>) -> ParsedRun {
        ParsedRun {
            run_id: "000001".into(),
            outcome,
            diagnostics: vec![
                DiagnosticSummary {
                    code: "b.code".into(),
                    severity: "Info".into(),
                    message: String::new(),
                },
                DiagnosticSummary {
                    code: "a.code".into(),
                    severity: "Info".into(),
                    message: String::new(),
                },
                DiagnosticSummary {
                    code: "b.code".into(),
                    severity: "Info".into(),
                    message: String::new(),
                },
            ],
            plan_evidence: ev,
            refusal_reason: None,
            canonical_operation: None,
            result_rows: rows,
        }
    }

    #[test]
    fn row_counts() {
        let c = compare_rows(&[1, 2, 3], &[3, 4]);
        assert_eq!((c.expected, c.actual, c.missing, c.extra), (3, 2, 2, 1));
        assert!(!c.agrees());
        assert!(compare_rows(&[1, 2], &[2, 1, 1]).agrees());
        assert!(compare_rows(&[], &[]).agrees());
    }

    #[test]
    fn equal_rows_with_different_predicate_stay_rows_match_plus_different() {
        let p = parsed(
            RunOutcome::Materialized,
            Some(vec![1]),
            Some(evidence("Other")),
        );
        let c = classify(&verified(), "Role", &[1], None, Some(&p));
        assert_eq!(c.observed, ObservedResult::RowsMatch);
        assert_eq!(c.plan_comparison, PlanComparison::Different);
        assert_eq!(c.diagnostic_codes, ["a.code", "b.code"]);
    }

    #[test]
    fn empty_equivalent_result_is_not_a_wrong_plan() {
        let p = parsed(
            RunOutcome::Materialized,
            Some(vec![]),
            Some(evidence("ANALYST")),
        );
        let c = classify(&verified(), "Role", &[], None, Some(&p));
        assert_eq!(c.observed, ObservedResult::RowsMatch);
        assert_eq!(c.plan_comparison, PlanComparison::Equivalent);
    }

    #[test]
    fn absent_evidence_is_unavailable_and_wrong_rows_counted() {
        let p = parsed(RunOutcome::Materialized, Some(vec![2]), None);
        let c = classify(&verified(), "Role", &[1], None, Some(&p));
        assert_eq!(c.observed, ObservedResult::WrongRows);
        assert_eq!(c.plan_comparison, PlanComparison::Unavailable);
        assert_eq!(c.row_counts.unwrap().missing, 1);
    }

    #[test]
    fn non_materialized_outcomes() {
        let mut p = parsed(RunOutcome::Error, None, None);
        p.refusal_reason = Some("intent.column_not_found".into());
        assert_eq!(
            classify(&verified(), "Role", &[1], None, Some(&p)).observed,
            ObservedResult::Refused
        );
        let p = parsed(RunOutcome::Error, None, None);
        assert_eq!(
            classify(&verified(), "Role", &[1], None, Some(&p)).observed,
            ObservedResult::Error
        );
        let p = parsed(RunOutcome::NeedsClarification, None, None);
        assert_eq!(
            classify(&verified(), "Role", &[1], None, Some(&p)).observed,
            ObservedResult::Clarification
        );
        let p = parsed(RunOutcome::Materialized, None, None);
        assert_eq!(
            classify(&verified(), "Role", &[1], None, Some(&p)).observed,
            ObservedResult::Error
        );
        assert_eq!(
            classify(&verified(), "Role", &[1], None, None).observed,
            ObservedResult::Error
        );
        let timed_out = CaseRun {
            exit_code: None,
            timed_out: true,
            stdout_bytes: 0,
            stderr_bytes: 0,
            run_id: None,
            run_dir: None,
        };
        let p = parsed(RunOutcome::Materialized, Some(vec![1]), None);
        assert_eq!(
            classify(&verified(), "Role", &[1], Some(&timed_out), Some(&p)).observed,
            ObservedResult::Error
        );
    }

    #[test]
    fn unverified_oracle_is_not_run_against_baho() {
        let p = parsed(RunOutcome::Materialized, Some(vec![1]), None);
        for oracle in [
            OracleCheck::Mismatch {
                expected: 1,
                actual: 2,
            },
            OracleCheck::Error {
                code: "sql_grammar",
            },
        ] {
            let c = classify(&oracle, "Role", &[1], None, Some(&p));
            assert_eq!(c.observed, ObservedResult::OracleError);
            assert!(c.row_counts.is_none());
        }
    }

    fn generation() -> LoadedGeneration {
        let case = |id: &str, hash: &str| IntakeCase {
            case_id: id.into(),
            prompt: "p".into(),
            sql: "s".into(),
            candidate_hash: hash.into(),
            expected_rows: vec![],
        };
        LoadedGeneration {
            id: "g".into(),
            generation_hash: String::new(),
            candidates_hash: String::new(),
            metadata_hash: String::new(),
            dataset_hash: String::new(),
            cases: vec![case("g-1", "h1"), case("g-2", "h2")],
        }
    }

    #[test]
    fn review_import_is_hash_bound() {
        let gen_ = generation();
        assert!(import_reviews(None, &gen_).unwrap().is_empty());
        let ok = ReviewFile::parse(
            br#"{"schema_version":1,"generation_id":"g","decisions":[{"case_id":"g-1","candidate_hash":"h1","decision":"confirmed","rationale":"fine"}]}"#,
        )
        .unwrap();
        let map = import_reviews(Some(&ok), &gen_).unwrap();
        assert_eq!(
            map["g-1"],
            (ReviewStatus::Confirmed, Some("fine".to_owned()))
        );
        let report = build_case_report(
            &gen_.cases[1],
            classify(&verified(), "Role", &[], None, None),
            &map,
        );
        assert_eq!(report.review, ReviewStatus::Unreviewed);
        let stale = ReviewFile::parse(
            br#"{"schema_version":1,"generation_id":"g","decisions":[{"case_id":"g-1","candidate_hash":"old","decision":"rejected"}]}"#,
        )
        .unwrap();
        assert!(import_reviews(Some(&stale), &gen_).is_err());
        let other =
            ReviewFile::parse(br#"{"schema_version":1,"generation_id":"x","decisions":[]}"#)
                .unwrap();
        assert!(import_reviews(Some(&other), &gen_).is_err());
    }
}
