use std::{fs, process::Command, thread};

use serde_json::Value;
use tempfile::tempdir;

fn baho() -> Command {
    Command::new(env!("CARGO_BIN_EXE_baho"))
}

#[test]
fn run_records_the_request_and_input_identity() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "name,total\nAda,42\n").expect("write input");

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "Extract all the unique names",
        ])
        .output()
        .expect("run baho");

    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("Run 000001 materialized at"));

    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert_eq!(stdout.trim(), "Ada");

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
    assert_eq!(
        fs::read_to_string(run.join("intent.txt")).expect("read intent"),
        "Extract all the unique names"
    );

    let manifest: Value =
        serde_json::from_slice(&fs::read(run.join("manifest.json")).expect("read manifest"))
            .expect("valid manifest JSON");
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["run_id"], "000001");
    assert_eq!(manifest["outcome"], "materialized");
    assert_eq!(manifest["invocation"]["subcommand"], "run");
    assert_eq!(manifest["input"]["size_bytes"], 18);
    assert_eq!(
        manifest["input"]["sha256"],
        "7a600f38d8bcb8a94ebe3feeaa3a01ea38f1bc8a06cfd635b2042835c421f267"
    );

    let events = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    let event_names: Vec<_> = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid event JSON"))
        .map(|event| event["event"].as_str().unwrap().to_owned())
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
            "materialization_completed",
            "run_finished",
        ]
    );

    let diagnostics: Value =
        serde_json::from_slice(&fs::read(run.join("diagnostics.json")).expect("read diagnostics"))
            .expect("valid diagnostics JSON");
    let diag_codes: Vec<_> = diagnostics["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_owned())
        .collect();
    assert!(!diag_codes.contains(&"processing.not_implemented".to_owned()));

    let result: Value =
        serde_json::from_slice(&fs::read(run.join("output/result.json")).expect("read result"))
            .expect("valid result JSON");
    assert_eq!(result["schema_version"], 2);
    assert_eq!(result["result"]["rows"].as_array().unwrap().len(), 1);
    let provenance = &result["result"]["provenance"][0];
    assert!(provenance.get("source_address").is_none());
    assert_eq!(provenance["source_row"], 1);
    assert_eq!(
        provenance["source_addresses"],
        serde_json::json!([{"sheet_index": 0, "row": 1, "col": 0}])
    );

    let parser_config: Value = serde_json::from_slice(
        &fs::read(run.join("parser-config.json")).expect("read parser-config"),
    )
    .expect("valid parser-config JSON");
    assert_eq!(parser_config["schema_version"], 1);
    assert_eq!(parser_config["dialect"]["delimiter"], b',');
    assert_eq!(parser_config["dialect"]["quote"], b'"');
    assert_eq!(parser_config["inspection"]["max_sample_records"], 1000);
    assert_eq!(parser_config["inspection"]["max_field_size"], 1_048_576);
    assert!(parser_config["inspection"]["force_encoding"].is_null());
    assert_eq!(
        parser_config["candidate_detection"]["max_body_width_difference"],
        2
    );
    assert_eq!(parser_config["row_classification"]["min_data_density"], 0.3);
    assert_eq!(
        parser_config["candidate_scoring"]["weights"]["body_row_count"],
        0.32
    );
    assert_eq!(
        parser_config["normalization"]["header_whitespace"],
        "trim_and_collapse"
    );
    assert_eq!(
        parser_config["candidate_ordering"]["tie_breaker"],
        "source_order"
    );
    assert_eq!(
        parser_config["evidence_limits"]["max_blank_record_indices"],
        10_000
    );

    let candidates: Value =
        serde_json::from_slice(&fs::read(run.join("candidates.json")).expect("read candidates"))
            .expect("valid candidates JSON");
    assert_eq!(candidates["schema_version"], 1);
    let selected = candidates["selected"]
        .as_object()
        .expect("selected candidate");
    let header_cells = selected["header"]["cells"]
        .as_array()
        .expect("header cells");
    assert!(
        !header_cells.is_empty(),
        "selected candidate must have populated header cells"
    );
    let classifications = selected["body_row_classifications"]
        .as_array()
        .expect("classifications");
    assert!(
        !classifications.is_empty(),
        "selected candidate must have populated classifications"
    );

    let profile: Value =
        serde_json::from_slice(&fs::read(run.join("input-profile.json")).expect("read profile"))
            .expect("valid profile JSON");
    assert_eq!(profile["schema_version"], 1);
    assert_eq!(profile["profile"]["encoding"], "utf-8");

    let plan: Value = serde_json::from_slice(&fs::read(run.join("plan.json")).expect("read plan"))
        .expect("valid plan JSON");
    assert_eq!(plan["schema_version"], 4);
    assert_eq!(plan["plan"]["schema_version"], 1);
    assert_eq!(
        plan["recognition_evidence"]["canonical_operation"],
        "distinct"
    );
    assert_eq!(plan["recognition_evidence"]["action"]["alias"], "extract");
    assert_eq!(plan["recognition_evidence"]["modifier"]["alias"], "unique");
    assert_eq!(
        plan["recognition_evidence"]["matched_column"]["display_name"],
        "name"
    );

    assert_eq!(
        manifest["artifacts"],
        serde_json::json!([
            "manifest.json",
            "intent.txt",
            "events.jsonl",
            "diagnostics.json",
            "input-profile.json",
            "parser-config.json",
            "candidates.json",
            "plan.json",
            "output/result.json"
        ])
    );
}

