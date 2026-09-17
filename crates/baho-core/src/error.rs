use thiserror::Error;

/// Top-level errors from core orchestration.
#[derive(Debug, Error)]
pub enum CoreError {
    #[error("ingestion failed: {0}")]
    Ingest(#[from] baho_ingest::ImportError),

    #[error("no table candidate found")]
    NoTableFound,

    #[error("ambiguous table selection: {candidate_count} candidates with similar scores")]
    AmbiguousTable { candidate_count: usize },

    #[error("intent recognition failed: {0}")]
    Intent(#[from] IntentError),

    #[error("plan validation failed: {0}")]
    Plan(#[from] baho_plan::validation::PlanValidationError),

    #[error("execution failed: {0}")]
    Execution(#[from] baho_exec::error::ExecutionError),
}

/// Errors from deterministic intent recognition.
#[derive(Debug, Error)]
pub enum IntentError {
    #[error("unsupported intent: {0}")]
    Unsupported(String),

    #[error("column not found for term '{prompt_term}'")]
    ColumnNotFound { prompt_term: String },

    #[error("ambiguous column match: {candidates:?}")]
    ColumnAmbiguous { candidates: Vec<String> },

    #[error("no operation recognized in prompt")]
    NoOperation,
}
