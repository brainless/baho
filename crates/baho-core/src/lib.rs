//! Application facade and orchestration.
//!
//! This crate coordinates ingestion, deterministic intent recognition,
//! candidate selection, planning, validation, execution, and artifact-ready
//! results. It does not own CLI formatting, akar/winit rendering, or
//! provider clients.

pub mod candidate_selection;
pub mod error;
pub mod intent;
pub mod orchestration;

pub use baho_ingest_csv::{CandidateConfig, ParserConfig};
pub use candidate_selection::select_candidate;
pub use error::{CoreError, IntentError};
pub use intent::{
    CanonicalAction, CanonicalOperation, RecognizedIntent, compile_intent_to_plan, recognize_intent,
};
pub use orchestration::{CoreEvent, CoreOutcome, CoreResult, run_pipeline};