#[test]
fn concurrent_runs_reserve_distinct_ids() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "name\nAda\n").expect("write input");

    thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let workspace = workspace.path();
                let input = &input;
                scope.spawn(move || {
                    baho()
                        .current_dir(workspace)
                        .args([
                            "run",
                            input.to_str().expect("UTF-8 path"),
                            "--prompt",
                            "Extract all the unique names",
                        ])
                        .output()
                        .expect("run baho")
                })
            })
            .collect();

        for handle in handles {
            let output = handle.join().expect("join invocation");
            assert!(output.status.success(), "{output:?}");
        }
    });

    let mut ids: Vec<_> = fs::read_dir(workspace.path().join(".baho/runs"))
        .expect("read runs")
        .map(|entry| entry.expect("read run entry").file_name())
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        [
            "000001", "000002", "000003", "000004", "000005", "000006", "000007", "000008"
        ]
    );
}

#[test]
fn failed_input_still_leaves_a_finalized_run() {
    let workspace = tempdir().expect("create temporary workspace");

    let output = baho()
        .current_dir(workspace.path())
        .args(["run", "missing.csv", "--prompt", "Read it"])
        .output()
        .expect("run baho");

    assert!(!output.status.success());
    let run = workspace.path().join(".baho/runs/000001");
    let manifest: Value =
        serde_json::from_slice(&fs::read(run.join("manifest.json")).expect("read manifest"))
            .expect("valid manifest JSON");
    assert_eq!(manifest["outcome"], "error");
    assert_eq!(manifest["error"]["code"], "input.unreadable");
    assert!(run.join("events.jsonl").is_file());
    assert!(run.join("diagnostics.json").is_file());
}

#[test]
fn runs_latest_reports_the_greatest_run_without_creating_one() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "name\nAda\n").expect("write input");

    for prompt in ["Extract all the unique names", "List all unique names"] {
        let status = baho()
            .current_dir(workspace.path())
            .args([
                "run",
                input.to_str().expect("UTF-8 path"),
                "--prompt",
                prompt,
            ])
            .status()
            .expect("run baho");
        assert!(status.success());
    }

    let output = baho()
        .current_dir(workspace.path())
        .args(["runs", "--latest"])
        .output()
        .expect("list runs");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        ".baho/runs/000002\n"
    );
    assert!(!workspace.path().join(".baho/runs/000003").exists());
}

