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
    assert!(profile["encoding"].as_str().is_some());
    assert!(profile["logical_record_count"].as_u64().unwrap() > 0);

    let candidates: Value =
        serde_json::from_slice(&fs::read(run.join("candidates.json")).expect("read candidates"))
            .expect("valid candidates JSON");
    assert!(
        candidates["candidates"].as_array().unwrap().len() >= 1,
        "expected at least one candidate"
    );

    let plan: Value = serde_json::from_slice(&fs::read(run.join("plan.json")).expect("read plan"))
        .expect("valid plan JSON");
    let steps = plan["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0]["op"], "filter");
    assert_eq!(steps[1]["op"], "select");
    assert_eq!(steps[2]["op"], "distinct");

    let result: Value =
        serde_json::from_slice(&fs::read(run.join("output/result.json")).expect("read result"))
            .expect("valid result JSON");
    let rows = result["rows"].as_array().unwrap();
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
}
