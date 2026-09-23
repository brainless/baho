use std::{fs, path::PathBuf, process::Command};

use serde_json::Value;
use tempfile::tempdir;

fn baho() -> Command {
    Command::new(env!("CARGO_BIN_EXE_baho"))
}

fn fixture_path() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("../../tests/fixtures/report_with_preamble.csv")
}

fn copy_fixture(workspace: &std::path::Path) -> PathBuf {
    let dest = workspace.join("report_with_preamble.csv");
    fs::copy(fixture_path(), &dest).expect("copy fixture");
    dest
}

#[test]
fn extract_unique_floor_plans_from_fixture() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = copy_fixture(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "Extract all the unique floor plans",
        ])
        .output()
        .expect("run baho");

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("Run 000001 materialized at"),
        "stderr: {stderr}"
    );

    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines, vec!["Type A", "Type B", "Type C"]);

    let run = workspace.path().join(".baho/runs/000001");
    for artifact in [
        "manifest.json",
        "intent.txt",
        "events.jsonl",
        "diagnostics.json",
        "input-profile.json",
        "parser-config.json",
        "candidates.json",
        "plan.json",
        "output/result.json",
    ] {
        assert!(run.join(artifact).is_file(), "missing {artifact}");
    }

    let manifest: Value =
        serde_json::from_slice(&fs::read(run.join("manifest.json")).expect("read manifest"))
            .expect("valid manifest JSON");
    assert_eq!(manifest["outcome"], "materialized");

    assert_eq!(
        fs::read_to_string(run.join("intent.txt")).expect("read intent"),
        "Extract all the unique floor plans"
    );

    let events = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    let event_names: Vec<String> = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid event JSON"))
        .map(|event| event["event"].as_str().unwrap().to_owned())
        .collect();
    assert!(event_names.contains(&"input_profiled".to_owned()));
    assert!(event_names.contains(&"table_candidates_detected".to_owned()));
    assert!(event_names.contains(&"table_candidate_selected".to_owned()));
    assert!(event_names.contains(&"header_selected".to_owned()));
    assert!(event_names.contains(&"body_rows_classified".to_owned()));
    assert!(event_names.contains(&"intent_recognized".to_owned()));
    assert!(event_names.contains(&"plan_validated".to_owned()));
    assert!(event_names.contains(&"materialization_completed".to_owned()));
    assert!(event_names.contains(&"run_finished".to_owned()));

    let diagnostics: Value =
        serde_json::from_slice(&fs::read(run.join("diagnostics.json")).expect("read diagnostics"))
            .expect("valid diagnostics JSON");
    let diag_array = diagnostics["diagnostics"].as_array().unwrap();
    let error_codes: Vec<&str> = diag_array
        .iter()
        .filter(|d| d["severity"].as_str() == Some("error"))
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(
        error_codes.is_empty(),
        "unexpected error diagnostics: {error_codes:?}"
    );

    let profile: Value =
        serde_json::from_slice(&fs::read(run.join("input-profile.json")).expect("read profile"))
            .expect("valid profile JSON");
    assert_eq!(profile["schema_version"], 1);
    assert!(profile["profile"]["encoding"].as_str().is_some());
    assert!(profile["profile"]["logical_record_count"].as_u64().unwrap() > 0);

    let candidates: Value =
        serde_json::from_slice(&fs::read(run.join("candidates.json")).expect("read candidates"))
            .expect("valid candidates JSON");
    assert_eq!(candidates["schema_version"], 1);
    assert!(
        candidates["candidates"].as_array().unwrap().len() >= 1,
        "expected at least one candidate"
    );

    let plan: Value = serde_json::from_slice(&fs::read(run.join("plan.json")).expect("read plan"))
        .expect("valid plan JSON");
    assert_eq!(plan["schema_version"], 4);
    let steps = plan["plan"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0]["op"], "filter");
    assert_eq!(steps[1]["op"], "select");
    assert_eq!(steps[2]["op"], "distinct");

    let evidence = &plan["recognition_evidence"];
    assert!(
        evidence["prompt_tokens"].as_array().is_some(),
        "plan must include recognition evidence"
    );
    assert!(
        evidence["matched_column"].as_object().is_some(),
        "plan must include matched column evidence"
    );
    assert!(
        evidence["action"].as_object().is_some(),
        "plan must include action evidence"
    );
    assert!(
        evidence["canonical_operation"].as_str().is_some(),
        "plan must include canonical operation"
    );

    let result: Value =
        serde_json::from_slice(&fs::read(run.join("output/result.json")).expect("read result"))
            .expect("valid result JSON");
    assert_eq!(result["schema_version"], 2);
    let rows = result["result"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
}

