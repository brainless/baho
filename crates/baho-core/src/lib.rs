//! Application facade and orchestration.
//!
//! This crate coordinates ingestion, deterministic intent recognition,
//! candidate selection, planning, validation, execution, and artifact-ready
//! results. It does not own CLI formatting, akar/winit rendering, or
//! provider clients.

pub mod candidate_selection;
pub mod error;
pub mod grounding;
pub mod intent;
pub mod orchestration;

pub use baho_ingest_csv::{CandidateConfig, ParserConfig};
pub use candidate_selection::select_candidate;
pub use error::{CoreError, IntentError};
pub use grounding::*;
pub use intent::{
    CanonicalAction, CanonicalOperation, DeferredLiteralError, RecognizedIntent, RecognizedRequest,
    RowFilterIntent, compile_intent_to_plan, compile_request_to_plan, recognize_intent,
    recognize_request, resolve_deferred_literals,
};
pub use orchestration::{
    CoreEvent, CoreOutcome, CoreResult, OpenTableFailure, OpenedRow, OpenedTable, RawCell,
    dispatch_format, execute_prompt, execute_prompt_with_clarification, open_table,
    open_table_metadata, resolve_pipeline, run_pipeline,
};
