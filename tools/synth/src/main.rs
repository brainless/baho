use std::{path::PathBuf, sync::Arc, time::Duration};
use synth::{
    cli_run::RunLimits,
    dataset::LoadedDataset,
    evaluate::{EvaluateOptions, evaluate},
    generator::{Settings, credential, env_file_value, generate, neuralwatt_model},
    planner::{PlanOptions, PlanSettings, plan_epic, resolve_credential},
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("synth: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("generate") => {}
        Some("evaluate") => return evaluate_command(args),
        Some("plan-epic") => return plan_epic_command(args).await,
        Some("repair") => return repair_command(args),
        _ => return Err("usage: synth plan-epic --evaluation EVALUATION_JSON [--context NAME] [--proposal PROPOSAL_JSON] [--reasoning-effort low|medium|high] [--max-turns N] [--max-tool-calls N] [--stage-max-turns N] [--stage-max-tool-calls N] [--per-call-seconds N] [--whole-run-seconds N]\n       synth evaluate --dataset CSV --baho-bin PATH [--generation DIR] [--review FILE] [--output DIR]\n       synth generate --dataset CSV --feature MARKDOWN --count N [--output DIR] [--per-call-seconds N] [--whole-run-seconds N] [--max-turns N] [--variation TEXT]".into()),
    }
    let mut dataset = None;
    let mut feature = None;
    let mut count = None;
    let mut output = PathBuf::from("synthetic/out");
    let mut per_call = None;
    let mut whole_run = None;
    let mut max_turns = None;
    let mut variation = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("option requires a value")?;
        match flag.as_str() {
            "--dataset" => dataset = Some(PathBuf::from(value)),
            "--feature" => feature = Some(PathBuf::from(value)),
            "--count" => count = Some(value.parse::<usize>()?),
            "--output" => output = PathBuf::from(value),
            "--per-call-seconds" => per_call = Some(value.parse::<u64>()?),
            "--whole-run-seconds" => whole_run = Some(value.parse::<u64>()?),
            "--max-turns" => max_turns = Some(value.parse::<usize>()?),
            "--variation" => variation = Some(value),
            _ => return Err("unknown option".into()),
        }
    }
    let mut settings = Settings::new(count.ok_or("--count is required")?)?;
    if let Some(v) = per_call {
        settings.per_call_seconds = v;
    }
    if let Some(v) = whole_run {
        settings.whole_run_seconds = v;
    }
    if let Some(v) = max_turns {
        settings.max_turns = v;
    }
    if let Some(v) = variation {
        settings.variation = v;
    }
    settings.validate()?;
    // The process environment wins; otherwise fall back to the repository root `.env`,
    // located from this package's manifest rather than the current directory.
    let key = credential(std::env::var("NEURALWATT_API_KEY").ok().or_else(|| {
        let root_env = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
        env_file_value(
            &std::fs::read_to_string(root_env).ok()?,
            "NEURALWATT_API_KEY",
        )
    }))?;
    let feature = std::fs::read_to_string(feature.ok_or("--feature is required")?)?;
    let dataset = Arc::new(LoadedDataset::load(
        &dataset.ok_or("--dataset is required")?,
    )?);
    let result = generate(
        neuralwatt_model(key, synth::generator::MODEL),
        dataset,
        &feature,
        settings,
    )
    .await?;
    result.write(&output)?;
    println!(
        "{}: {} accepted ({})",
        result.record.id, result.record.accepted, result.record.outcome
    );
    for call in &result.record.calls {
        if call.standard_tier || call.served_tier == "unknown" {
            eprintln!(
                "turn {}: served tier {} (requested flex)",
                call.turn, call.served_tier
            );
        }
    }
    Ok(())
}

fn repair_command(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut evaluation = None;
    let mut case_id = None;
    let mut baseline_revision = None;
    let mut dataset = None;
    let mut review = None;
    let mut output = PathBuf::from("synthetic/out/repair");
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("option requires a value")?;
        match flag.as_str() {
            "--evaluation" => evaluation = Some(PathBuf::from(value)),
            "--case" => case_id = Some(value),
            "--baseline" => baseline_revision = Some(value),
            "--dataset" => dataset = Some(PathBuf::from(value)),
            "--review" => review = Some(PathBuf::from(value)),
            "--output" => output = PathBuf::from(value),
            _ => return Err("unknown repair option".into()),
        }
    }
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let options = synth::repair_entry::RepairOptions {
        repo: repo.clone(),
        evaluation: evaluation.ok_or("--evaluation is required")?,
        case_id: case_id.ok_or("--case is required")?,
        baseline_revision: baseline_revision.ok_or("--baseline is required")?,
        dataset: dataset.ok_or("--dataset is required")?,
        review: review.ok_or("--review is required")?,
        output,
        settings: synth::repair_entry::RepairOptions::default_settings(),
    };
    // Intake runs before credential loading, so invalid or unreviewed cases
    // cannot trigger a provider call.
    synth::repair_entry::prepare(&options)?;
    let key = resolve_credential(
        std::env::var("NEURALWATT_API_KEY").ok(),
        std::fs::read_to_string(repo.join(".env")).ok().as_deref(),
    )?;
    let (outcome, path) = synth::repair_entry::repair(
        neuralwatt_model(key, synth::planner::MODEL).into(),
        &options,
    )?;
    println!("{:?}: {}", outcome, path.display());
    let manifest =
        synth::repair::AttemptManifest::parse(&std::fs::read(path.join("manifest.json"))?)?;
    if let Some(reason) = manifest.reason {
        println!("reason: {reason}");
    }
    Ok(())
}