#[test]
fn unsupported_intent_returns_failure() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = copy_fixture(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "Calculate the average floor plan size",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("unsupported intent"), "stderr: {stderr}");

    let run = workspace.path().join(".baho/runs/000001");
    assert!(run.join("manifest.json").is_file(), "missing manifest");

    let manifest: Value =
        serde_json::from_slice(&fs::read(run.join("manifest.json")).expect("read manifest"))
            .expect("valid manifest JSON");
    assert_eq!(manifest["outcome"], "error");

    assert_eq!(
        fs::read_to_string(run.join("intent.txt")).expect("read intent"),
        "Calculate the average floor plan size"
    );

    // Refusal persists bounded recognition evidence in plan.json with no plan.
    let plan: Value = serde_json::from_slice(&fs::read(run.join("plan.json")).expect("read plan"))
        .expect("valid plan JSON");
    assert_eq!(plan["schema_version"], 4);
    assert!(
        plan.get("plan").is_none(),
        "refusal must not write a nested plan, got {:?}",
        plan.get("plan")
    );
    let evidence = &plan["recognition_evidence"];
    assert_eq!(evidence["refusal_reason"], "intent.unsupported");
    assert_eq!(
        evidence["competing_parses"].as_array().unwrap().len(),
        0,
        "unsupported must not carry competing parses"
    );
    assert!(evidence["canonical_operation"].is_null());
    assert!(evidence["matched_column"].is_null());
    assert!(
        evidence["prompt_tokens"].as_array().is_some(),
        "refusal evidence must retain bounded prompt tokens"
    );

    // No misleading materialized output on refusal.
    assert!(
        !run.join("output/result.json").exists(),
        "refusal must not write output/result.json"
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(run.join("manifest.json")).expect("read manifest"))
            .expect("valid manifest JSON");
    let artifacts = manifest["artifacts"].as_array().unwrap();
    let expected_artifacts = serde_json::json!([
        "manifest.json",
        "intent.txt",
        "events.jsonl",
        "diagnostics.json",
        "input-profile.json",
        "parser-config.json",
        "candidates.json",
        "plan.json"
    ]);
    assert_eq!(
        artifacts,
        expected_artifacts
            .as_array()
            .expect("expected artifact array")
    );

    let event_names = fs::read_to_string(run.join("events.jsonl"))
        .expect("read events")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid event JSON"))
        .map(|event| event["event"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        event_names,
        [
            "run_started",
            "input_identified",
            "input_profiled",
            "table_candidates_detected",
            "table_candidate_selected",
            "header_selected",
            "body_rows_classified",
            "run_finished",
        ]
    );
}

#[test]
fn column_not_found_returns_failure() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = copy_fixture(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "Extract all the unique zones",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("column not found"), "stderr: {stderr}");

    let run = workspace.path().join(".baho/runs/000001");
    assert!(run.join("diagnostics.json").is_file());

    let diagnostics: Value =
        serde_json::from_slice(&fs::read(run.join("diagnostics.json")).expect("read diagnostics"))
            .expect("valid diagnostics JSON");
    let diag_codes: Vec<&str> = diagnostics["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(
        diag_codes.contains(&"intent.column_not_found"),
        "expected column_not_found diagnostic, got: {diag_codes:?}"
    );

    let plan: Value = serde_json::from_slice(&fs::read(run.join("plan.json")).expect("read plan"))
        .expect("valid plan JSON");
    let evidence = &plan["recognition_evidence"];
    assert_eq!(evidence["refusal_reason"], "intent.column_not_found");
    assert!(
        plan.get("plan").is_none(),
        "column_not_found refusal must not write a nested plan"
    );
    assert!(!run.join("output/result.json").exists());
}

