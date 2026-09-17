use std::path::Path;

use baho_exec::executor::{ExecutionResult, GridInput, execute_plan};
use baho_ingest::InspectOptions;
use baho_ingest::profile::InputProfile;
use baho_ingest::traits::FormatImporter;
use baho_ingest_csv::CsvImporter;
use baho_ingest_csv::candidates::CandidateConfig;
use baho_ingest_csv::classify_rows;
use baho_ingest_csv::header::build_header;
use baho_ingest_csv::row_features::compute_row_features;
use baho_model::candidate::TableCandidate;
use baho_model::diagnostic::{Diagnostic, Severity};
use baho_model::document::Value;
use baho_model::materialized::MaterializedView;
use baho_plan::plan::{DistinctKeep, Expression, Plan, PlanSource, PlanStep};
use baho_plan::validation::{validate_plan_references, validate_plan_structure};
use serde::{Deserialize, Serialize};

use crate::candidate_selection::select_candidate;
use crate::error::CoreError;
use crate::intent::{RecognizedIntent, recognize_intent};

/// The outcome of a pipeline run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreOutcome {
    Materialized,
    Recorded,
    Failed,
}

/// A structured event emitted during pipeline execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreEvent {
    pub name: String,
    pub stage: String,
    pub fields: serde_json::Value,
}

/// The complete result of running the core pipeline.
#[derive(Debug)]
pub struct CoreResult {
    pub input_profile: Option<InputProfile>,
    pub parser_config: Option<CandidateConfig>,
    pub candidates: Vec<TableCandidate>,
    pub selected_candidate: Option<TableCandidate>,
    pub intent: Option<RecognizedIntent>,
    pub plan: Option<Plan>,
    pub output: Option<MaterializedView>,
    pub diagnostics: Vec<Diagnostic>,
    pub events: Vec<CoreEvent>,
    pub outcome: CoreOutcome,
}