fn evaluate_command(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut dataset = None;
    let mut generation = None;
    let mut baho_bin = None;
    let mut review = None;
    let mut output = PathBuf::from("synthetic/out");
    while let Some(flag) = args.next() {
        let value = PathBuf::from(args.next().ok_or("option requires a value")?);
        match flag.as_str() {
            "--dataset" => dataset = Some(value),
            "--generation" => generation = Some(value),
            "--baho-bin" => baho_bin = Some(value),
            "--review" => review = Some(value),
            "--output" => output = value,
            _ => return Err("unknown option".into()),
        }
    }
    let outcome = evaluate(&EvaluateOptions {
        dataset: dataset.ok_or("--dataset is required")?,
        generation,
        baho_bin: baho_bin.ok_or("--baho-bin is required")?,
        review,
        output,
        repo: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        limits: RunLimits::default(),
        oracle_timeout: Duration::from_secs(30),
    })?;
    let a = &outcome.report.aggregates;
    println!(
        "{}: {} cases, {} materialized, {} rows_match, {} confirmed wrong-plan",
        outcome.report.evaluation_id, a.cases, a.materialized, a.rows_match, a.confirmed_wrong_plan
    );
    println!("report: {}", outcome.report_path.display());
    println!("evaluation hash: {}", outcome.evaluation_hash);
    if !outcome.report.limitations.is_empty() {
        eprintln!(
            "{} mapping limitation(s); see the report before drawing conclusions",
            outcome.report.limitations.len()
        );
    }
    outcome.require_started_replay()?;
    Ok(())
}

async fn plan_epic_command(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut evaluation = None;
    let mut settings = PlanSettings::default();
    let mut context = None;
    let mut proposal = None;
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("option requires a value")?;
        match flag.as_str() {
            "--evaluation" => evaluation = Some(PathBuf::from(value)),
            "--context" => context = Some(value),
            "--proposal" => proposal = Some(PathBuf::from(value)),
            "--reasoning-effort" => settings.reasoning_effort = value.parse()?,
            "--max-turns" => settings.max_turns = value.parse()?,
            "--max-tool-calls" => settings.limits.max_calls = value.parse()?,
            "--stage-max-turns" => {
                let cap = value.parse()?;
                settings.stage_budgets.investigation.max_turns = cap;
                settings.stage_budgets.draft.max_turns = cap;
                settings.stage_budgets.review.max_turns = cap;
            }
            "--stage-max-tool-calls" => {
                let cap = value.parse()?;
                settings.stage_budgets.investigation.max_tool_calls = cap;
                settings.stage_budgets.draft.max_tool_calls = cap;
                settings.stage_budgets.review.max_tool_calls = cap;
            }
            "--per-call-seconds" => settings.per_call_seconds = value.parse()?,
            "--whole-run-seconds" => settings.whole_run_seconds = value.parse()?,
            _ => return Err("unknown option".into()),
        }
    }
    settings.validate()?;
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let evaluation = evaluation.ok_or("--evaluation is required")?;
    // The sidecar is checked inside `plan_epic`, but the key is resolved only once it is
    // known the run can start, so a missing key reports before any provider traffic.
    let key = resolve_credential(
        std::env::var("NEURALWATT_API_KEY").ok(),
        std::fs::read_to_string(repo.join(".env")).ok().as_deref(),
    )?;
    let result = plan_epic(
        neuralwatt_model(key, synth::planner::MODEL),
        &PlanOptions {
            evaluation,
            repo,
            settings,
            context,
            proposal,
        },
    )
    .await?;
    let r = &result.record;
    println!("{}: {}", r.id, r.outcome);
    if let Some(path) = &r.epic_path {
        println!("epic: {path}");
    }
    if let Some(why) = &r.explanation {
        println!("no epic: {why}");
    }
    if let Some(excerpt) = &r.final_text_excerpt {
        let label = if r.outcome == "malformed_tool_call" {
            "malformed tool call (final text excerpt)"
        } else {
            "no terminal action (final text excerpt)"
        };
        println!("{label}: {excerpt}");
    }
    if let Some(path) = &r.proposal_path {
        println!("proposal: {path}");
    }
    println!("record: {}", result.record_path.display());
    for call in &r.calls {
        if !matches!(call.served_tier.as_str(), "standard" | "default") {
            eprintln!(
                "{} turn {}: served tier {} (requested {})",
                call.stage, call.turn, call.served_tier, call.requested_tier
            );
        }
    }
    Ok(())
}
