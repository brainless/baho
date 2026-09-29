use std::path::Path;

use crate::ImportedDocument;
use crate::error::ImportError;
use crate::profile::InspectOptions;
use crate::selected_source::{
    RowReadLimits, SelectedRowBatch, SelectedSourceMetadata, SelectedSourceReadError,
};

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

/// Sequential access to rows from an already selected table.
///
/// Implementations return no more than `limits.max_rows` rows and no more
/// than `limits.max_bytes` of present cell text in one batch. A single row
/// that cannot fit the byte limit is an error. Rows remain in source order,
/// and each row retains its original source coordinate. Every read must be
/// checked against the exact `SourceRevision` exposed by `metadata`; a change
/// before or during a read returns `RevisionChanged` rather than mixing data
/// from two revisions.
pub trait SelectedSourceReader {
    fn metadata(&self) -> &SelectedSourceMetadata;

    fn read_next(
        &mut self,
        limits: RowReadLimits,
    ) -> Result<SelectedRowBatch, SelectedSourceReadError>;
}
