use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use baho_core::{
    ClarificationChoice, ClarificationRequest, ClarificationResponse, CoreOutcome, CoreResult,
    GroundingOutcome, PredicateForm,
};
use baho_model::document::Value;
use baho_run::{InputIdentity, Invocation, PendingRun, RecordedRun, load_pending_clarification};

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
    finish_result(&result, recorded)
}

pub(crate) fn resolve(
    run_id: &str,
    choices: &[String],
    arguments: Vec<String>,
) -> std::result::Result<RecordedRun, RecordFailure> {
    let working_directory = std::env::current_dir()
        .context("could not read working directory")
        .map_err(|error| RecordFailure { run: None, error })?;
    let runs = working_directory.join(".baho/runs");
    let original = load_pending_clarification(&runs, run_id).map_err(|error| RecordFailure {
        run: None,
        error: error.into(),
    })?;
    let selections = choices
        .iter()
        .map(|choice| {
            let (clause_id, candidate_id) = choice
                .split_once('=')
                .ok_or_else(|| anyhow!("choice must have the form clause_id=candidate_id"))?;
            if clause_id.is_empty() || candidate_id.is_empty() {
                return Err(anyhow!("choice must have the form clause_id=candidate_id"));
            }
            Ok(ClarificationChoice {
                clause_id: clause_id.into(),
                selected_candidate_id: candidate_id.into(),
            })
        })
        .collect::<Result<Vec<_>>>()
        .map_err(|error| RecordFailure { run: None, error })?;
    let response = ClarificationResponse {
        schema_version: original.request.schema_version,
        request_id: original.request.request_id.clone(),
        choices: selections,
    };
    let pending = PendingRun::reserve(
        &runs,
        Invocation {
            command: arguments
                .first()
                .cloned()
                .unwrap_or_else(|| "baho".to_owned()),
            action: "resolve".into(),
            event_target: "baho_cli".into(),
            arguments,
            working_directory,
            output: None,
        },
        &original.prompt,
    )
    .and_then(|pending| pending.link_resolution(&original, response.clone()))
    .map_err(|error| RecordFailure {
        run: None,
        error: error.into(),
    })?;
    let identity = match InputIdentity::inspect(&original.input_path, &original.input_path) {
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
    let result = baho_core::resolve_pipeline(
        &original.input_path,
        &original.prompt,
        &original.request,
        &response,
    );
    let recorded = pending
        .record_result(identity, &result)
        .map_err(|error| RecordFailure {
            run: None,
            error: error.into(),
        })?;
    finish_result(&result, recorded)
}

fn finish_result(
    result: &CoreResult,
    recorded: RecordedRun,
) -> std::result::Result<RecordedRun, RecordFailure> {
    print_materialized(result);
    if let Some(baho_core::GroundingResult {
        outcome: GroundingOutcome::NeedsClarification { request },
        ..
    }) = &result.grounding
    {
        print_clarification(request, &recorded.id);
        return Ok(recorded);
    }
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

fn print_clarification(request: &ClarificationRequest, run_id: &str) {
    eprintln!("Clarification required (exit status 2):");
    for clause in &request.unresolved {
        eprintln!("  {}: {}", clause.clause_id, clause.rendered_condition);
        for candidate in &clause.candidates {
            let form = match candidate.predicate_form {
                PredicateForm::EqualsValue => "equals value",
                PredicateForm::FlagIsTrue => "flag is true",
                PredicateForm::IsNotBlank => "is not blank",
                PredicateForm::Compare => "compare",
            };
            eprintln!(
                "    {}  {}  {}  {}",
                candidate.candidate_id, candidate.column_id, candidate.display_name, form
            );
        }
    }
    eprint!("Resolve with: baho resolve {run_id}");
    for clause in &request.unresolved {
        eprint!(" {}=<candidate_id>", clause.clause_id);
    }
    eprintln!();
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
