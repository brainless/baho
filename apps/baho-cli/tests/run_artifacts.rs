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
    assert!(event_names.contains(&"run_started".to_owned()));
    assert!(event_names.contains(&"input_identified".to_owned()));
    assert!(event_names.contains(&"input_profiled".to_owned()));
    assert!(event_names.contains(&"table_candidates_detected".to_owned()));
    assert!(event_names.contains(&"run_finished".to_owned()));
    assert!(!event_names.contains(&"processing_unavailable".to_owned()));

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
    assert_eq!(result["rows"].as_array().unwrap().len(), 1);
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
