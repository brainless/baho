use std::{path::PathBuf, sync::Arc, time::Duration};
use synth::repair_progress::{RepairProgress, Verbosity};
use synth::{
    cli_run::RunLimits,
    dataset::LoadedDataset,
    evaluate::{EvaluateOptions, evaluate},
    generator::{
        REPAIR_MODEL, Settings, credential, env_file_value, generate, neuralwatt_model,
        neuralwatt_repair_model, resolve_credential,
    },
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
        Some("repair") => return repair_command(args),
        _ => return Err("usage: synth repair --evaluation EVALUATION_JSON --case CASE_ID --baseline COMMIT --dataset CSV --review FILE [--output DIR] [--verbose | --quiet]\n       synth evaluate --dataset CSV --baho-bin PATH [--generation DIR] [--review FILE] [--output DIR]\n       synth generate --dataset CSV --feature MARKDOWN --count N [--output DIR] [--per-call-seconds N] [--whole-run-seconds N] [--max-turns N] [--variation TEXT]".into()),
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
    let mut verbosity = Verbosity::Normal;
    while let Some(flag) = args.next() {
        if matches!(flag.as_str(), "--verbose" | "-v" | "--quiet" | "-q") {
            verbosity = match flag.as_str() {
                "--verbose" | "-v" => Verbosity::Verbose,
                _ => Verbosity::Quiet,
            };
            continue;
        }
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
    let progress = RepairProgress::new(verbosity, |line| println!("{line}"));
    progress.message("repair: validating intake");
    synth::repair_entry::prepare(&options)?;
    progress.message("repair: loading provider credential");
    let key = resolve_credential(
        std::env::var("NEURALWATT_API_KEY").ok(),
        std::fs::read_to_string(repo.join(".env")).ok().as_deref(),
    )?;
    let (model, retries) = neuralwatt_repair_model(
        key,
        REPAIR_MODEL,
        options.settings.api_request_timeout_seconds,
    )?;
    let (outcome, path) = synth::repair_entry::repair_with_progress_and_retries(
        model.into(),
        &options,
        Some(progress),
        retries,
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
