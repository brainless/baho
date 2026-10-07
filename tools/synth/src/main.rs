use std::{path::PathBuf, sync::Arc};
use synth::{
    dataset::LoadedDataset,
    generator::{Settings, credential, generate, neuralwatt_model},
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
    if args.next().as_deref() != Some("generate") {
        return Err("usage: synth generate --dataset CSV --feature MARKDOWN --count N [--output DIR] [--per-call-seconds N] [--whole-run-seconds N] [--max-turns N] [--variation TEXT]".into());
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
    let key = credential(std::env::var("NEURALWATT_API_KEY").ok())?;
    let feature = std::fs::read_to_string(feature.ok_or("--feature is required")?)?;
    let dataset = Arc::new(LoadedDataset::load(
        &dataset.ok_or("--dataset is required")?,
    )?);
    let result = generate(neuralwatt_model(key), dataset, &feature, settings).await?;
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
