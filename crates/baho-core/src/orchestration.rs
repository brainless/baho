use std::path::Path;

use baho_exec::executor::{ExecutionResult, GridInput, execute_plan};
use baho_ingest::profile::InputProfile;
use baho_ingest::{DetectedFormat, ImportError, InspectOptions, detect_format};
use baho_ingest_csv::header::build_header_with_config;
use baho_ingest_csv::row_features::compute_row_features_with_config;
use baho_ingest_csv::{
    CsvImporter, DialectDetectionError, ParserConfig, SelectedRegionError,
    detect_candidates_with_config, read_selected_region,
};
use baho_model::candidate::TableCandidate;
use baho_model::diagnostic::{Diagnostic, Severity};
use baho_model::document::Value;
use baho_model::materialized::MaterializedView;
use baho_plan::evidence::RecognitionEvidence;
use baho_plan::plan::Plan;
use baho_plan::validation::{validate_plan_references, validate_plan_structure};
use serde::{Deserialize, Serialize};

use crate::candidate_selection::select_candidate;
use crate::error::CoreError;
use crate::intent::{RecognizedIntent, compile_intent_to_plan, recognize_intent};

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
    pub parser_config: Option<ParserConfig>,
    pub candidates: Vec<TableCandidate>,
    pub selected_candidate: Option<TableCandidate>,
    pub intent: Option<RecognizedIntent>,
    pub plan: Option<Plan>,
    pub intent_evidence: Option<RecognitionEvidence>,
    pub output: Option<MaterializedView>,
    pub diagnostics: Vec<Diagnostic>,
    pub events: Vec<CoreEvent>,
    pub outcome: CoreOutcome,
}

/// A raw cell in an opened selected table. `Missing` is distinct from an
/// explicitly present empty field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCell {
    Present(String),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedRow {
    pub source_row: usize,
    pub cells: Vec<RawCell>,
}

/// The selected source table, before intent recognition or plan execution.
#[derive(Debug)]
pub struct OpenedTable {
    pub source_revision: baho_model::revision::SourceRevision,
    pub source_sheet_index: usize,
    pub source_sheet_name: Option<String>,
    pub selected_candidate: TableCandidate,
    pub candidates: Vec<TableCandidate>,
    pub columns: Vec<baho_model::column::ColumnDefinition>,
    pub rows: Vec<OpenedRow>,
    pub input_profile: InputProfile,
    pub parser_config: ParserConfig,
    pub diagnostics: Vec<Diagnostic>,
    pub events: Vec<CoreEvent>,
}

#[derive(Debug)]
pub struct OpenTableFailure {
    pub error: CoreError,
    pub input_profile: Option<InputProfile>,
    pub parser_config: Option<ParserConfig>,
    pub candidates: Vec<TableCandidate>,
    pub selected_candidate: Option<TableCandidate>,
    pub diagnostics: Vec<Diagnostic>,
    pub events: Vec<CoreEvent>,
}

impl std::fmt::Display for OpenTableFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for OpenTableFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Dispatch an input to a supported format importer without parsing it.
pub fn dispatch_format(path: &Path) -> Result<DetectedFormat, CoreError> {
    Ok(detect_format(path, &InspectOptions::default())?)
}

fn opening_failure(
    error: CoreError,
    input_profile: Option<InputProfile>,
    parser_config: Option<ParserConfig>,
    candidates: Vec<TableCandidate>,
    selected_candidate: Option<TableCandidate>,
    diagnostics: Vec<Diagnostic>,
    events: Vec<CoreEvent>,
) -> OpenTableFailure {
    OpenTableFailure {
        error,
        input_profile,
        parser_config,
        candidates,
        selected_candidate,
        diagnostics,
        events,
    }
}

