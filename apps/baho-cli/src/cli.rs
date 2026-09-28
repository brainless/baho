use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "baho",
    version,
    about = "Turn document requests into reproducible runs"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Record work requested for an input document.
    Run {
        /// Document to operate on.
        input: PathBuf,

        /// Plain-language description of the desired result.
        #[arg(long)]
        prompt: String,

        /// Destination for a future materialized result.
        #[arg(long)]
        output: Option<PathBuf>,
    },

    /// List previously recorded runs without creating a new one.
    Runs {
        /// Show only the greatest allocated run ID.
        #[arg(long)]
        latest: bool,
    },

    /// Resolve a pending clarification with clause-to-candidate choices.
    Resolve {
        /// Run ID containing the pending clarification.
        run_id: String,

        /// Selections in the form clause_id=candidate_id.
        #[arg(required = true)]
        choices: Vec<String>,
    },
}
