//! Format-independent inspection and import contracts.
//!
//! This crate defines the traits and result types for importing documents
//! into baho's model. Format-specific heuristics live in dedicated crates
//! (e.g. `baho-ingest-csv`).

pub mod error;
pub mod profile;
pub mod registry;
pub mod traits;

pub use error::{ImportError, UnsupportedFormat};
pub use profile::{InputProfile, InspectOptions};
pub use registry::{DetectedFormat, ImportRegistry, detect_format};
pub use traits::{FormatImporter, FormatInspector};

use baho_model::diagnostic::Diagnostic;
use baho_model::document::Document;

/// Result of a successful document import.
#[derive(Debug, Clone)]
pub struct ImportedDocument {
    pub document: Document,
    pub input_profile: InputProfile,
    pub diagnostics: Vec<Diagnostic>,
}
