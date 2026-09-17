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
    assert_eq!(plan["schema_version"], 3);
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
    assert_eq!(plan["schema_version"], 3);
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
    assert!(
        !artifacts.iter().any(|a| a == "output/result.json"),
        "artifact index must not list output/result.json"
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
