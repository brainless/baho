use std::path::PathBuf;

use thiserror::Error;

/// Errors that can occur during document ingestion.
#[derive(Debug, Error)]
pub enum ImportError {
    #[error("I/O error reading `{path}`: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("unsupported encoding: {detail}")]
    UnsupportedEncoding { detail: String },

    #[error("format detection failed: {detail}")]
    FormatDetectionFailed { detail: String },

    #[error("limit exceeded ({limit}): {detail}")]
    LimitExceeded { limit: String, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;
    use std::io;

    #[test]
    fn io_error_display() {
        let err = ImportError::Io {
            path: PathBuf::from("/tmp/test.csv"),
            source: io::Error::new(io::ErrorKind::NotFound, "not found"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/test.csv"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn unsupported_encoding_display() {
        let err = ImportError::UnsupportedEncoding {
            detail: "expected UTF-8, found ISO-8859-1".to_string(),
        };
        assert!(err.to_string().contains("expected UTF-8"));
    }

    #[test]
    fn format_detection_failed_display() {
        let err = ImportError::FormatDetectionFailed {
            detail: "no importer claimed the file".to_string(),
        };
        assert!(err.to_string().contains("no importer claimed the file"));
    }

    #[test]
    fn limit_exceeded_display() {
        let err = ImportError::LimitExceeded {
            limit: "max_sample_records".to_string(),
            detail: "exceeded 1000 records".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("max_sample_records"));
        assert!(msg.contains("exceeded 1000 records"));
    }

    #[test]
    fn io_error_source_chain() {
        let inner = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let err = ImportError::Io {
            path: PathBuf::from("/tmp/test.csv"),
            source: inner,
        };
        assert!((&err as &dyn StdError).source().is_some());
    }
}