#[test]
fn select_only_run_writes_correct_plan() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "ID,Name\n1,Ada\n2,Bob\n3,Carol\n").expect("write input");

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List name",
        ])
        .output()
        .expect("run baho");

    assert!(output.status.success(), "{output:?}");

    let run = workspace.path().join(".baho/runs/000001");

    // Plan artifact has schema_version 4
    let plan: Value = serde_json::from_slice(&fs::read(run.join("plan.json")).expect("read plan"))
        .expect("valid plan JSON");
    assert_eq!(plan["schema_version"], 4);

    // A retrieval request (`List <column>`) still emits a plan schema
    // version 1 plan inside the envelope, with no row-filter evidence.
    assert_eq!(plan["plan"]["schema_version"], 1);
    let recognition = &plan["recognition_evidence"];
    assert_eq!(recognition["schema_version"], 2);
    assert!(recognition["row_filter"].is_null());

    // Plan has exactly 1 step (select only)
    let steps = plan["plan"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["op"], "select");

    // Recognition evidence has canonical_operation: "select"
    let evidence = &plan["recognition_evidence"];
    assert_eq!(evidence["canonical_operation"], "select");

    // Output is materialized
    let result: Value =
        serde_json::from_slice(&fs::read(run.join("output/result.json")).expect("read result"))
            .expect("valid result JSON");
    assert_eq!(result["schema_version"], 2);
    let rows = result["result"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);

    // Stdout has the values
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines, vec!["Ada", "Bob", "Carol"]);
}

// ==== Epic 006 Task 7: row-filter run-artifact regressions ====
//
// Synthetic fixtures only (no user data): the motivating epic shape
// `Job = unemployed or Annual Income < 10000` over a four-column table.

fn read_artifact(run: &std::path::Path, name: &str) -> Value {
    let path = run.join(name);
    serde_json::from_slice(&fs::read(&path).unwrap_or_else(|error| {
        panic!("could not read {name}: {error}");
    }))
    .unwrap_or_else(|error| panic!("{name} must be valid JSON: {error}"))
}

