//! Domain model for baho.
//!
//! This crate owns source revisions, sheets, cells, coordinates, grid regions,
//! column definitions, values, provenance, and diagnostics. It has no I/O, CLI,
//! UI, or provider dependencies.

pub mod candidate;
pub mod column;
pub mod diagnostic;
pub mod document;
pub mod grid;
pub mod materialized;
pub mod revision;

pub use candidate::*;
pub use column::*;
pub use diagnostic::*;
pub use document::*;
pub use grid::*;
pub use materialized::*;
pub use revision::*;
