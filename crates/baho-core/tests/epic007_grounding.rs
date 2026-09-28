//! Synthetic end-to-end regressions for Epic 007 grounding.
//!
use std::io::{Seek, SeekFrom, Write};

use baho_core::{
    ClarificationChoice, ClarificationResponse, CoreOutcome, CoreResult, GroundingOutcome,
    execute_prompt, open_table, resolve_pipeline, run_pipeline,
};
use baho_plan::plan::{ComparisonOperator, Expression, Literal, PlanStep};

fn run(csv: &str, prompt: &str) -> CoreResult {
    let mut source = tempfile::NamedTempFile::new().unwrap();
    source.write_all(csv.as_bytes()).unwrap();
    run_pipeline(source.path(), prompt)
}

fn predicate(result: &CoreResult) -> &Expression {
    let plan = result.plan.as_ref().expect("expected a grounded plan");
    let [PlanStep::Filter { predicate }] = plan.steps.as_slice() else {
        panic!("expected one filter step, got {:?}", plan.steps);
    };
    predicate
}

fn retained_rows(result: &CoreResult) -> Vec<usize> {
    result
        .output
        .as_ref()
        .expect("expected materialized output")
        .provenance
        .iter()
        .map(|source| source.source_row)
        .collect()
}

fn assert_clarification_without_execution(result: &CoreResult) {
    assert!(result.plan.is_none(), "unresolved predicate was compiled");
    assert!(result.output.is_none(), "unresolved predicate was executed");
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "grounding.clarification_required"),
        "expected clarification diagnostic, got {:?}",
        result.diagnostics
    );
}

#[test]
fn unique_value_among_other_values_grounds_to_its_column() {
    let result = run(
        "ID,Job,City\n1,employed,Pune\n2,unemployed,Delhi\n3,student,Pune\n4,unemployed,Jaipur\n",
        "List rows where unemployed",
    );

    assert_eq!(result.outcome, CoreOutcome::Materialized);
    assert_eq!(
        predicate(&result),
        &Expression::Compare {
            column: "column-1".into(),
            operator: ComparisonOperator::Equal,
            literal: Literal::Text("unemployed".into()),
        }
    );
    assert_eq!(retained_rows(&result), [2, 4]);
    let GroundingOutcome::Grounded { evidence, .. } = &result.grounding.as_ref().unwrap().outcome
    else {
        panic!("expected grounded evidence");
    };
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].selected.column_id, "column-1");
    assert_eq!(evidence[0].column_match_counts.len(), 3);
    assert_eq!(
        evidence[0]
            .column_match_counts
            .iter()
            .map(|count| count.match_count)
            .collect::<Vec<_>>(),
        [0, 2, 0]
    );
    assert!(evidence[0].selected.evidence.match_locations.len() <= 3);
}

#[test]
fn flag_and_nonflag_headers_compile_to_distinct_predicates() {
    let csv = "ID,Active,Note\n1,True,ready\n2,FALSE,\n3,TRUE,later\n4,,ready\n";
    let flag = run(csv, "List rows where Active");
    assert_eq!(flag.outcome, CoreOutcome::Materialized);
    assert_eq!(
        predicate(&flag),
        &Expression::Compare {
            column: "column-1".into(),
            operator: ComparisonOperator::Equal,
            literal: Literal::Text("true".into()),
        }
    );
    assert_eq!(retained_rows(&flag), [1, 3]);

    let nonflag = run(csv, "List rows where Note");
    assert_eq!(nonflag.outcome, CoreOutcome::Materialized);
    assert_eq!(
        predicate(&nonflag),
        &Expression::IsNotBlank {
            column: "column-2".into(),
        }
    );
    assert_eq!(retained_rows(&nonflag), [1, 3, 4]);
}

#[test]
fn header_and_value_interpretations_require_clarification() {
    let result = run(
        "ID,Active,Status\n1,true,active\n2,false,inactive\n3,true,active\n",
        "List rows where active",
    );
    assert_clarification_without_execution(&result);
}

#[test]
fn blank_tests_include_ragged_and_whitespace_cells_but_not_malformed_raw_text() {
    let csv = "ID,Note,Amount\n1,,10\n2,   ,20\n3\n4,malformed,not-a-number\n5,ready,30\n";
    let blank = run(csv, "List rows where Note is blank");
    assert_eq!(
        predicate(&blank),
        &Expression::Not {
            predicate: Box::new(Expression::IsNotBlank {
                column: "column-1".into(),
            }),
        }
    );
    assert_eq!(retained_rows(&blank), [1, 2, 3]);

    let nonblank = run(csv, "List rows where Note is not blank");
    assert_eq!(
        predicate(&nonblank),
        &Expression::IsNotBlank {
            column: "column-1".into(),
        }
    );
    assert_eq!(retained_rows(&nonblank), [4, 5]);
}

