//! Typed validation and deterministic execution/materialization.
//!
//! This crate owns execution errors, plan validation against a concrete table
//! schema, and deterministic filter/select/distinct materialization. It does
//! not own parsing of source formats, UI, or provider clients.

pub mod error;
pub mod executor;
pub mod validation;

pub use error::ExecutionError;
pub use executor::{ExecutionResult, GridInput, execute_plan};
pub use validation::validate_execution_context;