fn artifact_diagnostic_codes(document: &Value) -> Vec<&str> {
    document["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .map(|d| d["code"].as_str().expect("stable diagnostic code"))
        .collect()
}

fn write_job_income_csv(workspace: &std::path::Path) -> std::path::PathBuf {
    let input = workspace.join("job_income.csv");
    fs::write(
        &input,
        "ID,Job,Annual Income,Note\n\
         1,unemployed,12000,full\n\
         2,teacher,9999.99,part\n\
         3,teacher,12000,long\n\
         4,,,\n\
         5,unemployed,,row5-note\n\
         6,unemployed\n",
    )
    .expect("write input");
    input
}

#[test]
fn row_filter_records_plan_v2_with_all_columns_and_provenance() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = write_job_income_csv(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List rows where Job = unemployed or Annual Income < 10000",
        ])
        .output()
        .expect("run baho");

    assert!(output.status.success(), "{output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    assert_eq!(
        fs::read_to_string(run.join("intent.txt")).expect("read intent"),
        "List rows where Job = unemployed or Annual Income < 10000"
    );

    let manifest = read_artifact(&run, "manifest.json");
    assert_eq!(manifest["outcome"], "materialized");

    // The plan.json envelope is schema version 4 and carries a schema
    // version 2 row-filter plan with a single Filter step.
    let plan = read_artifact(&run, "plan.json");
    assert_eq!(plan["schema_version"], 4);
    assert_eq!(plan["plan"]["schema_version"], 2);
    let steps = plan["plan"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["op"], "filter");

    // Recognition evidence is schema version 2 and records the row-filter
    // recognition decision, including the plan schema version it emitted.
    let recognition = &plan["recognition_evidence"];
    assert_eq!(recognition["schema_version"], 2);
    assert!(recognition["refusal_reason"].is_null());
    assert_eq!(recognition["canonical_operation"], "row_filter");
    let row_filter = &recognition["row_filter"];
    assert_eq!(row_filter["plan_schema_version"], 2);
    assert_eq!(row_filter["action"]["alias"], "list");
    let headers = row_filter["headers"].as_array().unwrap();
    let header_names: Vec<&str> = headers
        .iter()
        .map(|header| header["display_name"].as_str().unwrap())
        .collect();
    assert_eq!(header_names, ["Job", "Annual Income"]);
    assert_eq!(
        headers[0],
        serde_json::json!({
            "tokens": ["job"], "span": [3, 4],
            "column_id": "column-1", "display_name": "Job"
        })
    );
    assert_eq!(
        headers[1],
        serde_json::json!({
            "tokens": ["annual", "income"], "span": [7, 9],
            "column_id": "column-2", "display_name": "Annual Income"
        })
    );
    let literals = row_filter["literals"].as_array().unwrap();
    assert_eq!(literals[0]["raw_text"], "unemployed");
    assert_eq!(
        literals[0]["literal"],
        serde_json::json!({"text": "unemployed"})
    );
    assert!(literals[0]["parser_policy"].is_null());
    assert_eq!(literals[1]["raw_text"], "10000");
    assert_eq!(
        literals[1]["literal"],
        serde_json::json!({"decimal": "10000"})
    );
    assert_eq!(literals[1]["parser_policy"], "strict_decimal");
    let predicate = &row_filter["predicate"];
    assert_eq!(predicate["op"], "or");

    // A predicate-only row request retains all four source columns; only
    // rows whose predicate evaluated true are kept, in source order.
    let result = read_artifact(&run, "output/result.json");
    assert_eq!(result["schema_version"], 2);
    let columns = result["result"]["columns"].as_array().unwrap();
    let column_ids: Vec<&str> = columns
        .iter()
        .map(|column| column["id"].as_str().unwrap())
        .collect();
    assert_eq!(column_ids, ["column-0", "column-1", "column-2", "column-3"]);
    let rows = result["result"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    let provenance = result["result"]["provenance"].as_array().unwrap();
    let source_rows: Vec<u64> = provenance
        .iter()
        .map(|row| row["source_row"].as_u64().unwrap())
        .collect();
    assert_eq!(source_rows, [1, 2, 5, 6]);

    // Blank and missing raw evidence survive in the materialized cells, with
    // per-cell source coordinates aligned to the retained columns.
    let blank_row_values = rows[2]["values"].as_array().unwrap();
    assert_eq!(
        blank_row_values[2],
        serde_json::json!("Blank"),
        "blank Annual Income cell"
    );
    assert_eq!(
        blank_row_values[3],
        serde_json::json!({"Text": "row5-note"})
    );
    let ragged_row_values = rows[3]["values"].as_array().unwrap();
    assert!(
        ragged_row_values[2].is_null(),
        "missing cells materialize as absent values"
    );
    for row_index in 0..rows.len() {
        let values = rows[row_index]["values"].as_array().unwrap();
        let addresses = provenance[row_index]["source_addresses"]
            .as_array()
            .unwrap();
        assert_eq!(values.len(), 4, "all source columns are retained");
        assert_eq!(
            values.len(),
            addresses.len(),
            "row {row_index} provenance must be column-aligned"
        );
        assert_eq!(addresses[0]["col"], 0);
        assert_eq!(addresses[0]["sheet_index"], 0);
    }
    assert_eq!(
        provenance[0]["source_addresses"][1],
        serde_json::json!({"sheet_index": 0, "row": 1, "col": 1})
    );

    // The CLI prints every retained row's every cell in source order; blank
    // and missing cells print as empty lines.
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let printed = stdout
        .strip_suffix('\n')
        .expect("stdout ends with a newline");
    let lines: Vec<&str> = printed.split('\n').collect();
    assert_eq!(
        lines,
        [
            "1",
            "unemployed",
            "12000",
            "full", //
            "2",
            "teacher",
            "9999.99",
            "part", //
            "5",
            "unemployed",
            "",
            "row5-note", //
            "6",
            "unemployed",
            "",
            "",
        ]
    );

    // Events use stable names: the row-filter recognition, the observable
    // typed parse of compared columns, then materialization.
    let events_raw = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    let events: Vec<Value> = events_raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid event JSON"))
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
    assert_eq!(recognized["fields"]["fields"]["operation"], "row_filter");
    assert_eq!(recognized["fields"]["fields"]["action"], "list");
    assert_eq!(
        recognized["fields"]["fields"]["column_ids"],
        serde_json::json!(["column-1", "column-2"])
    );
    let parsed = events
        .iter()
        .find(|event| event["event"] == "compared_columns_parsed")
        .unwrap();
    assert_eq!(
        parsed["fields"]["fields"]["columns"],
        serde_json::json!(["column-1", "column-2"]),
        "every compared column is profiled"
    );
    assert_eq!(parsed["fields"]["fields"]["mixed"], serde_json::json!([]));
    assert_eq!(parsed["fields"]["fields"]["schema_version"], 2);
    assert_eq!(
        parsed["fields"]["fields"]["column_evidence"],
        serde_json::json!([
            {
                "column_id": "column-1",
                "column_ordinal": 1,
                "policy": "strict_decimal",
                "inferred_type": "text",
                "decimal_comparison_required": false,
                "verdict": { "status": "mixed", "reason": "no_parseable_values" },
                "counts": { "rows": 5, "missing": 0, "blank": 0, "valid": 0, "malformed": 5 }
            },
            {
                "column_id": "column-2",
                "column_ordinal": 2,
                "policy": "strict_decimal",
                "inferred_type": "numeric",
                "decimal_comparison_required": true,
                "verdict": { "status": "accepted" },
                "counts": { "rows": 5, "missing": 1, "blank": 1, "valid": 3, "malformed": 0 }
            }
        ])
    );

    // Structured events carry no full-document dumps of the source records.
    for record in [
        "1,unemployed,12000,full",
        "2,teacher,9999.99,part",
        "3,teacher,12000,long",
        "5,unemployed,,row5-note",
    ] {
        assert!(
            !events_raw.contains(record),
            "events must not dump the full document record {record:?}"
        );
    }
}