/// Open and classify the selected CSV table without a prompt or plan.
pub fn open_table(path: &Path) -> Result<OpenedTable, OpenTableFailure> {
    let mut diagnostics = Vec::new();
    let mut events = Vec::new();
    let mut input_profile = None;
    let mut parser_config = None;
    let mut candidates = Vec::new();
    let mut selected_candidate = None;

    if let Err(error) = dispatch_format(path) {
        diagnostics.push(Diagnostic {
            code: if matches!(
                error,
                CoreError::Ingest(ImportError::UnsupportedFormat { .. })
            ) {
                "core.unsupported_format"
            } else {
                "core.format_detection_failed"
            }
            .to_string(),
            severity: Severity::Error,
            stage: "ingest".to_string(),
            message: error.to_string(),
            location: None,
        });
        return Err(opening_failure(
            error,
            input_profile,
            parser_config,
            candidates,
            selected_candidate,
            diagnostics,
            events,
        ));
    }

    let config = match ParserConfig::detect(path, InspectOptions::default()) {
        Ok(config) => config,
        Err(error) => {
            let code = match &error {
                DialectDetectionError::Ambiguous { .. } => "csv.dialect_ambiguous",
                DialectDetectionError::Io { .. } => "core.import_failed",
            };
            diagnostics.push(Diagnostic {
                code: code.to_string(),
                severity: Severity::Error,
                stage: "ingest-csv".to_string(),
                message: error.to_string(),
                location: None,
            });
            let core_error = CoreError::Ingest(ImportError::FormatDetectionFailed {
                detail: error.to_string(),
            });
            return Err(opening_failure(
                core_error,
                input_profile,
                parser_config,
                candidates,
                selected_candidate,
                diagnostics,
                events,
            ));
        }
    };
    parser_config = Some(config.clone());

    let imported = match CsvImporter.import_with_config(path, &config) {
        Ok(imported) => imported,
        Err(error) => {
            let core_error = CoreError::Ingest(error);
            diagnostics.push(Diagnostic {
                code: "core.import_failed".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: core_error.to_string(),
                location: None,
            });
            return Err(opening_failure(
                core_error,
                input_profile,
                parser_config,
                candidates,
                selected_candidate,
                diagnostics,
                events,
            ));
        }
    };
    input_profile = Some(imported.input_profile);
    diagnostics.extend(imported.diagnostics);
    let profile = input_profile.as_ref().expect("import profile stored");
    push_event(
        &mut events,
        "input_profiled",
        "ingest",
        serde_json::json!({
            "encoding": profile.encoding, "record_count": profile.logical_record_count,
        }),
    );

    let source_revision = imported.document.source;
    let sheet = match imported.document.sheets.into_iter().next() {
        Some(sheet) => sheet,
        None => {
            let error = CoreError::NoTableFound;
            diagnostics.push(Diagnostic {
                code: "core.no_sheet".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: "imported document has no sheets".to_string(),
                location: None,
            });
            return Err(opening_failure(
                error,
                input_profile,
                parser_config,
                candidates,
                selected_candidate,
                diagnostics,
                events,
            ));
        }
    };
    let source_sheet_index = sheet.index;
    let source_sheet_name = sheet.name;
    let logical_records = sheet
        .rows
        .into_iter()
        .map(|row| {
            let fields = row
                .cells
                .into_iter()
                .map(|cell| cell.raw_text)
                .collect::<Vec<_>>();
            let is_blank = fields
                .iter()
                .all(|field| config.normalization.is_blank(field));
            baho_ingest_csv::inspector::LogicalRecord {
                index: row.index,
                fields,
                is_blank,
            }
        })
        .collect::<Vec<_>>();
    let features = compute_row_features_with_config(&logical_records, &config.normalization);
    candidates = detect_candidates_with_config(
        &logical_records,
        &features,
        &config.candidate_detection,
        &config.candidate_scoring,
        &config.candidate_ordering,
    );
    push_event(
        &mut events,
        "table_candidates_detected",
        "detect",
        serde_json::json!({ "count": candidates.len() }),
    );

    let mut selected = match select_candidate(&candidates, &config.candidate_detection) {
        Ok(candidate) => candidate.clone(),
        Err(error @ CoreError::NoTableFound) => {
            diagnostics.push(Diagnostic {
                code: "table.not_found".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: "no table candidate met the minimum score threshold".to_string(),
                location: None,
            });
            return Err(opening_failure(
                error,
                input_profile,
                parser_config,
                candidates,
                selected_candidate,
                diagnostics,
                events,
            ));
        }
        Err(error @ CoreError::AmbiguousTable { candidate_count }) => {
            diagnostics.push(Diagnostic {
                code: "table.ambiguous".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: format!("{} candidates with similar scores", candidate_count),
                location: None,
            });
            return Err(opening_failure(
                error,
                input_profile,
                parser_config,
                candidates,
                selected_candidate,
                diagnostics,
                events,
            ));
        }
        Err(error) => {
            diagnostics.push(Diagnostic {
                code: "core.candidate_selection_failed".to_string(),
                severity: Severity::Error,
                stage: "core".to_string(),
                message: error.to_string(),
                location: None,
            });
            return Err(opening_failure(
                error,
                input_profile,
                parser_config,
                candidates,
                selected_candidate,
                diagnostics,
                events,
            ));
        }
    };
    selected_candidate = Some(selected.clone());
    push_event(
        &mut events,
        "table_candidate_selected",
        "select",
        serde_json::json!({ "candidate_id": selected.id, "score": selected.score.total }),
    );

    let header_idx = selected.region.header_row.unwrap_or(0);
    let (header_decision, header_diag) = build_header_with_config(
        &features[header_idx],
        &logical_records[header_idx],
        source_sheet_index,
        &config.normalization,
    );
    diagnostics.extend(header_diag);
    push_event(
        &mut events,
        "header_selected",
        "header",
        serde_json::json!({ "source_row": header_decision.source_row, "column_count": header_decision.cells.len() }),
    );

    let selected_region = match read_selected_region(
        path,
        features[header_idx].physical_width,
        selected.region.body_start_row,
        &config,
    ) {
        Ok(region) => region,
        Err(error) => {
            let (code, stage, location) = match &error {
                SelectedRegionError::FieldTooLarge { row, col, .. } => (
                    "csv.field_too_large",
                    "ingest-csv",
                    Some(baho_model::diagnostic::DiagnosticLocation {
                        row: Some(*row),
                        col: Some(*col),
                        cell: None,
                    }),
                ),
                SelectedRegionError::MalformedRecord { row, .. } => (
                    "csv.malformed_record",
                    "ingest-csv",
                    Some(baho_model::diagnostic::DiagnosticLocation {
                        row: Some(*row),
                        col: None,
                        cell: None,
                    }),
                ),
                SelectedRegionError::Io { .. } => ("core.materialize_region_failed", "core", None),
            };
            diagnostics.push(Diagnostic {
                code: code.to_string(),
                severity: Severity::Error,
                stage: stage.to_string(),
                message: error.to_string(),
                location,
            });
            let core_error = CoreError::Ingest(ImportError::FormatDetectionFailed {
                detail: error.to_string(),
            });
            return Err(opening_failure(
                core_error,
                input_profile,
                parser_config,
                candidates,
                selected_candidate,
                diagnostics,
                events,
            ));
        }
    };
    selected.region.body_end_row = selected_region.body_end_row;
    push_event(
        &mut events,
        "body_rows_classified",
        "classify",
        serde_json::json!({
            "data_rows": selected_region.data_records.len(), "total_classified": selected_region.classification_count,
            "classification_evidence_retained": selected_region.classifications.len(),
        }),
    );
    for candidate in &mut candidates {
        if candidate.id == selected.id {
            candidate.header = header_decision.clone();
            candidate.region.body_end_row = selected.region.body_end_row;
            candidate.body_row_classifications = selected_region.classifications.clone();
            candidate.selected = true;
        }
    }
    selected.header = header_decision.clone();
    selected.body_row_classifications = selected_region.classifications.clone();
    let columns = header_decision
        .cells
        .iter()
        .map(|cell| baho_model::column::ColumnDefinition {
            id: cell.column_id.clone(),
            ordinal: cell.col,
            source_header_raw: Some(cell.raw_text.clone()),
            source_header_normalized: Some(cell.normalized_text.clone()),
            display_name: if cell.normalized_text.is_empty() {
                format!("Column {}", cell.col)
            } else {
                cell.normalized_text.clone()
            },
        })
        .collect::<Vec<_>>();
    let rows = selected_region
        .data_records
        .iter()
        .map(|record| OpenedRow {
            source_row: record.index,
            cells: (0..columns.len())
                .map(|col| {
                    record
                        .fields
                        .get(col)
                        .cloned()
                        .map(RawCell::Present)
                        .unwrap_or(RawCell::Missing)
                })
                .collect(),
        })
        .collect();
    Ok(OpenedTable {
        source_revision,
        source_sheet_index,
        source_sheet_name,
        selected_candidate: selected,
        candidates,
        columns,
        rows,
        input_profile: input_profile.expect("import profile stored"),
        parser_config: parser_config.expect("parser config stored"),
        diagnostics,
        events,
    })
}

