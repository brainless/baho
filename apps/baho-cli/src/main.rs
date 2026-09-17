mod cli;
mod run_record;

use std::{env, process::ExitCode};

use clap::Parser;
use cli::{Cli, Command};

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
                eprintln!("Run {} recorded at {}", run.id, run.path.display());
                eprintln!("The request was captured; CSV processing is not implemented yet.");
                ExitCode::SUCCESS
            }
            Err(failure) => {
                if let Some(run) = &failure.run {
                    eprintln!("Run {} recorded at {}", run.id, run.path.display());
                }
                eprintln!("error: {:#}", failure.error);
                ExitCode::FAILURE
            }
        },
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