#[test]
fn mixed_compared_column_refusal_writes_parse_diagnostics() {
    // 2 valid and 1 malformed nonblank values: a 1/3 malformed share exceeds
    // the 10% limit, so the compared column refuses before execution.
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("mixed.csv");
    fs::write(&input, "ID,Annual Income\n1,500\n2,abc\n3,600\n").expect("write input");

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List rows where Annual Income < 1000",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains("exceeding the 10% limit"),
        "stderr: {stderr}"
    );

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics = read_artifact(&run, "diagnostics.json");
    let codes = artifact_diagnostic_codes(&diagnostics);
    assert!(
        codes.contains(&"parse.column_mixed"),
        "expected column_mixed diagnostic, got: {codes:?}"
    );
    assert!(
        codes.contains(&"parse.value_malformed"),
        "expected value_malformed diagnostic, got: {codes:?}"
    );
    let mixed = diagnostics["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"].as_str() == Some("parse.column_mixed"))
        .unwrap();
    assert_eq!(mixed["severity"], "Error");
    assert_eq!(mixed["stage"], "ingest-csv");
    assert_eq!(
        mixed["message"],
        "column 'column-1': 1 of 3 nonblank values are malformed, exceeding the 10% limit"
    );
    assert_eq!(
        mixed["location"]["col"], 1,
        "mixed-column refusal locates the compared column"
    );
    let malformed = diagnostics["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"].as_str() == Some("parse.value_malformed"))
        .unwrap();
    assert_eq!(malformed["location"]["cell"]["col"], 1);
    assert_eq!(
        malformed["location"]["cells"][0]["col"], 1,
        "structured sample cells survive into diagnostics.json"
    );

    // Recognition evidence and the refused plan stay inspectable in
    // plan.json, with the run stuck before execution.
    let plan = read_artifact(&run, "plan.json");
    assert_eq!(plan["schema_version"], 4);
    assert_eq!(plan["plan"]["schema_version"], 2);
    assert_eq!(plan["plan"]["steps"][0]["op"], "filter");
    let recognition = &plan["recognition_evidence"];
    assert_eq!(recognition["schema_version"], 2);
    assert!(recognition["refusal_reason"].is_null());
    assert_eq!(recognition["row_filter"]["plan_schema_version"], 2);

    // No output is materialized; the refusal is visible in the manifest and
    // the output artifact is absent from the artifact index.
    assert!(!run.join("output/result.json").exists());
    let manifest = read_artifact(&run, "manifest.json");
    assert_eq!(manifest["outcome"], "error");
    let artifacts: Vec<&str> = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| artifact.as_str().unwrap())
        .collect();
    assert_eq!(
        artifacts,
        [
            "manifest.json",
            "intent.txt",
            "events.jsonl",
            "diagnostics.json",
            "input-profile.json",
            "parser-config.json",
            "candidates.json",
            "plan.json"
        ]
    );

    // The structured parse event follows plan validation and no
    // materialization event appears after a refusal.
    let events: Vec<Value> = fs::read_to_string(run.join("events.jsonl"))
        .expect("read events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid event JSON"))
        .collect();
    let event_names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        &event_names[event_names.len() - 3..],
        ["plan_validated", "compared_columns_parsed", "run_finished"]
    );
    assert!(
        !event_names.contains(&"materialization_completed"),
        "refusal must abort before materialization"
    );
    let parsed = events
        .iter()
        .find(|event| event["event"] == "compared_columns_parsed")
        .unwrap();
    assert_eq!(
        parsed["fields"]["fields"]["mixed"],
        serde_json::json!(["column-1"])
    );
    assert_eq!(parsed["fields"]["fields"]["schema_version"], 2);
    assert_eq!(
        parsed["fields"]["fields"]["column_evidence"],
        serde_json::json!([{
            "column_id": "column-1",
            "column_ordinal": 1,
            "policy": "strict_decimal",
            "inferred_type": "mixed",
            "decimal_comparison_required": true,
            "verdict": { "status": "mixed", "reason": "malformed_share_exceeded" },
            "counts": { "rows": 3, "missing": 0, "blank": 0, "valid": 2, "malformed": 1 }
        }])
    );
}