/// Run the full pipeline: ingest, detect, select, plan, validate, execute.
pub fn run_pipeline(path: &Path, prompt: &str) -> CoreResult {
    let opened = match open_table(path) {
        Ok(opened) => opened,
        Err(failure) => {
            return CoreResult {
                input_profile: failure.input_profile,
                parser_config: failure.parser_config,
                candidates: failure.candidates,
                selected_candidate: failure.selected_candidate,
                intent: None,
                plan: None,
                intent_evidence: None,
                output: None,
                diagnostics: failure.diagnostics,
                events: failure.events,
                outcome: CoreOutcome::Failed,
            };
        }
    };
    execute_prompt(&opened, prompt)
}

/// Recognize and execute one prompt against an already opened source table.
///
/// The opened table is borrowed so callers can issue independent requests
/// against the same immutable source snapshot.
pub fn execute_prompt(opened: &OpenedTable, prompt: &str) -> CoreResult {
    let mut result = CoreResult {
        input_profile: Some(opened.input_profile.clone()),
        parser_config: Some(opened.parser_config.clone()),
        candidates: opened.candidates.clone(),
        selected_candidate: Some(opened.selected_candidate.clone()),
        intent: None,
        plan: None,
        intent_evidence: None,
        output: None,
        diagnostics: opened.diagnostics.clone(),
        events: opened.events.clone(),
        outcome: CoreOutcome::Recorded,
    };
    let intent = match recognize_intent(prompt, &opened.columns) {
        Ok(intent) => intent,
        Err(error) => {
            result.intent_evidence = recognition_evidence_of(&error);
            result.diagnostics.push(Diagnostic {
                code: match &error {
                    crate::error::IntentError::Unsupported(_, _) => "intent.unsupported",
                    crate::error::IntentError::ColumnNotFound { .. } => "intent.column_not_found",
                    crate::error::IntentError::ColumnAmbiguous { .. } => "intent.column_ambiguous",
                    crate::error::IntentError::ParseAmbiguous { .. } => "intent.parse_ambiguous",
                }
                .to_string(),
                severity: Severity::Error,
                stage: "intent".to_string(),
                message: error.to_string(),
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
            "action": intent.action, "operation": intent.operation, "column_id": intent.column_id,
            "score": intent.evidence.matched_column.as_ref().map(|matched| matched.score),
        }),
    );
    result.intent_evidence = Some(intent.evidence.clone());
    result.intent = Some(intent.clone());
    let plan = compile_intent_to_plan(
        &intent,
        &opened.source_revision.content_hash,
        &opened.selected_candidate.id,
    );
    if let Err(error) = validate_plan_structure(&plan) {
        result.diagnostics.push(Diagnostic {
            code: "plan.invalid".to_string(),
            severity: Severity::Error,
            stage: "plan".to_string(),
            message: error.to_string(),
            location: None,
        });
        result.outcome = CoreOutcome::Failed;
        return result;
    }
    let available_col_ids = opened
        .columns
        .iter()
        .map(|column| column.id.clone())
        .collect::<Vec<_>>();
    if let Err(error) = validate_plan_references(&plan, &available_col_ids) {
        result.diagnostics.push(Diagnostic {
            code: "plan.invalid".to_string(),
            severity: Severity::Error,
            stage: "plan".to_string(),
            message: error.to_string(),
            location: None,
        });
        result.outcome = CoreOutcome::Failed;
        return result;
    }
    push_event(
        &mut result.events,
        "plan_validated",
        "plan",
        serde_json::json!({ "schema_version": plan.schema_version, "step_count": plan.steps.len() }),
    );
    result.plan = Some(plan.clone());
    let grid = GridInput {
        table_id: opened.selected_candidate.id.clone(),
        source_revision: opened.source_revision.content_hash.clone(),
        source_sheet_index: opened.source_sheet_index,
        columns: opened.columns.clone(),
        rows: opened
            .rows
            .iter()
            .map(|row| {
                row.cells
                    .iter()
                    .map(|cell| match cell {
                        RawCell::Present(text)
                            if opened.parser_config.normalization.is_blank(text) =>
                        {
                            Some(Value::Blank)
                        }
                        RawCell::Present(text) => Some(Value::Text(text.clone())),
                        RawCell::Missing => None,
                    })
                    .collect()
            })
            .collect(),
        source_rows: opened.rows.iter().map(|row| row.source_row).collect(),
    };
    let execution = match execute_plan(&plan, &grid) {
        Ok(execution) => execution,
        Err(error) => {
            result.diagnostics.push(Diagnostic {
                code: "execution.failed".to_string(),
                severity: Severity::Error,
                stage: "exec".to_string(),
                message: error.to_string(),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
    };
    result.diagnostics.extend(execution.diagnostics);
    push_event(
        &mut result.events,
        "materialization_completed",
        "exec",
        serde_json::json!({
            "rows_processed": execution.rows_processed, "rows_output": execution.rows_output,
        }),
    );
    result.output = Some(execution.view);
    result.outcome = CoreOutcome::Materialized;
    result
}

#[allow(dead_code)]
fn run_pipeline_legacy(path: &Path, prompt: &str) -> CoreResult {
    let mut result = CoreResult {
        input_profile: None,
        parser_config: None,
        candidates: Vec::new(),
        selected_candidate: None,
        intent: None,
        plan: None,
        intent_evidence: None,
        output: None,
        diagnostics: Vec::new(),
        events: Vec::new(),
        outcome: CoreOutcome::Recorded,
    };

    // Step 1: Dispatch, then import. This boundary prevents recognized
    // non-CSV inputs from reaching CSV dialect detection.
    if let Err(error) = dispatch_format(path) {
        result.diagnostics.push(Diagnostic {
            code: match &error {
                CoreError::Ingest(ImportError::UnsupportedFormat { .. }) => {
                    "core.unsupported_format"
                }
                _ => "core.format_detection_failed",
            }
            .to_string(),
            severity: Severity::Error,
            stage: "ingest".to_string(),
            message: error.to_string(),
            location: None,
        });
        result.outcome = CoreOutcome::Failed;
        return result;
    }

    // Step 2: Import
    let importer = CsvImporter;
    let options = InspectOptions::default();
    let parser_config = match ParserConfig::detect(path, options) {
        Ok(config) => config,
        Err(error) => {
            result.diagnostics.push(Diagnostic {
                code: match &error {
                    DialectDetectionError::Ambiguous { .. } => "csv.dialect_ambiguous",
                    DialectDetectionError::Io { .. } => "core.import_failed",
                }
                .to_string(),
                severity: Severity::Error,
                stage: "ingest-csv".to_string(),
                message: error.to_string(),
                location: None,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
    };
    result.parser_config = Some(parser_config.clone());
    let imported = match importer.import_with_config(path, &parser_config) {
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

    result.input_profile = Some(imported.input_profile);
    result.diagnostics.extend(imported.diagnostics);
    let input_profile = result.input_profile.as_ref().unwrap();
    push_event(
        &mut result.events,
        "input_profiled",
        "ingest",
        serde_json::json!({
            "encoding": input_profile.encoding,
            "record_count": input_profile.logical_record_count,
        }),
    );

    let source_revision = imported.document.source.content_hash;
    let sheet = match imported.document.sheets.into_iter().next() {
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
    let source_sheet_index = sheet.index;

    // Step 2: Move the bounded analysis rows into CSV records. Moving the
    // strings avoids retaining both a sampled Document and a duplicate record
    // collection throughout the rest of the pipeline.
    let logical_records: Vec<baho_ingest_csv::inspector::LogicalRecord> = sheet
        .rows
        .into_iter()
        .map(|r| {
            let fields = r
                .cells
                .into_iter()
                .map(|cell| cell.raw_text)
                .collect::<Vec<_>>();
            let is_blank = fields
                .iter()
                .all(|field| parser_config.normalization.is_blank(field));
            baho_ingest_csv::inspector::LogicalRecord {
                index: r.index,
                fields,
                is_blank,
            }
        })
        .collect();
    let features = compute_row_features_with_config(&logical_records, &parser_config.normalization);

    // Step 3: Detect candidates
    let candidates = detect_candidates_with_config(
        &logical_records,
        &features,
        &parser_config.candidate_detection,
        &parser_config.candidate_scoring,
        &parser_config.candidate_ordering,
    );
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
    let mut selected = match select_candidate(&candidates, &parser_config.candidate_detection) {
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
    let (header_decision, header_diag) = build_header_with_config(
        header_feature,
        header_record,
        source_sheet_index,
        &parser_config.normalization,
    );
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
    let selected_region = match read_selected_region(
        path,
        header_feature.physical_width,
        selected.region.body_start_row,
        &parser_config,
    ) {
        Ok(region) => region,
        Err(error) => {
            let (code, stage, location) = match &error {
                SelectedRegionError::FieldTooLarge { row, col, .. } => (
                    "csv.field_too_large",
                    "ingest-csv",
                    Some(baho_model::diagnostic::DiagnosticLocation {
                        row: Some(*row),
                        col: Some(*col),
                        cell: None,
                    }),
                ),
                SelectedRegionError::MalformedRecord { row, .. } => (
                    "csv.malformed_record",
                    "ingest-csv",
                    Some(baho_model::diagnostic::DiagnosticLocation {
                        row: Some(*row),
                        col: None,
                        cell: None,
                    }),
                ),
                SelectedRegionError::Io { .. } => ("core.materialize_region_failed", "core", None),
            };
            result.diagnostics.push(Diagnostic {
                code: code.to_string(),
                severity: Severity::Error,
                stage: stage.to_string(),
                message: error.to_string(),
                location,
            });
            result.outcome = CoreOutcome::Failed;
            return result;
        }
    };
    selected.region.body_end_row = selected_region.body_end_row;
    let data_row_indices: Vec<usize> = selected_region
        .data_records
        .iter()
        .map(|record| record.index)
        .collect();
    let classifications = &selected_region.classifications;
    push_event(
        &mut result.events,
        "body_rows_classified",
        "classify",
        serde_json::json!({
            "data_rows": data_row_indices.len(),
            "total_classified": selected_region.classification_count,
            "classification_evidence_retained": classifications.len(),
        }),
    );

    // Update candidates with real header and classifications
    let selected_id = selected.id.clone();
    for candidate in result.candidates.iter_mut() {
        if candidate.id == selected_id {
            candidate.header = header_decision.clone();
            candidate.region.body_end_row = selected.region.body_end_row;
            candidate.body_row_classifications = classifications.clone();
            candidate.selected = true;
        }
    }
    if let Some(ref mut sel) = result.selected_candidate {
        sel.header = header_decision.clone();
        sel.region.body_end_row = selected.region.body_end_row;
        sel.body_row_classifications = classifications.clone();
    }

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
            result.intent_evidence = recognition_evidence_of(&e);
            result.diagnostics.push(Diagnostic {
                code: match &e {
                    crate::error::IntentError::Unsupported(_, _) => "intent.unsupported",
                    crate::error::IntentError::ColumnNotFound { .. } => "intent.column_not_found",
                    crate::error::IntentError::ColumnAmbiguous { .. } => "intent.column_ambiguous",
                    crate::error::IntentError::ParseAmbiguous { .. } => "intent.parse_ambiguous",
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
            "action": intent.action,
            "operation": intent.operation,
            "column_id": intent.column_id,
            "score": intent.evidence.matched_column.as_ref().map(|m| m.score),
        }),
    );
    result.intent = Some(intent.clone());
    result.intent_evidence = Some(intent.evidence.clone());

    // Step 8: Build plan
    let plan = compile_intent_to_plan(&intent, &source_revision, &selected.id);

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
    let data_rows = selected_region.data_records;
    let grid_rows: Vec<Vec<Option<Value>>> = data_rows
        .iter()
        .map(|row| {
            (0..columns.len())
                .map(|col| {
                    row.fields.get(col).map(|raw_text| {
                        if parser_config.normalization.is_blank(raw_text) {
                            Value::Blank
                        } else {
                            Value::Text(raw_text.clone())
                        }
                    })
                })
                .collect()
        })
        .collect();

    let grid = GridInput {
        table_id: selected.id.clone(),
        source_revision,
        source_sheet_index,
        columns: columns.clone(),
        rows: grid_rows,
        source_rows: data_rows.iter().map(|row| row.index).collect(),
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

fn recognition_evidence_of(error: &crate::error::IntentError) -> Option<RecognitionEvidence> {
    match error {
        crate::error::IntentError::Unsupported(_, evidence) => evidence.clone(),
        crate::error::IntentError::ColumnNotFound { evidence, .. } => evidence.clone(),
        crate::error::IntentError::ColumnAmbiguous { evidence, .. } => evidence.clone(),
        crate::error::IntentError::ParseAmbiguous { evidence, .. } => evidence.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_plan::plan::PlanStep;
    use std::io::Write;

    fn write_temp_csv(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file
    }

    #[test]
    fn open_table_preserves_preamble_source_rows_and_ragged_raw_cells() {
        let file = write_temp_csv(
            "Report title,,,\n,,,\nID,Name,Note\n1,A,\n2,B\n3,C,raw\nFooter,summary,extra,ignored,tail,more\n",
        );

        let opened = open_table(file.path()).expect("synthetic table should open");

        assert_eq!(opened.source_sheet_index, 0);
        assert!(!opened.source_revision.content_hash.is_empty());
        assert_eq!(
            opened
                .columns
                .iter()
                .map(|column| column.display_name.as_str())
                .collect::<Vec<_>>(),
            ["ID", "Name", "Note"]
        );
        assert_eq!(
            opened
                .rows
                .iter()
                .map(|row| row.source_row)
                .collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert_eq!(opened.rows[0].cells[2], RawCell::Present(String::new()));
        assert_eq!(opened.rows[1].cells[2], RawCell::Missing);
        assert_eq!(opened.rows[2].cells[2], RawCell::Present("raw".to_string()));
        assert!(
            opened
                .selected_candidate
                .body_row_classifications
                .iter()
                .any(|classification| classification.source_row == 4)
        );
        assert!(
            opened
                .events
                .iter()
                .any(|event| event.name == "body_rows_classified")
        );
    }

    #[test]
    fn run_pipeline_reuses_opening_metadata_and_event_prefix() {
        let file = write_temp_csv(
            "Title,,,\n,,,\nID,Name,Note\n1,A,\n2,B\n3,C,raw\nFooter,summary,extra,ignored,tail,more\n",
        );
        let opened = open_table(file.path()).expect("synthetic table should open");
        let pipeline = run_pipeline(file.path(), "List name");

        assert_eq!(pipeline.outcome, CoreOutcome::Materialized);
        assert_eq!(pipeline.input_profile.as_ref(), Some(&opened.input_profile));
        assert_eq!(pipeline.parser_config.as_ref(), Some(&opened.parser_config));
        assert_eq!(
            pipeline.selected_candidate.as_ref(),
            Some(&opened.selected_candidate)
        );
        let opening_event_names = opened
            .events
            .iter()
            .map(|event| event.name.as_str())
            .collect::<Vec<_>>();
        let pipeline_event_names = pipeline
            .events
            .iter()
            .map(|event| event.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            &pipeline_event_names[..opening_event_names.len()],
            opening_event_names
        );
        assert_eq!(
            pipeline
                .output
                .as_ref()
                .unwrap()
                .provenance
                .iter()
                .map(|provenance| provenance.source_row)
                .collect::<Vec<_>>(),
            [3, 4, 5]
        );
    }

    #[test]
    fn borrowed_prompt_execution_matches_pipeline_for_success_and_refusal() {
        let file = write_temp_csv("ID,Name\n1,Ada\n2,Bob\n3,Ada\n");
        let opened = open_table(file.path()).expect("synthetic table should open");

        for prompt in ["List unique name", "Calculate an average"] {
            let borrowed = execute_prompt(&opened, prompt);
            let pipeline = run_pipeline(file.path(), prompt);

            assert_eq!(borrowed.outcome, pipeline.outcome);
            assert_eq!(borrowed.input_profile, pipeline.input_profile);
            assert_eq!(borrowed.parser_config, pipeline.parser_config);
            assert_eq!(borrowed.candidates, pipeline.candidates);
            assert_eq!(borrowed.selected_candidate, pipeline.selected_candidate);
            assert_eq!(borrowed.plan, pipeline.plan);
            assert_eq!(borrowed.intent_evidence, pipeline.intent_evidence);
            assert_eq!(borrowed.output, pipeline.output);
            assert_eq!(borrowed.diagnostics, pipeline.diagnostics);
            assert_eq!(
                serde_json::to_value(&borrowed.events).unwrap(),
                serde_json::to_value(&pipeline.events).unwrap()
            );
        }
    }

    #[test]
    fn borrowed_prompt_execution_is_repeatable_and_does_not_mutate_opened_table() {
        let file = write_temp_csv("ID,Name,City\n1,Ada,Pune\n2,Bob,Delhi\n3,Ada,Pune\n");
        let opened = open_table(file.path()).expect("synthetic table should open");
        let revision_before = opened.source_revision.clone();
        let candidate_before = opened.selected_candidate.clone();
        let candidates_before = opened.candidates.clone();
        let columns_before = opened.columns.clone();
        let rows_before = opened.rows.clone();
        let profile_before = opened.input_profile.clone();
        let config_before = opened.parser_config.clone();
        let diagnostics_before = opened.diagnostics.clone();
        let events_before = serde_json::to_value(&opened.events).unwrap();

        let names = execute_prompt(&opened, "List unique name");
        let cities = execute_prompt(&opened, "List city");
        let names_again = execute_prompt(&opened, "List unique name");

        assert_eq!(names.outcome, CoreOutcome::Materialized);
        assert_eq!(cities.outcome, CoreOutcome::Materialized);
        assert_eq!(names.output, names_again.output);
        assert_eq!(names.plan, names_again.plan);
        assert_eq!(names.intent_evidence, names_again.intent_evidence);
        assert_eq!(names.output.as_ref().unwrap().rows.len(), 2);
        assert_eq!(cities.output.as_ref().unwrap().rows.len(), 3);

        assert_eq!(opened.source_revision, revision_before);
        assert_eq!(opened.selected_candidate, candidate_before);
        assert_eq!(opened.candidates, candidates_before);
        assert_eq!(opened.columns, columns_before);
        assert_eq!(opened.rows, rows_before);
        assert_eq!(opened.input_profile, profile_before);
        assert_eq!(opened.parser_config, config_before);
        assert_eq!(opened.diagnostics, diagnostics_before);
        assert_eq!(serde_json::to_value(&opened.events).unwrap(), events_before);
    }

    #[test]
    fn borrowed_prompt_execution_preserves_original_source_provenance() {
        let file =
            write_temp_csv("Report,,\n,,\nID,Name,City\n1,Ada,Pune\n2,Bob,Delhi\n3,Ada,Pune\n");
        let opened = open_table(file.path()).expect("synthetic table should open");

        let result = execute_prompt(&opened, "List unique name");
        let output = result.output.expect("supported prompt should materialize");

        assert_eq!(
            output
                .provenance
                .iter()
                .map(|provenance| {
                    (
                        provenance.source_row,
                        provenance.source_addresses[0].sheet_index,
                        provenance.source_addresses[0].row,
                        provenance.source_addresses[0].col,
                    )
                })
                .collect::<Vec<_>>(),
            [(3, 0, 3, 1), (4, 0, 4, 1)]
        );
        assert_eq!(
            result.plan.unwrap().source.revision,
            opened.source_revision.content_hash
        );
    }

    #[test]
    fn dispatch_returns_typed_unsupported_formats_before_csv_parsing() {
        let cases = [
            (
                "report.xlsx",
                b"not csv".as_slice(),
                baho_ingest::UnsupportedFormat::Excel,
            ),
            (
                "report.ods",
                b"not csv".as_slice(),
                baho_ingest::UnsupportedFormat::Ods,
            ),
            (
                "report.pdf",
                b"%PDF-1.7".as_slice(),
                baho_ingest::UnsupportedFormat::Pdf,
            ),
        ];

        for (suffix, bytes, expected) in cases {
            let mut file = tempfile::Builder::new().suffix(suffix).tempfile().unwrap();
            file.write_all(bytes).unwrap();
            assert!(matches!(
                dispatch_format(file.path()),
                Err(CoreError::Ingest(
                    baho_ingest::ImportError::UnsupportedFormat { format }
                )) if format == expected
            ));
        }
    }

    #[test]
    fn pipeline_keeps_unknown_input_distinct_from_unsupported_format() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"\0\x01\x02").unwrap();
        let result = run_pipeline(file.path(), "List values");

        assert_eq!(result.outcome, CoreOutcome::Failed);
        assert_eq!(result.diagnostics[0].code, "core.format_detection_failed");
        assert!(result.parser_config.is_none());

        let mut pdf = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
        pdf.write_all(b"%PDF-1.7").unwrap();
        let result = run_pipeline(pdf.path(), "List values");
        assert_eq!(result.outcome, CoreOutcome::Failed);
        assert_eq!(result.diagnostics[0].code, "core.unsupported_format");
        assert!(result.diagnostics[0].message.contains("PDF"));
        assert!(result.parser_config.is_none());
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
        assert_eq!(
            intent.operation,
            crate::intent::CanonicalOperation::Distinct
        );
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
        assert_eq!(
            output
                .provenance
                .iter()
                .map(|provenance| {
                    (
                        provenance.source_row,
                        provenance.source_addresses.as_slice(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (
                    4,
                    &[baho_model::document::CellAddress {
                        sheet_index: 0,
                        row: 4,
                        col: 1,
                    }][..]
                ),
                (
                    5,
                    &[baho_model::document::CellAddress {
                        sheet_index: 0,
                        row: 5,
                        col: 1,
                    }][..]
                ),
                (
                    7,
                    &[baho_model::document::CellAddress {
                        sheet_index: 0,
                        row: 7,
                        col: 1,
                    }][..]
                ),
            ]
        );

        let evidence = result
            .intent_evidence
            .as_ref()
            .expect("success must retain recognition evidence");
        assert_eq!(evidence.refusal_reason, None);
        assert_eq!(evidence.canonical_operation.as_deref(), Some("distinct"));
        assert_eq!(evidence.action.as_ref().unwrap().alias, "extract");
        assert_eq!(evidence.modifier.as_ref().unwrap().alias, "unique");
        assert_eq!(
            evidence.matched_column.as_ref().unwrap().display_name,
            "Floor Plan"
        );

        // Events in correct order
        let event_names: Vec<&str> = result.events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            event_names,
            [
                "input_profiled",
                "table_candidates_detected",
                "table_candidate_selected",
                "header_selected",
                "body_rows_classified",
                "intent_recognized",
                "plan_validated",
                "materialization_completed",
            ]
        );
    }

    #[test]
    fn all_text_table_with_five_body_rows_materializes() {
        let csv = "\
Name,City
Ada,London
Bob,Paris
Eve,Berlin
Lin,Taipei
Sam,Lagos
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "Extract all the unique names");

        assert_eq!(result.outcome, CoreOutcome::Materialized);
        let selected = result.selected_candidate.as_ref().unwrap();
        assert_eq!(selected.region.header_row, Some(0));
        assert_eq!(selected.region.body_end_row, 5);

        let values = result
            .output
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .map(|row| match row.values.first().unwrap().as_ref().unwrap() {
                Value::Text(value) => value.as_str(),
                _ => panic!("expected text"),
            })
            .collect::<Vec<_>>();
        assert_eq!(values, ["Ada", "Bob", "Eve", "Lin", "Sam"]);
    }

    #[test]
    fn large_input_retains_bounded_evidence_and_selected_region_provenance() {
        let mut csv = String::from("Name,Code\n");
        for index in 0..1_500 {
            let code = if index == 1_200 {
                "C"
            } else if index % 2 == 0 {
                "A"
            } else {
                "B"
            };
            csv.push_str(&format!("person-{index},{code}\n"));
        }
        let file = write_temp_csv(&csv);
        let result = run_pipeline(file.path(), "Extract all the unique codes");

        assert_eq!(result.outcome, CoreOutcome::Materialized);
        assert_eq!(
            result.input_profile.as_ref().unwrap().logical_record_count,
            Some(1_501)
        );
        let selected = result.selected_candidate.as_ref().unwrap();
        assert_eq!(selected.region.body_end_row, 1_500);
        assert_eq!(
            selected.body_row_classifications.len(),
            InspectOptions::default().max_sample_records
        );

        let output = result.output.as_ref().unwrap();
        let values = output
            .rows
            .iter()
            .map(|row| match row.values[0].as_ref().unwrap() {
                Value::Text(value) => value.as_str(),
                _ => panic!("expected text"),
            })
            .collect::<Vec<_>>();
        assert_eq!(values, ["A", "B", "C"]);
        assert_eq!(output.provenance[0].source_addresses[0].row, 1);
        assert_eq!(output.provenance[0].source_addresses[0].col, 1);
        assert_eq!(output.provenance[1].source_addresses[0].row, 2);
        assert_eq!(output.provenance[2].source_addresses[0].row, 1_201);
    }

    #[test]
    fn oversized_field_in_selected_region_retains_selected_candidate_without_output() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "Name,Code").unwrap();
        for (name, code) in [
            ("Ada", "A"),
            ("Bob", "B"),
            ("Eve", "C"),
            ("Lin", "D"),
            ("Sam", "E"),
        ] {
            writeln!(file, "{name},{code}").unwrap();
        }
        writeln!(
            file,
            "Oversized,{}",
            "x".repeat(InspectOptions::default().max_field_size + 1)
        )
        .unwrap();

        let result = run_pipeline(file.path(), "Extract all the unique codes");

        assert_eq!(result.outcome, CoreOutcome::Failed);
        assert!(result.output.is_none());
        let selected = result
            .selected_candidate
            .as_ref()
            .expect("candidate selected before the selected-region failure");
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.id == selected.id)
        );
        assert!(result.events.iter().any(|event| {
            event.name == "table_candidate_selected" && event.fields["candidate_id"] == selected.id
        }));
        let diagnostic = result
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code == "csv.field_too_large" && diagnostic.severity == Severity::Error
            })
            .expect("selected-region limit violation should be an error");
        let location = diagnostic.location.as_ref().unwrap();
        assert_eq!(location.row, Some(6));
        assert_eq!(location.col, Some(1));
        assert!(
            !result
                .events
                .iter()
                .any(|event| event.name == "materialization_completed")
        );
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

    #[test]
    fn pipeline_uses_content_detected_semicolon_dialect() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("misnamed.csv");
        std::fs::write(
            &path,
            "ID;Code;Comment\n1;A;\"comma, inside\"\n2;B;plain\n3;A;other\n4;C;last\n",
        )
        .unwrap();

        let result = run_pipeline(&path, "Extract all the unique codes");

        assert_eq!(result.outcome, CoreOutcome::Materialized);
        assert_eq!(
            result.parser_config.as_ref().unwrap().dialect.delimiter,
            b';'
        );
        assert_eq!(
            result.input_profile.as_ref().unwrap().detected_delimiter,
            Some(';')
        );
    }

    #[test]
    fn pipeline_refuses_ambiguous_dialect_with_stable_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mixed");
        std::fs::write(&path, "A,B;C\n1,2;3\n4,5;6\n").unwrap();

        let result = run_pipeline(&path, "Extract all the unique values");

        assert_eq!(result.outcome, CoreOutcome::Failed);
        assert!(result.output.is_none());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, "csv.dialect_ambiguous");
        assert_eq!(result.diagnostics[0].stage, "ingest-csv");
    }

    #[test]
    fn select_only_preserves_blanks_and_duplicates() {
        let csv = "\
ID,Income
1,50000
2,
3,60000
4,50000
5,
6,70000
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "List income");

        assert_eq!(result.outcome, CoreOutcome::Materialized);

        // Plan has exactly 1 step: Select only (no Filter, no Distinct)
        let plan = result.plan.as_ref().unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(plan.steps[0], PlanStep::Select { .. }));

        // Intent resolved as select-only
        let intent = result.intent.as_ref().unwrap();
        assert_eq!(intent.operation, crate::intent::CanonicalOperation::Select);
        assert_eq!(intent.column_display_name, "Income");

        // Output has 6 rows including blanks and duplicates
        let output = result.output.as_ref().unwrap();
        assert_eq!(output.rows.len(), 6);

        // Values: 50000, blank, 60000, 50000, blank, 70000
        let values: Vec<Option<&str>> = output
            .rows
            .iter()
            .map(|r| match r.values.first().unwrap() {
                Some(Value::Text(s)) => Some(s.as_str()),
                Some(Value::Blank) => None,
                _ => panic!("expected text or blank"),
            })
            .collect();
        assert_eq!(
            values,
            vec![
                Some("50000"),
                None,
                Some("60000"),
                Some("50000"),
                None,
                Some("70000")
            ]
        );

        // Source order is maintained
        assert_eq!(output.provenance[0].source_row, 1);
        assert_eq!(output.provenance[1].source_row, 2);
        assert_eq!(output.provenance[2].source_row, 3);
        assert_eq!(output.provenance[3].source_row, 4);
        assert_eq!(output.provenance[4].source_row, 5);
        assert_eq!(output.provenance[5].source_row, 6);

        // Provenance points to the Income column (col 1)
        assert!(
            output
                .provenance
                .iter()
                .all(|p| { p.source_addresses.len() == 1 && p.source_addresses[0].col == 1 })
        );
    }

    #[test]
    fn collision_show_time_selects_exact_time_column() {
        let csv = "\
ID,Time,Show Time
1,9:00,10:00
2,11:00,12:00
3,14:00,15:00
";
        let file = write_temp_csv(csv);

        // "Show time" → action show, exact column Time
        let result = run_pipeline(file.path(), "Show time");
        assert_eq!(result.outcome, CoreOutcome::Materialized);
        let intent = result.intent.as_ref().unwrap();
        assert_eq!(intent.column_display_name, "Time");
        assert_eq!(intent.column_id, "column-1");

        // Output is the Time column values
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
        assert_eq!(values, vec!["9:00", "11:00", "14:00"]);
    }

    #[test]
    fn collision_list_show_time_selects_show_time_column() {
        let csv = "\
ID,Time,Show Time
1,9:00,10:00
2,11:00,12:00
3,14:00,15:00
";
        let file = write_temp_csv(csv);

        // "List Show Time" → action list, exact column Show Time
        let result = run_pipeline(file.path(), "List Show Time");
        assert_eq!(result.outcome, CoreOutcome::Materialized);
        let intent = result.intent.as_ref().unwrap();
        assert_eq!(intent.column_display_name, "Show Time");
        assert_eq!(intent.column_id, "column-2");

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
        assert_eq!(values, vec!["10:00", "12:00", "15:00"]);
    }

    #[test]
    fn terminal_s_variant_in_pipeline() {
        let csv = "\
ID,Income
1,50000
2,60000
3,70000
";
        let file = write_temp_csv(csv);

        // "List incomes" → recognizes Income through terminal-s variant
        let result = run_pipeline(file.path(), "List incomes");
        assert_eq!(result.outcome, CoreOutcome::Materialized);

        let intent = result.intent.as_ref().unwrap();
        assert_eq!(intent.operation, crate::intent::CanonicalOperation::Select);
        assert_eq!(intent.column_display_name, "Income");

        let plan = result.plan.as_ref().unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(plan.steps[0], PlanStep::Select { .. }));

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
        assert_eq!(values, vec!["50000", "60000", "70000"]);
    }

    #[test]
    fn column_ambiguous_diagnostic_produces_failure() {
        // Two columns with the same normalized header cause a single column
        // span to map equally to multiple columns.
        let csv = "\
floor,Floor
1,A
2,B
3,C
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "list floor");

        assert_eq!(result.outcome, CoreOutcome::Failed);
        assert!(result.output.is_none());

        let diagnostic = result
            .diagnostics
            .iter()
            .find(|d| d.code == "intent.column_ambiguous")
            .expect("expected column_ambiguous diagnostic");
        assert_eq!(diagnostic.severity, Severity::Error);

        let evidence = result
            .intent_evidence
            .as_ref()
            .expect("column_ambiguous must surface refusal evidence");
        assert_eq!(
            evidence.refusal_reason.as_deref(),
            Some("intent.column_ambiguous")
        );
        let rows: Vec<(&str, f64)> = evidence
            .competing_parses
            .iter()
            .map(|c| (c.column_display_name.as_str(), c.score))
            .collect();
        assert!(
            rows.contains(&("floor", 1.0)),
            "competing parses must include both matched headers: {rows:?}"
        );
    }

    #[test]
    fn parse_ambiguous_diagnostic_produces_failure() {
        // "unique" can be read as the distinct modifier (span "income" ->
        // Income) or as the first word of the "Unique Income" header
        // (span "unique income"). Both parses score equally, so the tie is
        // between distinct complete parses, not between columns sharing a
        // header.
        let csv = "\
Unique Income,Income
1000,10
2000,20
3000,30
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "list unique income");

        assert_eq!(result.outcome, CoreOutcome::Failed);
        assert!(result.output.is_none());

        let diagnostic = result
            .diagnostics
            .iter()
            .find(|d| d.code == "intent.parse_ambiguous")
            .expect("expected parse_ambiguous diagnostic");
        assert_eq!(diagnostic.severity, Severity::Error);

        let evidence = result
            .intent_evidence
            .as_ref()
            .expect("parse_ambiguous must surface refusal evidence");
        assert_eq!(
            evidence.refusal_reason.as_deref(),
            Some("intent.parse_ambiguous")
        );
        assert_eq!(evidence.competing_parses.len(), 2);
    }

    #[test]
    fn intent_evidence_surfaced_on_success() {
        let csv = "\
ID,Name
1,Ada
2,Bob
3,Carol
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "List name");
        assert_eq!(result.outcome, CoreOutcome::Materialized);
        let evidence = result
            .intent_evidence
            .as_ref()
            .expect("success must surface recognition evidence");
        assert_eq!(evidence.refusal_reason, None);
        assert_eq!(evidence.canonical_operation.as_deref(), Some("select"));
        assert!(
            evidence.matched_column.as_ref().is_some(),
            "success evidence must carry the matched column"
        );
    }

    #[test]
    fn unsupported_intent_preserves_opening_events_and_refusal_evidence() {
        let csv = "ID,Name\n1,Ada\n2,Bob\n3,Carol\n";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "Calculate an average");

        assert_eq!(result.outcome, CoreOutcome::Failed);
        assert!(result.intent.is_none());
        assert!(result.plan.is_none());
        assert!(result.output.is_none());
        assert_eq!(
            result
                .events
                .iter()
                .map(|event| event.name.as_str())
                .collect::<Vec<_>>(),
            [
                "input_profiled",
                "table_candidates_detected",
                "table_candidate_selected",
                "header_selected",
                "body_rows_classified",
            ]
        );

        let evidence = result
            .intent_evidence
            .as_ref()
            .expect("refusal must retain recognition evidence");
        assert_eq!(
            evidence.refusal_reason.as_deref(),
            Some("intent.unsupported")
        );
        assert_eq!(
            evidence
                .prompt_tokens
                .iter()
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>(),
            ["calculate", "an", "average"]
        );
        assert!(evidence.action.is_none());
        assert!(evidence.matched_column.is_none());
        assert!(evidence.canonical_operation.is_none());
        assert!(evidence.competing_parses.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "intent.unsupported")
        );
    }

    #[test]
    fn filler_word_header_selects_in_pipeline() {
        // "value" is both a filler token and a complete header phrase; the
        // recognizer must treat it as the column span for `list value`.
        let csv = "\
ID,Value
1,A
2,B
";
        let file = write_temp_csv(csv);
        let result = run_pipeline(file.path(), "list value");

        assert_eq!(result.outcome, CoreOutcome::Materialized);
        let intent = result.intent.as_ref().unwrap();
        assert_eq!(intent.operation, crate::intent::CanonicalOperation::Select);
        assert_eq!(intent.column_display_name, "Value");

        let plan = result.plan.as_ref().unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(plan.steps[0], PlanStep::Select { .. }));

        let output = result.output.as_ref().unwrap();
        let values: Vec<&str> = output
            .rows
            .iter()
            .map(|r| match r.values.first().unwrap().as_ref().unwrap() {
                Value::Text(s) => s.as_str(),
                _ => panic!("expected text"),
            })
            .collect();
        assert_eq!(values, vec!["A", "B"]);
    }
}
