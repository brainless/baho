//! Versioned plan and expression IR with structural validation.
//!
//! This crate owns the serializable operation-plan types, expression tree,
//! structural and reference validation, and recognition evidence. It does not
//! own LLM calls, arbitrary code execution, or materialization.

pub mod evidence;
pub mod plan;
pub mod validation;

pub use evidence::*;
pub use plan::*;
pub use validation::*;