#[test]
fn late_match_in_another_column_prevents_sample_based_uniqueness() {
    let mut csv = String::from("ID,Job,Note\n");
    for row in 1..=40 {
        csv.push_str(&format!("{row},unemployed,other\n"));
    }
    csv.push_str("41,employed,unemployed\n");

    let result = run(&csv, "List rows where unemployed");
    assert_clarification_without_execution(&result);
}

#[test]
fn late_match_missing_from_profile_still_grounds_after_complete_scan() {
    let mut csv = String::from("ID,Job,Note\n");
    for row in 1..=40 {
        csv.push_str(&format!("{row},employed,other\n"));
    }
    csv.push_str("41,employed,unemployed\n");

    let result = run(&csv, "List rows where unemployed");
    assert_eq!(result.outcome, CoreOutcome::Materialized);
    assert_eq!(
        predicate(&result),
        &Expression::Compare {
            column: "column-2".into(),
            operator: ComparisonOperator::Equal,
            literal: Literal::Text("unemployed".into()),
        }
    );
    assert_eq!(retained_rows(&result), [41]);
}

#[test]
fn response_binds_every_clause_and_rechecks_source_and_candidates() {
    let mut source = tempfile::NamedTempFile::new().unwrap();
    source.write_all(b"ID,Job,Note,Amount\n1,unemployed,other,5\n2,employed,unemployed,20\n3,employed,other,1\n").unwrap();
    let prompt = "List rows where unemployed and < 10";
    let pending = run_pipeline(source.path(), prompt);
    let request = match &pending.grounding.as_ref().unwrap().outcome {
        GroundingOutcome::NeedsClarification { request } => request.clone(),
        other => panic!("expected clarification, got {other:?}"),
    };
    assert_eq!(request.unresolved.len(), 2);
    assert_clarification_without_execution(&pending);
    let response = ClarificationResponse {
        schema_version: request.schema_version,
        request_id: request.request_id.clone(),
        choices: request
            .unresolved
            .iter()
            .map(|clause| ClarificationChoice {
                clause_id: clause.clause_id.clone(),
                selected_candidate_id: clause.candidates[0].candidate_id.clone(),
            })
            .collect(),
    };
    let completed = resolve_pipeline(source.path(), prompt, &request, &response);
    assert_eq!(
        completed.outcome,
        CoreOutcome::Materialized,
        "{:?}",
        completed.diagnostics
    );
    assert_eq!(retained_rows(&completed), [1]);

    let incomplete = ClarificationResponse {
        choices: response.choices[..1].to_vec(),
        ..response.clone()
    };
    let result = resolve_pipeline(source.path(), prompt, &request, &incomplete);
    assert_eq!(result.outcome, CoreOutcome::Failed);
    assert!(result.plan.is_none());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "grounding.invalid_clarification_response")
    );

    let wrong_candidate = ClarificationResponse {
        choices: vec![
            ClarificationChoice {
                clause_id: response.choices[0].clause_id.clone(),
                selected_candidate_id: "candidate-999".into(),
            },
            response.choices[1].clone(),
        ],
        ..response.clone()
    };
    let result = resolve_pipeline(source.path(), prompt, &request, &wrong_candidate);
    assert_eq!(result.outcome, CoreOutcome::Failed);

    source.as_file_mut().set_len(0).unwrap();
    source.seek(SeekFrom::Start(0)).unwrap();
    source.write_all(b"ID,Job,Note,Amount\n1,unemployed,other,5\n2,employed,unemployed,20\n3,employed,unemployed,1\n").unwrap();
    let stale = resolve_pipeline(source.path(), prompt, &request, &response);
    assert_eq!(stale.outcome, CoreOutcome::Failed);
    assert!(stale.plan.is_none());
}

