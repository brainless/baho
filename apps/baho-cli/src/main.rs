mod cli;
mod run_record;

use std::{env, process::ExitCode};

use clap::Parser;
use cli::{Cli, Command};

/// A pending clarification is a distinct, scriptable outcome.
const CLARIFICATION_EXIT_STATUS: u8 = 2;

fn main() -> ExitCode {
    let arguments = env::args_os()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let cli = Cli::parse();

    match cli.command {
        Command::Run {
            input,
            prompt,
            output,
        } => match run_record::record(input, prompt, output, arguments) {
            Ok(run) => {
                if run.materialized {
                    eprintln!("Run {} materialized at {}", run.id, run.path.display());
                } else {
                    eprintln!("Run {} recorded at {}", run.id, run.path.display());
                }
                if run.needs_clarification {
                    ExitCode::from(CLARIFICATION_EXIT_STATUS)
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(failure) => {
                if let Some(run) = &failure.run {
                    eprintln!("Run {} recorded at {}", run.id, run.path.display());
                }
                eprintln!("error: {:#}", failure.error);
                ExitCode::FAILURE
            }
        },
        Command::Resolve { run_id, choices } => {
            match run_record::resolve(&run_id, &choices, arguments) {
                Ok(run) => {
                    if run.materialized {
                        eprintln!("Run {} materialized at {}", run.id, run.path.display());
                        ExitCode::SUCCESS
                    } else {
                        eprintln!("Run {} recorded at {}", run.id, run.path.display());
                        if run.needs_clarification {
                            ExitCode::from(CLARIFICATION_EXIT_STATUS)
                        } else {
                            ExitCode::FAILURE
                        }
                    }
                }
                Err(failure) => {
                    if let Some(run) = &failure.run {
                        eprintln!("Run {} recorded at {}", run.id, run.path.display());
                    }
                    eprintln!("error: {:#}", failure.error);
                    ExitCode::FAILURE
                }
            }
        }
        Command::Runs { latest } => match run_record::list(latest) {
            Ok(paths) => {
                for path in paths {
                    println!("{}", path.display());
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error:#}");
                ExitCode::FAILURE
            }
        },
    }
}
