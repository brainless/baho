use baho_plan::evidence::RecognitionEvidence;
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
///
/// Each variant may carry bounded refusal evidence describing why recognition
/// failed. The evidence is always present when the error comes from the
/// recognizer; it stays optional so callers constructing the error manually do
/// not have to build one.
#[derive(Debug, Error)]
pub enum IntentError {
    #[error("unsupported intent: {0}")]
    Unsupported(String, Option<RecognitionEvidence>),

    #[error("column not found for term '{prompt_term}'")]
    ColumnNotFound {
        prompt_term: String,
        evidence: Option<RecognitionEvidence>,
    },

    #[error("ambiguous column match: {candidates:?}")]
    ColumnAmbiguous {
        candidates: Vec<String>,
        evidence: Option<RecognitionEvidence>,
    },

    #[error("ambiguous parse: {candidates:?}")]
    ParseAmbiguous {
        candidates: Vec<String>,
        evidence: Option<RecognitionEvidence>,
    },
}
