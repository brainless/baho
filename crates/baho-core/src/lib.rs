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

pub use candidate_selection::select_candidate;
pub use error::{CoreError, IntentError};
pub use intent::{RecognizedIntent, recognize_intent};
pub use orchestration::{CoreEvent, CoreOutcome, CoreResult, run_pipeline};
