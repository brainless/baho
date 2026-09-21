use std::{fs, thread};

use baho_run::{InputIdentity, Invocation, PendingRun};
use serde_json::Value;
use tempfile::tempdir;

fn invocation(workspace: &std::path::Path, action: &str) -> Invocation {
    Invocation {
        command: "baho-gui".to_owned(),
        action: action.to_owned(),
        event_target: "baho_gui".to_owned(),
        arguments: vec!["baho-gui".to_owned(), "sample.csv".to_owned()],
        working_directory: workspace.to_owned(),
        output: None,
    }
}

#[test]
fn opened_snapshot_can_be_recorded_without_rehashing_changed_disk_bytes() {
    let workspace = tempdir().expect("temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "name\nAda\n").expect("write source");
    let opened = baho_core::open_table(&input).expect("open source snapshot");
    let identity = InputIdentity::from_snapshot(&input, &input, &opened.source_revision);
    let original_hash = opened.source_revision.content_hash.clone();
    fs::write(&input, "name\nChanged\n").expect("change source after opening");

    let prompt = "  List name\n";
    let pending = PendingRun::reserve(
        &workspace.path().join(".baho/runs"),
        invocation(workspace.path(), "submit"),
        prompt,
    )
    .expect("reserve run");
    let result = baho_core::execute_prompt(&opened, prompt);
    let recorded = pending
        .record_result(identity, &result)
        .expect("record result");

    assert_eq!(recorded.id, "000001");
    let run = workspace.path().join(".baho/runs/000001");
    assert_eq!(fs::read_to_string(run.join("intent.txt")).unwrap(), prompt);
    let manifest: Value =
        serde_json::from_slice(&fs::read(run.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["invocation"]["command"], "baho-gui");
    assert_eq!(manifest["invocation"]["subcommand"], "submit");
    assert_eq!(manifest["input"]["sha256"], original_hash);
    assert!(run.join("output/result.json").is_file());
}

#[test]
fn refused_result_is_finalized_without_materialized_output() {
    let workspace = tempdir().expect("temporary workspace");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "name\nAda\n").expect("write source");
    let opened = baho_core::open_table(&input).expect("open source snapshot");
    let pending = PendingRun::reserve(
        &workspace.path().join(".baho/runs"),
        invocation(workspace.path(), "submit"),
        "Calculate an average",
    )
    .expect("reserve run");
    let result = baho_core::execute_prompt(&opened, "Calculate an average");
    pending
        .record_result(
            InputIdentity::from_snapshot(&input, &input, &opened.source_revision),
            &result,
        )
        .expect("record refusal");

    let run = workspace.path().join(".baho/runs/000001");
    let manifest: Value =
        serde_json::from_slice(&fs::read(run.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["outcome"], "error");
    assert!(run.join("plan.json").is_file());
    assert!(!run.join("output/result.json").exists());
}

#[test]
fn concurrent_shared_reservations_are_unique() {
    let workspace = tempdir().expect("temporary workspace");
    thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let workspace = workspace.path();
                scope.spawn(move || {
                    PendingRun::reserve(
                        &workspace.join(".baho/runs"),
                        invocation(workspace, "submit"),
                        "List name",
                    )
                    .expect("reserve run")
                    .record_input_error("synthetic failure")
                    .expect("finalize run")
                    .id
                })
            })
            .collect();
        let mut ids: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                "000001", "000002", "000003", "000004", "000005", "000006", "000007", "000008"
            ]
        );
    });
}
