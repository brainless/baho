//! Domain model for baho.
//!
//! This crate owns source revisions, sheets, cells, coordinates, grid regions,
//! column definitions, values, provenance, and diagnostics. It has no I/O, CLI,
//! UI, or provider dependencies.

pub mod candidate;
pub mod column;
pub mod decimal;
pub mod diagnostic;
pub mod document;
pub mod grid;
pub mod materialized;
pub mod numeric_shape;
pub mod revision;
pub mod text_match;

pub use candidate::*;
pub use column::*;
pub use decimal::*;
pub use diagnostic::*;
pub use document::*;
pub use grid::*;
pub use materialized::*;
pub use numeric_shape::*;
pub use revision::*;
pub use text_match::*;
