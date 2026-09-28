use std::{fs, process::Command};

use serde_json::Value;
use tempfile::tempdir;

fn baho() -> Command {
    Command::new(env!("CARGO_BIN_EXE_baho"))
}

fn json(path: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn noninteractive_clarification_can_be_resolved_in_a_linked_run() {
    let workspace = tempdir().unwrap();
    let input = workspace.path().join("sample.csv");
    fs::write(
        &input,
        "ID,Job,Note,Amount\n1,unemployed,other,5\n2,employed,unemployed,20\n3,employed,other,1\n",
    )
    .unwrap();
    let run = baho()
        .current_dir(workspace.path())
        .args([
            "run",
            input.to_str().unwrap(),
            "--prompt",
            "List rows where unemployed and < 10",
        ])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(2), "{run:?}");
    assert!(run.stdout.is_empty());
    let stderr = String::from_utf8(run.stderr).unwrap();
    assert!(stderr.contains("Clarification required (exit status 2)"));
    assert!(stderr.contains("baho resolve 000001"));
    let runs = workspace.path().join(".baho/runs");
    let original_manifest = fs::read(runs.join("000001/manifest.json")).unwrap();
    let grounding = json(&runs.join("000001/grounding.json"));
    let request = &grounding["outcome"]["request"];
    let clauses = request["unresolved"].as_array().unwrap();
    assert_eq!(clauses.len(), 2);
    let choices: Vec<String> = clauses
        .iter()
        .map(|clause| {
            format!(
                "{}={}",
                clause["clause_id"].as_str().unwrap(),
                clause["candidates"][0]["candidate_id"].as_str().unwrap()
            )
        })
        .collect();
    for choice in &choices {
        assert!(stderr.contains(choice.split('=').next().unwrap()));
        assert!(stderr.contains(choice.split('=').nth(1).unwrap()));
    }
    let accepted = baho()
        .current_dir(workspace.path())
        .args(["resolve", "000001", &choices[0], &choices[1]])
        .output()
        .unwrap();
    assert!(accepted.status.success(), "{accepted:?}");
    assert!(runs.join("000002/output/result.json").exists());
    let linked = json(&runs.join("000002/manifest.json"));
    assert_eq!(linked["resumes_run_id"], "000001");
    assert_eq!(linked["clarification_request_id"], request["request_id"]);
    assert_eq!(linked["invocation"]["subcommand"], "resolve");
    assert_eq!(
        fs::read(runs.join("000001/manifest.json")).unwrap(),
        original_manifest
    );

    let invalid = baho()
        .current_dir(workspace.path())
        .args([
            "resolve",
            "000001",
            &format!(
                "{}=candidate-999",
                clauses[0]["clause_id"].as_str().unwrap()
            ),
            &choices[1],
        ])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert_eq!(json(&runs.join("000003/manifest.json"))["outcome"], "error");
    assert!(!runs.join("000003/output/result.json").exists());
    assert_eq!(
        json(&runs.join("000003/diagnostics.json"))["diagnostics"][0]["code"],
        "grounding.invalid_clarification_response"
    );

    let incomplete = baho()
        .current_dir(workspace.path())
        .args(["resolve", "000001", &choices[0]])
        .output()
        .unwrap();
    assert_eq!(incomplete.status.code(), Some(1));
    assert_eq!(json(&runs.join("000004/manifest.json"))["outcome"], "error");
    assert!(!runs.join("000004/output/result.json").exists());

    fs::write(&input, "ID,Job,Note,Amount\n1,unemployed,other,6\n").unwrap();
    let stale = baho()
        .current_dir(workspace.path())
        .args(["resolve", "000001", &choices[0], &choices[1]])
        .output()
        .unwrap();
    assert_eq!(stale.status.code(), Some(1));
    assert_eq!(json(&runs.join("000005/manifest.json"))["outcome"], "error");
    assert_eq!(
        json(&runs.join("000005/diagnostics.json"))["diagnostics"][0]["code"],
        "grounding.stale_clarification"
    );
    assert!(!runs.join("000005/output/result.json").exists());
    assert_eq!(
        fs::read(runs.join("000001/manifest.json")).unwrap(),
        original_manifest
    );
}