#[test]
fn one_unbound_comparison_grounds_and_descriptive_term_does_not() {
    let comparison = run(
        "ID,Income,Job\n1,5,employed\n2,20,unemployed\n",
        "List rows where < 10",
    );
    assert_eq!(comparison.outcome, CoreOutcome::Recorded);
    assert_clarification_without_execution(&comparison);

    let unique = run(
        "Name,Income,Job\nA,5,employed\nB,20,unemployed\n",
        "List rows where < 10",
    );
    assert_eq!(
        unique.outcome,
        CoreOutcome::Materialized,
        "{:?}",
        unique.diagnostics
    );
    assert_eq!(retained_rows(&unique), [1]);

    let descriptive = run(
        "ID,Income,Job\n1,5,employed\n2,20,unemployed\n",
        "List rows where earning",
    );
    assert_eq!(descriptive.outcome, CoreOutcome::Failed);
    assert!(
        descriptive
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "grounding.value_not_found")
    );
}

#[test]
fn compact_boolean_with_bare_value_requires_complete_grounding() {
    let result = run(
        "ID,Job,Amount\n1,unemployed,20\n2,employed,5\n3,employed,20\n",
        "List unemployed or < 10",
    );
    assert!(matches!(
        result
            .grounding
            .as_ref()
            .map(|grounding| &grounding.outcome),
        Some(GroundingOutcome::NeedsClarification { .. })
    ));
    assert_clarification_without_execution(&result);
}

#[test]
fn grounding_cell_limit_counts_each_source_cell_once_across_clauses() {
    let mut source = tempfile::NamedTempFile::new().unwrap();
    source
        .write_all(b"Name,Job,Amount\nA,unemployed,5\nB,employed,20\n")
        .unwrap();
    let mut opened = open_table(source.path()).unwrap();
    let cells_per_lookup = (opened.rows.len() * opened.columns.len()) as u64;
    opened
        .parser_config
        .evidence_limits
        .max_grounding_cells_scanned = cells_per_lookup;

    let two_terms = execute_prompt(&opened, "List rows where unemployed or employed");
    assert_eq!(two_terms.outcome, CoreOutcome::Materialized);

    opened
        .parser_config
        .evidence_limits
        .max_grounding_cells_scanned = cells_per_lookup - 1;
    let over_limit = execute_prompt(&opened, "List rows where unemployed or employed");
    assert_eq!(over_limit.outcome, CoreOutcome::Failed);
    assert!(over_limit.plan.is_none());
    assert!(over_limit.output.is_none());
    assert!(
        over_limit
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "grounding.resource_limit_exceeded")
    );

    opened
        .parser_config
        .evidence_limits
        .max_grounding_cells_scanned = cells_per_lookup;
    let comparison = execute_prompt(&opened, "List rows where < 10");
    assert_eq!(comparison.outcome, CoreOutcome::Materialized);
    assert_eq!(retained_rows(&comparison), [1]);
}

#[test]
fn large_selected_table_grounding_retains_bounded_evidence() {
    let mut source = tempfile::NamedTempFile::new().unwrap();
    writeln!(source, "ID,Job,Amount").unwrap();
    for row in 1..=12_000 {
        writeln!(
            source,
            "{row},{},{}",
            if row == 11_999 {
                "unemployed"
            } else {
                "employed"
            },
            row
        )
        .unwrap();
    }
    let opened = open_table(source.path()).unwrap();
    let result = execute_prompt(&opened, "List rows where unemployed");
    assert_eq!(
        result.outcome,
        CoreOutcome::Materialized,
        "{:?}",
        result.diagnostics
    );
    assert_eq!(retained_rows(&result), [11_999]);
    let GroundingOutcome::Grounded { evidence, .. } = &result.grounding.unwrap().outcome else {
        panic!("expected grounded evidence");
    };
    assert_eq!(evidence[0].selected.evidence.match_locations.len(), 1);
    assert_eq!(evidence[0].column_match_counts.len(), 3);
}

#[test]
fn source_change_after_opening_refuses_grounding() {
    let mut source = tempfile::NamedTempFile::new().unwrap();
    source
        .write_all(b"ID,Job\n1,unemployed\n2,employed\n")
        .unwrap();
    let opened = open_table(source.path()).unwrap();
    source.as_file_mut().set_len(0).unwrap();
    source.seek(SeekFrom::Start(0)).unwrap();
    source
        .write_all(b"ID,Job\n1,employed\n2,employed\n")
        .unwrap();
    source.flush().unwrap();
    let result = execute_prompt(&opened, "List rows where unemployed");
    assert_eq!(result.outcome, CoreOutcome::Failed);
    assert!(result.plan.is_none());
    assert!(result.output.is_none());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "grounding.stale_clarification")
    );
}