/// Run the full pipeline: ingest, detect, select, plan, validate, execute.
pub fn run_pipeline(path: &Path, prompt: &str) -> CoreResult {
    let mut result = CoreResult {
        input_profile: None,
        parser_config: None,
        candidates: Vec::new(),
        selected_candidate: None,
        intent: None,
        plan: None,
        output: None,
        diagnostics: Vec::new(),
        events: Vec::new(),
        outcome: CoreOutcome::Recorded,
    };

    // Step 1: Import
    let importer = CsvImporter;
    let options = InspectOptions::default();
    let imported = match importer.import(path, &options) {
        Ok(doc) => doc,
        Err(e) => {
            result.diagnostics.push(Diagnostic {
                code: "core.import_failed".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: e.to_string(),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
    };

    result.input_profile = Some(imported.input_profile.clone());
    result.diagnostics.extend(imported.diagnostics.clone());
    push_event(
        &mut result.events,
        "input_profiled",
        "ingest",
        serde_json::json!({
            "encoding": imported.input_profile.encoding,
            "record_count": imported.input_profile.logical_record_count,
        }),
    );

    let doc = &imported.document;
    let sheet = match doc.sheets.first() {
        Some(s) => s,
        None => {
            result.diagnostics.push(Diagnostic {
                code: "core.no_sheet".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: "imported document has no sheets".to_string(),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
    };

    // Step 2: Compute row features
    let logical_records: Vec<baho_ingest_csv::inspector::LogicalRecord> = sheet
        .rows
        .iter()
        .map(|r| baho_ingest_csv::inspector::LogicalRecord {
            index: r.index,
            fields: r.cells.iter().map(|c| c.raw_text.clone()).collect(),
            is_blank: r.cells.iter().all(|c| c.raw_text.trim().is_empty()),
        })
        .collect();
    let features = compute_row_features(&logical_records);

    // Step 3: Detect candidates
    let candidate_config = CandidateConfig::default();
    let candidates =
        baho_ingest_csv::detect_candidates(&logical_records, &features, &candidate_config);
    result.candidates = candidates.clone();
    push_event(
        &mut result.events,
        "table_candidates_detected",
        "detect",
        serde_json::json!({
            "count": candidates.len(),
        }),
    );

    // Step 4: Select candidate
    let selected = match select_candidate(&candidates, &candidate_config) {
        Ok(c) => c.clone(),
        Err(CoreError::NoTableFound) => {
            result.diagnostics.push(Diagnostic {
                code: "table.not_found".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: "no table candidate met the minimum score threshold".to_string(),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
        Err(CoreError::AmbiguousTable { candidate_count }) => {
            result.diagnostics.push(Diagnostic {
                code: "table.ambiguous".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: format!("{} candidates with similar scores", candidate_count),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
        Err(e) => {
            result.diagnostics.push(Diagnostic {
                code: "core.candidate_selection_failed".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: e.to_string(),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
    };

    result.selected_candidate = Some(selected.clone());
    push_event(
        &mut result.events,
        "table_candidate_selected",
        "select",
        serde_json::json!({
            "candidate_id": selected.id,
            "score": selected.score.total,
        }),
    );

    // Step 5: Build header from selected candidate
    let header_idx = selected.region.header_row.unwrap_or(0);
    let header_record = &logical_records[header_idx];
    let header_feature = &features[header_idx];
    let (header_decision, header_diag) = build_header(header_feature, header_record, 0);
    result.diagnostics.extend(header_diag);
    push_event(
        &mut result.events,
        "header_selected",
        "header",
        serde_json::json!({
            "source_row": header_decision.source_row,
            "column_count": header_decision.cells.len(),
        }),
    );

    // Step 6: Classify body rows
    let classifications = classify_rows(&features, header_idx, &candidate_config);
    let data_row_indices: Vec<usize> = classifications
        .iter()
        .filter(|c| {
            matches!(c.kind, baho_model::candidate::RowKind::Data)
                && c.source_row >= selected.region.body_start_row
                && c.source_row <= selected.region.body_end_row
        })
        .map(|c| c.source_row)
        .collect();
    push_event(
        &mut result.events,
        "body_rows_classified",
        "classify",
        serde_json::json!({
            "data_rows": data_row_indices.len(),
            "total_classified": classifications.len(),
        }),
    );

    // Update candidates with real header and classifications
    let selected_id = selected.id.clone();
    for candidate in result.candidates.iter_mut() {
        if candidate.id == selected_id {
            candidate.header = header_decision.clone();
            candidate.body_row_classifications = classifications.clone();
            candidate.selected = true;
        }
    }
    if let Some(ref mut sel) = result.selected_candidate {
        sel.header = header_decision.clone();
        sel.body_row_classifications = classifications.clone();
    }
    result.parser_config = Some(candidate_config.clone());

    // Build column definitions from header
    let columns: Vec<baho_model::column::ColumnDefinition> = header_decision
        .cells
        .iter()
        .map(|hc| baho_model::column::ColumnDefinition {
            id: hc.column_id.clone(),
            ordinal: hc.col,
            source_header_raw: Some(hc.raw_text.clone()),
            source_header_normalized: Some(hc.normalized_text.clone()),
            display_name: if hc.normalized_text.is_empty() {
                format!("Column {}", hc.col)
            } else {
                hc.normalized_text.clone()
            },
        })
        .collect();

    // Step 7: Recognize intent
    let intent = match recognize_intent(prompt, &columns) {
        Ok(i) => i,
        Err(e) => {
            result.diagnostics.push(Diagnostic {
                code: match &e {
                    crate::error::IntentError::Unsupported(_) => "intent.unsupported",
                    crate::error::IntentError::ColumnNotFound { .. } => "intent.column_not_found",
                    crate::error::IntentError::ColumnAmbiguous { .. } => "intent.column_ambiguous",
                    crate::error::IntentError::NoOperation => "intent.unsupported",
                }
                .to_string(),
                severity: Severity::Error,
                stage: "intent".to_string(),
                message: e.to_string(),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
    };

    push_event(
        &mut result.events,
        "intent_recognized",
        "intent",
        serde_json::json!({
            "operation": intent.operation,
            "column_id": intent.column_id,
            "score": intent.evidence.matched_column.as_ref().map(|m| m.score),
        }),
    );
    result.intent = Some(intent.clone());

    // Step 8: Build plan
    let plan = Plan {
        schema_version: 1,
        source: PlanSource {
            revision: doc.source.content_hash.clone(),
            table_id: selected.id.clone(),
        },
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
    };

    // Step 9: Validate plan
    if let Err(e) = validate_plan_structure(&plan) {
        result.diagnostics.push(Diagnostic {
            code: "plan.invalid".to_string(),
            severity: Severity::Error,
            stage: "plan".to_string(),
            message: e.to_string(),
            location: None,
        });
        result.outcome = CoreOutcome::Failed;
        return result;
    }

    let available_col_ids: Vec<String> = columns.iter().map(|c| c.id.clone()).collect();
    if let Err(e) = validate_plan_references(&plan, &available_col_ids) {
        result.diagnostics.push(Diagnostic {
            code: "plan.invalid".to_string(),
            severity: Severity::Error,
            stage: "plan".to_string(),
            message: e.to_string(),
            location: None,
        });
        result.outcome = CoreOutcome::Failed;
        return result;
    }

    push_event(
        &mut result.events,
        "plan_validated",
        "plan",
        serde_json::json!({
            "schema_version": plan.schema_version,
            "step_count": plan.steps.len(),
        }),
    );
    result.plan = Some(plan.clone());

    // Step 10: Build GridInput from the selected candidate's data rows
    let grid_rows: Vec<Vec<Option<Value>>> = data_row_indices
        .iter()
        .map(|&row_idx| {
            let row = &sheet.rows[row_idx];
            (0..columns.len())
                .map(|col| {
                    row.cells.get(col).map(|cell| {
                        if cell.raw_text.trim().is_empty() {
                            Value::Blank
                        } else {
                            Value::Text(cell.raw_text.clone())
                        }
                    })
                })
                .collect()
        })
        .collect();

    let grid = GridInput {
        table_id: selected.id.clone(),
        source_revision: doc.source.content_hash.clone(),
        columns: columns.clone(),
        rows: grid_rows,
        source_rows: data_row_indices.clone(),
    };

    // Step 11: Execute plan
    let exec_result: ExecutionResult = match execute_plan(&plan, &grid) {
        Ok(r) => r,
        Err(e) => {
            result.diagnostics.push(Diagnostic {
                code: "execution.failed".to_string(),
                severity: Severity::Error,
                stage: "exec".to_string(),
                message: e.to_string(),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
    };

    result.diagnostics.extend(exec_result.diagnostics);
    push_event(
        &mut result.events,
        "materialization_completed",
        "exec",
        serde_json::json!({
            "rows_processed": exec_result.rows_processed,
            "rows_output": exec_result.rows_output,
        }),
    );

    result.output = Some(exec_result.view);
    result.outcome = CoreOutcome::Materialized;

    result
}

fn push_event(events: &mut Vec<CoreEvent>, name: &str, stage: &str, fields: serde_json::Value) {
    events.push(CoreEvent {
        name: name.to_string(),
        stage: stage.to_string(),
        fields,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_csv(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file
    }

    #[test]
    fn full_pipeline_with_preamble_and_footer() {
        let csv = "\
Report Title,,,,
Generated: 2025-01-01,,,,
,,,,
ID,Floor Plan,Color
1,Type A,Red
2,Type B,Blue
3,Type A,Green
4,Type C,Red
,,,
Total,4 items,approximate,extra,notes,more
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "Extract all the unique floor plans");

        assert_eq!(result.outcome, CoreOutcome::Materialized);

        // Selected candidate exists
        let selected = result.selected_candidate.as_ref().unwrap();
        assert!(selected.score.total > 0.0);

        // Intent resolved to Floor Plan
        let intent = result.intent.as_ref().unwrap();
        assert_eq!(intent.operation, "distinct");
        let matched = intent.evidence.matched_column.as_ref().unwrap();
        assert!(matched.display_name.to_lowercase().contains("floor plan"));

        // Plan has filter/select/distinct
        let plan = result.plan.as_ref().unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert!(matches!(plan.steps[0], PlanStep::Filter { .. }));
        assert!(matches!(plan.steps[1], PlanStep::Select { .. }));
        assert!(matches!(plan.steps[2], PlanStep::Distinct { .. }));

        // Output has 3 rows: Type A, Type B, Type C
        let output = result.output.as_ref().unwrap();
        assert_eq!(output.rows.len(), 3);
        let values: Vec<&str> = output
            .rows
            .iter()
            .map(|r| match r.values.first().unwrap().as_ref().unwrap() {
                Value::Text(s) => s.as_str(),
                _ => panic!("expected text"),
            })
            .collect();
        assert_eq!(values, vec!["Type A", "Type B", "Type C"]);

        // Events in correct order
        let event_names: Vec<&str> = result.events.iter().map(|e| e.name.as_str()).collect();
        assert!(event_names.contains(&"input_profiled"));
        assert!(event_names.contains(&"table_candidates_detected"));
        assert!(event_names.contains(&"table_candidate_selected"));
        assert!(event_names.contains(&"header_selected"));
        assert!(event_names.contains(&"body_rows_classified"));
        assert!(event_names.contains(&"intent_recognized"));
        assert!(event_names.contains(&"plan_validated"));
        assert!(event_names.contains(&"materialization_completed"));

        // Verify event ordering
        let idx_profiled = event_names
            .iter()
            .position(|&n| n == "input_profiled")
            .unwrap();
        let idx_candidates = event_names
            .iter()
            .position(|&n| n == "table_candidates_detected")
            .unwrap();
        let idx_selected = event_names
            .iter()
            .position(|&n| n == "table_candidate_selected")
            .unwrap();
        let idx_intent = event_names
            .iter()
            .position(|&n| n == "intent_recognized")
            .unwrap();
        let idx_plan = event_names
            .iter()
            .position(|&n| n == "plan_validated")
            .unwrap();
        let idx_materialized = event_names
            .iter()
            .position(|&n| n == "materialization_completed")
            .unwrap();
        assert!(idx_profiled < idx_candidates);
        assert!(idx_candidates < idx_selected);
        assert!(idx_selected < idx_intent);
        assert!(idx_intent < idx_plan);
        assert!(idx_plan < idx_materialized);
    }

    #[test]
    fn empty_file_fails() {
        let file = write_temp_csv("");
        let result = run_pipeline(file.path(), "Extract all the unique floor plans");
        assert_eq!(result.outcome, CoreOutcome::Failed);
    }

    #[test]
    fn fixture_with_embedded_newline_header_and_interleaved_blanks() {
        let csv = "\
Report Title,,,,,,,,,,,,,,
Generated: 2025-01-01,,,,,,,,,,,,,,
,,,,,,,,,,,,,,,
ID,\"Floor
Plan\",Color,Status,Unit,Size,Year,Notes,,,,,,
,,,,,,,,,,,,,,,
1,Type A,Red,Active,101,850,2020,note1,,,,,,
2,Type B,Blue,Active,102,900,2021,note2,,,,,,
3,Type A,Green,Active,103,750,2020,note3,,,,,,
,,,,,,,,,,,,,,,
4,Type C,Red,Inactive,104,1200,2019,note4,,,,,,
5,Type B,Blue,Active,105,850,2022,note5,,,,,,
6,Type A,Green,Active,106,900,2020,note6,,,,,,
,,,,,,,,,,,,,,,
7,Type C,Red,Active,107,750,2021,note7,,,,,,
8,Type B,Blue,Inactive,108,1200,2019,note8,,,,,,
,,,,,,,,,,,,,,,
Total,8 items,approximate,,,,,extra,notes,more,columns,here,and,here,here,foot1,foot2,foot3
,,,,,,,,,,,,,,,
,,,,,,,,,,,,,,,,
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "Extract all the unique floor plans");

        assert_eq!(result.outcome, CoreOutcome::Materialized);

        // Header normalized from embedded newline to "Floor Plan"
        let intent = result.intent.as_ref().unwrap();
        assert_eq!(intent.column_display_name, "Floor Plan");
        let matched = intent.evidence.matched_column.as_ref().unwrap();
        assert!(matched.display_name.to_lowercase().contains("floor plan"));

        // Footer row is excluded: output has exactly 8 data rows processed
        let output = result.output.as_ref().unwrap();
        assert_eq!(output.rows.len(), 3);

        // Exactly 3 unique floor plan values in first-occurrence order
        let values: Vec<&str> = output
            .rows
            .iter()
            .map(|r| match r.values.first().unwrap().as_ref().unwrap() {
                Value::Text(s) => s.as_str(),
                _ => panic!("expected text"),
            })
            .collect();
        assert_eq!(values, vec!["Type A", "Type B", "Type C"]);

        // Blank separator rows did not terminate the table
        let selected = result.selected_candidate.as_ref().unwrap();
        assert!(selected.region.body_end_row > selected.region.body_start_row + 3);
    }

    #[test]
    fn two_tables_only_selected_body_rows_used() {
        // First table: header at row 3, data rows 4-11 (3 columns).
        // 11 blank separator rows (",,,") force find_body_rows to break
        // (blank_gap_lookahead=10). Trailing rows "x,," and "y,," have
        // density 1/3 ≈ 0.33 which is below is_header_like threshold (0.5)
        // so no second candidate is detected, but classify_rows still
        // classifies them as Data (density >= 0.3, width-compatible).
        // Without the body-bounds fix they'd be included in the output.
        let csv = "\
Report Title,,,,
Generated: 2025-01-01,,,,
,,,,
Name,Value,Status
Alice,100,Active
Bob,200,Active
Carol,300,Inactive
Dave,400,Active
Eve,500,Active
Frank,600,Inactive
George,700,Active
Hannah,850,Inactive
,,,
,,,
,,,
,,,
,,,
,,,
,,,
,,,
,,,
,,,
,,,
x,,
y,,
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "Extract all the unique names");

        assert_eq!(result.outcome, CoreOutcome::Materialized);

        let selected = result.selected_candidate.as_ref().unwrap();
        assert_eq!(selected.region.body_start_row, 4);
        assert_eq!(selected.region.body_end_row, 11);

        let output = result.output.as_ref().unwrap();
        let values: Vec<&str> = output
            .rows
            .iter()
            .map(|r| match r.values.first().unwrap().as_ref().unwrap() {
                Value::Text(s) => s.as_str(),
                _ => panic!("expected text"),
            })
            .collect();
        assert_eq!(
            values,
            vec![
                "Alice", "Bob", "Carol", "Dave", "Eve", "Frank", "George", "Hannah"
            ]
        );
    }

    #[test]
    fn candidates_have_populated_headers_and_classifications() {
        let csv = "\
Report Title,,,,
Generated: 2025-01-01,,,,
,,,,
ID,Floor Plan,Color
1,Type A,Red
2,Type B,Blue
3,Type A,Green
4,Type C,Red
,,,
Total,4 items,approximate,extra,notes,more
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "Extract all the unique floor plans");

        assert_eq!(result.outcome, CoreOutcome::Materialized);

        // Parser config is populated
        let config = result.parser_config.as_ref().unwrap();
        assert_eq!(config.schema_version, 1);

        // Selected candidate has populated header
        let selected = result.selected_candidate.as_ref().unwrap();
        assert!(
            !selected.header.cells.is_empty(),
            "selected candidate header cells must be populated"
        );
        assert!(
            !selected.body_row_classifications.is_empty(),
            "selected candidate classifications must be populated"
        );

        // The matching candidate in the candidates list is also updated
        let matching = result
            .candidates
            .iter()
            .find(|c| c.id == selected.id)
            .unwrap();
        assert!(
            !matching.header.cells.is_empty(),
            "candidate in list must have populated header cells"
        );
        assert!(
            !matching.body_row_classifications.is_empty(),
            "candidate in list must have populated classifications"
        );
        assert!(
            matching.selected,
            "matching candidate must be marked selected"
        );

        // Header cells contain the normalized "Floor Plan"
        let floor_plan_cell = selected
            .header
            .cells
            .iter()
            .find(|c| c.normalized_text == "Floor Plan");
        assert!(
            floor_plan_cell.is_some(),
            "header must contain normalized 'Floor Plan' cell"
        );
    }
}
