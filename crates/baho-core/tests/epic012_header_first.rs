//! Synthetic regressions for Epic 012's retrieval and header-first rules.

use std::io::Write;

use baho_core::{CoreOutcome, CoreResult, run_pipeline};
use baho_plan::plan::{ComparisonOperator, Expression, Literal, PlanStep};

fn run(csv: &str, prompt: &str) -> CoreResult {
    let mut source = tempfile::NamedTempFile::new().unwrap();
    source.write_all(csv.as_bytes()).unwrap();
    run_pipeline(source.path(), prompt)
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

#[test]
fn action_plus_complete_header_retrieves_even_when_header_is_also_a_value() {
    let csv = "ID,Delhi,City,Income\n1,true,Delhi,10\n2,false,Pune,20\n3,,Delhi,30\n";
    for (prompt, column) in [("Show Delhi", "column-1"), ("Show Income", "column-3")] {
        let result = run(csv, prompt);
        assert_eq!(result.outcome, CoreOutcome::Materialized, "{prompt}");
        let plan = result.plan.as_ref().unwrap();
        assert_eq!(
            plan.steps,
            vec![PlanStep::Select {
                columns: vec![column.into()]
            }]
        );
        assert_eq!(retained_rows(&result), [1, 2, 3]);
    }
}

#[test]
fn bare_header_filters_nonblank_values_including_false() {
    let csv = "ID,Delhi,City\n1,true,Pune\n2,false,Delhi\n3,,Pune\n";
    for prompt in ["Delhi", "List rows where Delhi"] {
        let result = run(csv, prompt);
        assert_eq!(result.outcome, CoreOutcome::Materialized, "{prompt}");
        assert_eq!(
            result.plan.as_ref().unwrap().steps,
            vec![PlanStep::Filter {
                predicate: Expression::IsNotBlank {
                    column: "column-1".into()
                },
            }],
            "{prompt}"
        );
        assert_eq!(retained_rows(&result), [1, 2], "{prompt}");
    }
}

#[test]
fn explicit_comparison_keeps_header_shaped_right_side_literal() {
    let result = run(
        "ID,Delhi,City\n1,true,Delhi\n2,false,Pune\n3,,Delhi\n",
        "City = Delhi",
    );
    assert_eq!(result.outcome, CoreOutcome::Materialized);
    assert_eq!(
        result.plan.as_ref().unwrap().steps,
        vec![PlanStep::Filter {
            predicate: Expression::Compare {
                column: "column-2".into(),
                operator: ComparisonOperator::Equal,
                literal: Literal::Text("Delhi".into()),
            },
        }]
    );
    assert_eq!(retained_rows(&result), [1, 3]);
}

#[test]
fn bare_header_value_collision_chooses_header_without_clarification() {
    let result = run(
        "ID,Active,Status\n1,true,active\n2,false,inactive\n3,,active\n",
        "List rows where Active",
    );
    assert_eq!(result.outcome, CoreOutcome::Materialized);
    assert_eq!(
        result.plan.as_ref().unwrap().steps,
        vec![PlanStep::Filter {
            predicate: Expression::IsNotBlank {
                column: "column-1".into()
            },
        }]
    );
    assert_eq!(retained_rows(&result), [1, 2]);
}

#[test]
fn connector_only_value_chains_ground_and_keep_precedence() {
    let csv =
        "ID,City,Role\n1,Jaipur,analyst\n2,Pune,analyst\n3,Jaipur,engineer\n4,Delhi,engineer\n";
    for (prompt, expected) in [
        ("Jaipur and analyst", vec![1]),
        ("Jaipur and analyst or engineer", vec![1, 3, 4]),
        ("Show Jaipur and analyst", vec![1]),
        ("Jaipur and analyst but not engineer", vec![1]),
        ("Jaipur and (analyst or engineer)", vec![1, 3]),
    ] {
        let result = run(csv, prompt);
        assert_eq!(result.outcome, CoreOutcome::Materialized, "{prompt}");
        assert_eq!(retained_rows(&result), expected, "{prompt}");
        assert!(matches!(
            result.plan.as_ref().unwrap().steps.as_slice(),
            [PlanStep::Filter { .. }]
        ));
    }
}

#[test]
fn connector_chain_can_ground_two_values_in_one_column() {
    let result = run("ID,City\n1,Jaipur\n2,Pune\n3,Delhi\n", "Jaipur or Pune");
    assert_eq!(result.outcome, CoreOutcome::Materialized);
    assert_eq!(retained_rows(&result), [1, 2]);
}

#[test]
fn selected_interpretation_is_visible_in_summary_and_structured_event() {
    let csv = "ID,Delhi,City\n1,true,Jaipur\n2,false,Pune\n3,,Jaipur\n";
    let header = run(csv, "Delhi");
    assert_eq!(
        header.interpretation_summary().as_deref(),
        Some("Filter rows where Delhi is not blank")
    );
    let event = header
        .events
        .iter()
        .find(|event| event.name == "interpretation_selected")
        .unwrap();
    assert_eq!(
        event.fields["header_bindings"][0]["rule"],
        "complete_header_first"
    );
    assert_eq!(event.fields["value_bindings"].as_array().unwrap().len(), 0);

    let value = run(csv, "Jaipur");
    assert_eq!(
        value.interpretation_summary().as_deref(),
        Some("Filter rows where City = \"Jaipur\"")
    );
    let event = value
        .events
        .iter()
        .find(|event| event.name == "interpretation_selected")
        .unwrap();
    assert_eq!(event.fields["value_bindings"][0]["rule"], "observed_value");
    assert_eq!(event.fields["value_bindings"][0]["column_id"], "column-2");

    let retrieval = run(csv, "Show Delhi");
    assert_eq!(
        retrieval.interpretation_summary().as_deref(),
        Some("Retrieve Delhi")
    );
}

#[test]
fn ambiguous_value_in_compact_chain_waits_for_clarification() {
    let result = run(
        "ID,City,Note,Role\n1,Jaipur,other,analyst\n2,Pune,Jaipur,engineer\n",
        "Jaipur and analyst",
    );
    assert_eq!(result.outcome, CoreOutcome::Recorded);
    assert!(result.plan.is_none());
    assert!(result.output.is_none());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.code == "grounding.clarification_required")
    );
}

#[test]
fn missing_value_and_incomplete_compact_syntax_refuse_without_execution() {
    let csv = "ID,City,Role\n1,Jaipur,analyst\n2,Pune,engineer\n";
    for (prompt, code) in [
        ("Unknown and analyst", "grounding.value_not_found"),
        ("Jaipur and", "intent.predicate_unsupported"),
        ("City =", "intent.predicate_unsupported"),
    ] {
        let result = run(csv, prompt);
        assert!(result.plan.is_none(), "{prompt}");
        assert!(result.output.is_none(), "{prompt}");
        assert!(
            result.diagnostics.iter().any(|d| d.code == code),
            "{prompt}: {:?}",
            result.diagnostics
        );
    }
}

#[test]
fn quoted_connector_is_a_value_and_complete_connector_header_is_selectable() {
    let csv = "ID,A and B,A,B,Note\n1,x,x,x,or\n2,,x,,other\n";
    let value = run(csv, "\"or\"");
    assert_eq!(value.outcome, CoreOutcome::Materialized);
    assert_eq!(retained_rows(&value), [1]);

    let header = run(csv, "Show A and B");
    assert_eq!(header.outcome, CoreOutcome::Materialized);
    assert_eq!(
        header.interpretation_summary().as_deref(),
        Some("Retrieve A and B")
    );
}