#[test]
fn ambiguous_parse_writes_competing_parse_evidence() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "Unique Income,Income\n1000,10\n2000,20\n3000,30\n").expect("write input");

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "list unique income",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics: Value =
        serde_json::from_slice(&fs::read(run.join("diagnostics.json")).expect("read diagnostics"))
            .expect("valid diagnostics JSON");
    let diag_codes: Vec<&str> = diagnostics["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(
        diag_codes.contains(&"intent.parse_ambiguous"),
        "expected parse_ambiguous diagnostic, got: {diag_codes:?}"
    );

    let plan: Value = serde_json::from_slice(&fs::read(run.join("plan.json")).expect("read plan"))
        .expect("valid plan JSON");
    let evidence = &plan["recognition_evidence"];
    assert_eq!(evidence["refusal_reason"], "intent.parse_ambiguous");
    let competing = evidence["competing_parses"].as_array().unwrap();
    let rows: Vec<(String, f64)> = competing
        .iter()
        .map(|c| {
            (
                c["column_display_name"].as_str().unwrap().to_owned(),
                c["score"].as_f64().unwrap(),
            )
        })
        .collect();
    assert!(
        rows.contains(&("Unique Income".to_string(), 1.0)),
        "competing parses must include Unique Income: {rows:?}"
    );
    assert!(
        rows.contains(&("Income".to_string(), 1.0)),
        "competing parses must include Income: {rows:?}"
    );
    let income_row = competing
        .iter()
        .find(|c| c["column_display_name"].as_str() == Some("Income"))
        .expect("competing parses must include Income");
    assert_eq!(income_row["column_span"], serde_json::json!([2, 3]));
    assert_eq!(income_row["modifier"], "unique");
    let unique_income_row = competing
        .iter()
        .find(|c| c["column_display_name"].as_str() == Some("Unique Income"))
        .expect("competing parses must include Unique Income");
    assert_eq!(unique_income_row["column_span"], serde_json::json!([1, 3]));
    assert!(unique_income_row["modifier"].is_null());
    assert!(plan.get("plan").is_none());
    assert!(!run.join("output/result.json").exists());
}

// ==== Epic 006 Task 7: row-filter end-to-end and run-artifact regressions ====

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(path).expect("read artifact"))
        .unwrap_or_else(|error| panic!("{path:?} must be valid JSON: {error}"))
}

