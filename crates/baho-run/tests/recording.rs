use std::{fs, thread};

use baho_core::{ClarificationChoice, ClarificationResponse};
use baho_run::{InputIdentity, Invocation, PendingRun, load_pending_clarification};
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

#[test]
fn clarification_is_versioned_and_resolution_links_a_new_immutable_run() {
    let workspace = tempdir().unwrap();
    let input = workspace.path().join("sample.csv");
    fs::write(
        &input,
        "ID,Job,Note,Amount\n1,unemployed,other,5\n2,employed,unemployed,20\n3,employed,other,1\n",
    )
    .unwrap();
    let prompt = "List rows where unemployed and < 10";
    let runs = workspace.path().join(".baho/runs");
    let pending = PendingRun::reserve(&runs, invocation(workspace.path(), "run"), prompt).unwrap();
    let original_result = baho_core::run_pipeline(&input, prompt);
    let original = pending
        .record_result(
            InputIdentity::inspect(&input, &input).unwrap(),
            &original_result,
        )
        .unwrap();
    assert!(original.needs_clarification);
    assert!(!original.materialized);
    let original_manifest: Value =
        serde_json::from_slice(&fs::read(runs.join("000001/manifest.json")).unwrap()).unwrap();
    assert_eq!(original_manifest["schema_version"], 2);
    assert_eq!(original_manifest["outcome"], "needs_clarification");
    assert!(original_manifest["clarification_request_id"].is_string());
    assert!(
        original_manifest["artifacts"]
            .as_array()
            .unwrap()
            .contains(&Value::from("grounding.json"))
    );
    assert!(!runs.join("000001/output/result.json").exists());
    let saved = load_pending_clarification(&runs, &original.id).unwrap();
    assert_eq!(saved.prompt, prompt);
    assert_eq!(saved.input_path, input);
    let grounding: Value =
        serde_json::from_slice(&fs::read(runs.join("000001/grounding.json")).unwrap()).unwrap();
    assert_eq!(grounding["schema_version"], 2);
    assert_eq!(grounding["outcome"]["status"], "needs_clarification");
    let response = ClarificationResponse {
        schema_version: saved.request.schema_version,
        request_id: saved.request.request_id.clone(),
        choices: saved
            .request
            .unresolved
            .iter()
            .map(|clause| ClarificationChoice {
                clause_id: clause.clause_id.clone(),
                selected_candidate_id: clause.candidates[0].candidate_id.clone(),
            })
            .collect(),
    };
    let before = fs::read(runs.join("000001/manifest.json")).unwrap();
    let resumed_result =
        baho_core::resolve_pipeline(&saved.input_path, &saved.prompt, &saved.request, &response);
    let resumed = PendingRun::reserve(&runs, invocation(workspace.path(), "resolve"), prompt)
        .unwrap()
        .link_resolution(&saved, response.clone())
        .unwrap()
        .record_result(
            InputIdentity::inspect(&input, &input).unwrap(),
            &resumed_result,
        )
        .unwrap();
    assert_eq!(resumed.id, "000002");
    assert!(resumed.materialized);
    assert_eq!(fs::read(runs.join("000001/manifest.json")).unwrap(), before);
    let manifest: Value =
        serde_json::from_slice(&fs::read(runs.join("000002/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["resumes_run_id"], "000001");
    assert_eq!(manifest["clarification_request_id"], response.request_id);
    assert!(
        manifest["artifacts"]
            .as_array()
            .unwrap()
            .contains(&Value::from("clarification-response.json"))
    );
    assert!(runs.join("000002/output/result.json").is_file());
}

#[test]
fn pending_loader_rejects_tampered_prompt_and_parser_configuration() {
    let workspace = tempdir().unwrap();
    let input = workspace.path().join("sample.csv");
    fs::write(
        &input,
        "ID,Job,Note\n1,unemployed,other\n2,employed,unemployed\n",
    )
    .unwrap();
    let prompt = "List rows where unemployed";
    let runs = workspace.path().join(".baho/runs");
    let result = baho_core::run_pipeline(&input, prompt);
    PendingRun::reserve(&runs, invocation(workspace.path(), "run"), prompt)
        .unwrap()
        .record_result(InputIdentity::inspect(&input, &input).unwrap(), &result)
        .unwrap();
    let directory = runs.join("000001");
    assert!(load_pending_clarification(&runs, "000001").is_ok());
    let grounding_path = directory.join("grounding.json");
    let mut grounding: Value = serde_json::from_slice(&fs::read(&grounding_path).unwrap()).unwrap();
    grounding["schema_version"] = Value::from(1);
    fs::write(&grounding_path, serde_json::to_vec(&grounding).unwrap()).unwrap();
    assert!(load_pending_clarification(&runs, "000001").is_err());
    grounding["schema_version"] = Value::from(2);
    fs::write(&grounding_path, serde_json::to_vec(&grounding).unwrap()).unwrap();
    fs::write(directory.join("intent.txt"), "changed prompt").unwrap();
    assert!(load_pending_clarification(&runs, "000001").is_err());
    fs::write(directory.join("intent.txt"), prompt).unwrap();
    let config_path = directory.join("parser-config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["dialect"]["delimiter"] = Value::from(59);
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert!(load_pending_clarification(&runs, "000001").is_err());
}

#[test]
fn historical_and_nonpending_runs_cannot_be_loaded_as_clarifications() {
    let workspace = tempdir().unwrap();
    let runs = workspace.path().join(".baho/runs");
    let input = workspace.path().join("sample.csv");
    fs::write(&input, "name\nAda\n").unwrap();
    let result = baho_core::run_pipeline(&input, "List name");
    PendingRun::reserve(&runs, invocation(workspace.path(), "run"), "List name")
        .unwrap()
        .record_result(InputIdentity::inspect(&input, &input).unwrap(), &result)
        .unwrap();
    assert!(load_pending_clarification(&runs, "000001").is_err());
    assert!(load_pending_clarification(&runs, "../000001").is_err());
    let manifest_path = runs.join("000001/manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["schema_version"] = Value::from(1);
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(load_pending_clarification(&runs, "000001").is_err());
}