#[test]
fn text_literal_against_numeric_column_writes_plan_type_mismatch() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = write_job_income_csv(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List rows where Annual Income = \"500\"",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics = read_artifact(&run, "diagnostics.json");
    let codes = artifact_diagnostic_codes(&diagnostics);
    assert!(
        codes.contains(&"plan.type_mismatch"),
        "expected plan.type_mismatch, got: {codes:?}"
    );
    assert!(
        !codes.contains(&"execution.failed"),
        "a type mismatch is a plan diagnostic, not an execution failure"
    );
    let mismatch = diagnostics["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"].as_str() == Some("plan.type_mismatch"))
        .unwrap();
    assert_eq!(mismatch["severity"], "Error");

    // The validated plan stays inspectable; nothing is materialized.
    assert!(!run.join("output/result.json").exists());
    let plan = read_artifact(&run, "plan.json");
    assert_eq!(plan["schema_version"], 4);
    assert_eq!(plan["plan"]["schema_version"], 2);

    let events_raw = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    assert!(events_raw.contains("compared_columns_parsed"));
    assert!(!events_raw.contains("materialization_completed"));
}

#[test]
fn compact_grouped_literal_refuses_with_literal_invalid_in_artifacts() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = write_job_income_csv(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List job = 10,000",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics = read_artifact(&run, "diagnostics.json");
    let codes = artifact_diagnostic_codes(&diagnostics);
    assert!(
        codes.contains(&"intent.literal_invalid"),
        "expected literal_invalid diagnostic, got: {codes:?}"
    );
    assert!(
        !codes.contains(&"intent.column_not_found"),
        "a predicate-shaped compact prompt must not fall back to retrieval: {codes:?}"
    );

    let plan = read_artifact(&run, "plan.json");
    assert_eq!(plan["schema_version"], 4);
    assert_eq!(
        plan["recognition_evidence"]["refusal_reason"],
        "intent.literal_invalid"
    );
    assert!(
        plan.get("plan").is_none(),
        "refusal must not write a nested plan"
    );
    assert!(!run.join("output/result.json").exists());

    let events_raw = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    assert!(!events_raw.contains("compared_columns_parsed"));
    assert!(!events_raw.contains("materialization_completed"));
}