fn diagnostic_codes(document: &Value) -> Vec<&str> {
    document["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| d["code"].as_str().expect("stable diagnostic code"))
        .collect()
}

#[test]
fn row_filter_materializes_all_columns_from_fixture() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = copy_fixture(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List rows where Status = Inactive or Year = 2022",
        ])
        .output()
        .expect("run baho");

    assert!(output.status.success(), "{output:?}");

    let run = workspace.path().join(".baho/runs/000001");

    // Envelope 4 carries a schema version 2 row-filter plan.
    let plan = read_json(&run.join("plan.json"));
    assert_eq!(plan["schema_version"], 4);
    assert_eq!(plan["plan"]["schema_version"], 2);
    let steps = plan["plan"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["op"], "filter");

    // Recognition evidence has its own schema version 2 and the row-filter
    // decision with the plan schema version it emitted.
    let evidence = &plan["recognition_evidence"];
    assert_eq!(evidence["schema_version"], 2);
    assert!(evidence["refusal_reason"].is_null());
    assert_eq!(evidence["canonical_operation"], "row_filter");
    let row_filter = &evidence["row_filter"];
    assert_eq!(row_filter["plan_schema_version"], 2);
    let headers = row_filter["headers"].as_array().unwrap();
    let header_names: Vec<&str> = headers
        .iter()
        .map(|h| h["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(header_names, ["Status", "Year"]);
    let header_ids: Vec<&str> = headers
        .iter()
        .map(|h| h["column_id"].as_str().unwrap())
        .collect();
    assert_eq!(header_ids, ["column-3", "column-6"]);
    let predicate = &row_filter["predicate"];
    assert_eq!(predicate["op"], "or");
    assert_eq!(predicate["predicates"][0]["op"], "compare");
    assert_eq!(predicate["predicates"][0]["column"], "column-3");
    assert_eq!(
        predicate["predicates"][0]["literal"],
        serde_json::json!({"text": "Inactive"})
    );
    assert_eq!(predicate["predicates"][1]["op"], "compare");
    assert_eq!(predicate["predicates"][1]["column"], "column-6");
    assert_eq!(
        predicate["predicates"][1]["literal"],
        serde_json::json!({"decimal": "2022"})
    );

    // A predicate-only output retains every source column in source order;
    // only rows whose predicate evaluated true are kept, in source order.
    let result = read_json(&run.join("output/result.json"));
    assert_eq!(result["schema_version"], 2);
    let columns = result["result"]["columns"].as_array().unwrap();
    let named_column_names: Vec<&str> = columns[..8]
        .iter()
        .map(|column| column["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(
        named_column_names,
        [
            "ID",
            "Floor Plan",
            "Color",
            "Status",
            "Unit",
            "Size",
            "Year",
            "Notes"
        ]
    );
    let rows = result["result"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let provenance = result["result"]["provenance"].as_array().unwrap();
    let source_rows: Vec<u64> = provenance
        .iter()
        .map(|row| row["source_row"].as_u64().unwrap())
        .collect();
    assert_eq!(source_rows, [9, 10, 14]);

    // Row and cell provenance stay aligned with the retained columns.
    for row_index in 0..rows.len() {
        let values = rows[row_index]["values"].as_array().unwrap();
        let addresses = provenance[row_index]["source_addresses"]
            .as_array()
            .unwrap();
        assert_eq!(
            values.len(),
            addresses.len(),
            "row {row_index} provenance must be column-aligned"
        );
    }
    let inactive_row_values = rows[0]["values"].as_array().unwrap();
    assert_eq!(inactive_row_values[0], serde_json::json!({"Text": "4"}));
    assert_eq!(
        inactive_row_values[3],
        serde_json::json!({"Text": "Inactive"})
    );
    assert_eq!(inactive_row_values[6], serde_json::json!({"Text": "2019"}));
    let active_row_values = rows[1]["values"].as_array().unwrap();
    assert_eq!(active_row_values[3], serde_json::json!({"Text": "Active"}));
    assert_eq!(active_row_values[6], serde_json::json!({"Text": "2022"}));
    let address = &provenance[0]["source_addresses"][3];
    assert_eq!(
        address,
        &serde_json::json!({"sheet_index": 0, "row": 9, "col": 3})
    );

    // Error diagnostics stay empty; only warnings (unnamed padded columns)
    // may appear.
    let diagnostics = read_json(&run.join("diagnostics.json"));
    let error_codes: Vec<&str> = diagnostics["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["severity"].as_str() == Some("error"))
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(error_codes.is_empty(), "unexpected errors: {error_codes:?}");

    // The structured row-filter events appear with stable names and fields.
    let events: Vec<Value> = fs::read_to_string(run.join("events.jsonl"))
        .expect("read events")
        .lines()
        .map(|line| serde_json::from_str(&line).expect("valid event JSON"))
        .collect();
    let event_names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        event_names,
        [
            "run_started",
            "input_identified",
            "input_profiled",
            "table_candidates_detected",
            "table_candidate_selected",
            "header_selected",
            "body_rows_classified",
            "intent_recognized",
            "plan_validated",
            "compared_columns_parsed",
            "materialization_completed",
            "run_finished",
        ]
    );
    let recognized = events
        .iter()
        .find(|event| event["event"] == "intent_recognized")
        .unwrap();
    assert_eq!(recognized["fields"]["fields"]["action"], "list");
    assert_eq!(recognized["fields"]["fields"]["operation"], "row_filter");
    assert_eq!(
        recognized["fields"]["fields"]["column_ids"],
        serde_json::json!(["column-3", "column-6"])
    );
    let parsed = events
        .iter()
        .find(|event| event["event"] == "compared_columns_parsed")
        .unwrap();
    assert_eq!(
        parsed["fields"]["fields"]["columns"],
        serde_json::json!(["column-6"])
    );
    assert_eq!(parsed["fields"]["fields"]["mixed"], serde_json::json!([]));

    // Events carry no full-document dumps of the fixture contents.
    let events_raw = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    for record in [
        "1,Type A,Red,Active,101,850,2020,note1",
        "2,Type B,Blue,Active,102,900,2021,note2",
    ] {
        assert!(
            !events_raw.contains(record),
            "events must not dump the full document record {record:?}"
        );
    }
}

#[test]
fn condensed_row_filter_matches_canonical_from_fixture() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = copy_fixture(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List status Inactive or year = 2022",
        ])
        .output()
        .expect("run baho");

    assert!(output.status.success(), "{output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let plan = read_json(&run.join("plan.json"));
    assert_eq!(plan["schema_version"], 4);
    assert_eq!(plan["plan"]["schema_version"], 2);
    let evidence = &plan["recognition_evidence"];
    assert_eq!(evidence["schema_version"], 2);
    assert_eq!(evidence["canonical_operation"], "row_filter");
    let row_filter = &evidence["row_filter"];
    assert_eq!(row_filter["plan_schema_version"], 2);
    // The condensed form binds the implicit-equality header directly.
    let headers = row_filter["headers"].as_array().unwrap();
    assert_eq!(headers[0]["tokens"], serde_json::json!(["status"]));
    assert_eq!(headers[0]["display_name"], "Status");
    assert_eq!(headers[1]["display_name"], "Year");

    let result = read_json(&run.join("output/result.json"));
    let source_rows: Vec<u64> = result["result"]["provenance"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["source_row"].as_u64().unwrap())
        .collect();
    assert_eq!(source_rows, [9, 10, 14]);
}

#[test]
fn overlapping_row_filter_headers_refuse_as_parse_ambiguous() {
    // "Annual" and "Annual Income" share their first token with different
    // phrase lengths, so `Annual Income` survives more than one complete
    // parse and recognition refuses with a deterministic ambiguity
    // diagnostic instead of choosing a column.
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "ID,Annual,Annual Income\n1,5,2\n2,6,3\n").expect("write input");

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List rows where Annual Income < 5",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics = read_json(&run.join("diagnostics.json"));
    let codes = diagnostic_codes(&diagnostics);
    assert!(
        codes.contains(&"intent.parse_ambiguous"),
        "expected parse_ambiguous diagnostic, got: {codes:?}"
    );

    let plan = read_json(&run.join("plan.json"));
    assert_eq!(plan["schema_version"], 4);
    assert!(
        plan.get("plan").is_none(),
        "refusal must not write a nested plan"
    );
    let evidence = &plan["recognition_evidence"];
    assert_eq!(evidence["schema_version"], 2);
    assert_eq!(evidence["refusal_reason"], "intent.parse_ambiguous");
    let competing: Vec<&str> = evidence["competing_parses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|parse| parse["column_display_name"].as_str().unwrap())
        .collect();
    assert_eq!(competing, ["Annual Income", "Annual"]);
    assert!(
        !run.join("output/result.json").exists(),
        "refusal must not materialize output"
    );
}

#[test]
fn expression_depth_limit_refuses_as_predicate_unsupported() {
    // The recognizer enforces the predicate depth limit before any plan is
    // compiled, so the CLI can only observe intent.predicate_unsupported;
    // plan.expression_limit_exceeded is a plan-level code for plans that
    // were not produced by recognition.
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "ID,Job\n1,unemployed\n2,teacher\n").expect("write input");

    let mut prompt = String::from("List rows where ");
    for _ in 0..9 {
        prompt.push_str("not ( ");
    }
    prompt.push_str("Job = 1");
    for _ in 0..9 {
        prompt.push_str(" )");
    }

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            &prompt,
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics = read_json(&run.join("diagnostics.json"));
    let codes = diagnostic_codes(&diagnostics);
    assert!(
        codes.contains(&"intent.predicate_unsupported"),
        "expected predicate_unsupported diagnostic, got: {codes:?}"
    );
    let plan = read_json(&run.join("plan.json"));
    assert!(plan.get("plan").is_none());
    assert!(
        diagnostic_codes(&read_json(&run.join("diagnostics.json")))
            .iter()
            .all(|code| !code.starts_with("parse."))
    );
    assert!(!run.join("output/result.json").exists());
}
