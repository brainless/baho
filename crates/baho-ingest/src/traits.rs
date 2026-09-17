use std::path::Path;

use crate::ImportedDocument;
use crate::error::ImportError;
use crate::profile::InspectOptions;

/// Detect whether a format importer can handle a given file.
pub trait FormatInspector {
    /// Human-readable format name (e.g. "csv", "xlsx").
    fn name(&self) -> &str;

    /// Return `true` if this inspector believes it can import the file,
    /// based on the path and the first few bytes of content.
    fn can_inspect(&self, path: &Path, header_bytes: &[u8]) -> bool;
}

/// Import a file into baho's document model.
pub trait FormatImporter {
    /// Human-readable format name.
    fn name(&self) -> &str;

    /// Import the file at `path` using the given inspection options.
    fn import(
        &self,
        path: &Path,
        options: &InspectOptions,
    ) -> Result<ImportedDocument, ImportError>;
}