#[test]
fn duplicate_header_row_filter_refuses_with_column_ambiguous() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = workspace.path().join("duplicate.csv");
    fs::write(&input, "ID,Job,job\n1,unemployed,x\n2,teacher,y\n").expect("write input");

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List rows where Job = unemployed",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics = read_artifact(&run, "diagnostics.json");
    let codes = artifact_diagnostic_codes(&diagnostics);
    assert!(
        codes.contains(&"intent.column_ambiguous"),
        "expected column_ambiguous diagnostic, got: {codes:?}"
    );
    assert!(
        !codes.iter().any(|code| code.starts_with("parse.")),
        "recognition refusals precede typed parsing"
    );

    let plan = read_artifact(&run, "plan.json");
    assert_eq!(plan["schema_version"], 4);
    assert!(
        plan.get("plan").is_none(),
        "refusal must not write a nested plan"
    );
    let recognition = &plan["recognition_evidence"];
    assert_eq!(recognition["refusal_reason"], "intent.column_ambiguous");
    let competing: Vec<&str> = recognition["competing_parses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|parse| parse["column_display_name"].as_str().unwrap())
        .collect();
    assert!(
        competing.contains(&"Job"),
        "competing parses must name the ambiguous header: {competing:?}"
    );

    assert!(!run.join("output/result.json").exists());
    let events_raw = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    assert!(!events_raw.contains("compared_columns_parsed"));
    assert!(!events_raw.contains("materialization_completed"));
}

#[test]
fn missing_column_row_filter_refuses_with_column_not_found() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = write_job_income_csv(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List rows where Salary > 1000",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics = read_artifact(&run, "diagnostics.json");
    let codes = artifact_diagnostic_codes(&diagnostics);
    assert!(
        codes.contains(&"intent.column_not_found"),
        "expected column_not_found diagnostic, got: {codes:?}"
    );

    let plan = read_artifact(&run, "plan.json");
    let recognition = &plan["recognition_evidence"];
    assert_eq!(recognition["refusal_reason"], "intent.column_not_found");
    assert!(plan.get("plan").is_none());
    assert!(!run.join("output/result.json").exists());

    let events_raw = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    assert!(!events_raw.contains("compared_columns_parsed"));
    assert!(!events_raw.contains("materialization_completed"));
}

#[test]
fn grouped_literal_row_filter_refuses_with_literal_invalid() {
    let workspace = tempdir().expect("create temporary workspace");
    let input = write_job_income_csv(workspace.path());

    let output = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().expect("UTF-8 path"),
            "--prompt",
            "List rows where Annual Income < 10,000",
        ])
        .output()
        .expect("run baho");

    assert!(!output.status.success(), "expected failure: {output:?}");

    let run = workspace.path().join(".baho/runs/000001");
    let diagnostics = read_artifact(&run, "diagnostics.json");
    let codes = artifact_diagnostic_codes(&diagnostics);
    assert!(
        codes.contains(&"intent.literal_invalid"),
        "expected literal_invalid diagnostic, got: {codes:?}"
    );
    assert!(
        !codes.iter().any(|code| code.starts_with("parse.")),
        "literal refusals precede typed parsing"
    );

    let plan = read_artifact(&run, "plan.json");
    let recognition = &plan["recognition_evidence"];
    assert_eq!(recognition["refusal_reason"], "intent.literal_invalid");
    assert!(plan.get("plan").is_none());
    assert!(!run.join("output/result.json").exists());

    let events_raw = fs::read_to_string(run.join("events.jsonl")).expect("read events");
    assert!(!events_raw.contains("compared_columns_parsed"));
    assert!(!events_raw.contains("materialization_completed"));
}
