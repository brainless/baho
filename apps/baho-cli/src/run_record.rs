use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use baho_core::{CoreOutcome, CoreResult};
use baho_model::document::Value;
use baho_run::{InputIdentity, Invocation, PendingRun, RecordedRun};

#[derive(Debug)]
pub(crate) struct RecordFailure {
    pub(crate) run: Option<RecordedRun>,
    pub(crate) error: anyhow::Error,
}

pub(crate) fn record(
    input: PathBuf,
    prompt: String,
    output: Option<PathBuf>,
    arguments: Vec<String>,
) -> std::result::Result<RecordedRun, RecordFailure> {
    let working_directory = std::env::current_dir()
        .context("could not read working directory")
        .map_err(|error| RecordFailure { run: None, error })?;
    let pending = PendingRun::reserve(
        &working_directory.join(".baho/runs"),
        Invocation {
            command: arguments
                .first()
                .cloned()
                .unwrap_or_else(|| "baho".to_owned()),
            action: "run".to_owned(),
            event_target: "baho_cli".to_owned(),
            arguments,
            working_directory: working_directory.clone(),
            output,
        },
        &prompt,
    )
    .map_err(|error| RecordFailure {
        run: None,
        error: error.into(),
    })?;
    let absolute_input = if input.is_absolute() {
        input.clone()
    } else {
        working_directory.join(&input)
    };
    let identity = match InputIdentity::inspect(&input, &absolute_input) {
        Ok(identity) => identity,
        Err(error) => {
            let message = error.to_string();
            return match pending.record_input_error(&message) {
                Ok(run) => Err(RecordFailure {
                    run: Some(run),
                    error: anyhow!(message),
                }),
                Err(record_error) => Err(RecordFailure {
                    run: None,
                    error: record_error.into(),
                }),
            };
        }
    };
    let result = baho_core::run_pipeline(&absolute_input, &prompt);
    let recorded = pending
        .record_result(identity, &result)
        .map_err(|error| RecordFailure {
            run: None,
            error: error.into(),
        })?;
    print_materialized(&result);
    if result.outcome == CoreOutcome::Materialized {
        Ok(recorded)
    } else {
        let message = result
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                matches!(diagnostic.severity, baho_model::diagnostic::Severity::Error)
            })
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        Err(RecordFailure {
            run: Some(recorded),
            error: anyhow!(message),
        })
    }
}

fn print_materialized(result: &CoreResult) {
    if result.outcome != CoreOutcome::Materialized {
        return;
    }
    if let Some(view) = &result.output {
        for row in &view.rows {
            for value in &row.values {
                match value {
                    Some(Value::Text(value)) => println!("{value}"),
                    Some(Value::Number(value)) => println!("{value}"),
                    Some(Value::Boolean(value)) => println!("{value}"),
                    Some(Value::Blank) | None => println!(),
                }
            }
        }
    }
}

pub(crate) fn list(latest: bool) -> Result<Vec<PathBuf>> {
    let working_directory = std::env::current_dir().context("could not read working directory")?;
    baho_run::list(
        &working_directory.join(".baho/runs"),
        &PathBuf::from(".baho/runs"),
        latest,
    )
    .map_err(Into::into)
}
