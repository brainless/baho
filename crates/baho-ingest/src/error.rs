use std::path::PathBuf;

use thiserror::Error;

/// A format that is recognized but does not yet have an importer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedFormat {
    Excel,
    Ods,
    Pdf,
}

impl std::fmt::Display for UnsupportedFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Excel => "Excel",
            Self::Ods => "ODS",
            Self::Pdf => "PDF",
        };
        formatter.write_str(name)
    }
}

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

    #[error("unsupported {format} format")]
    UnsupportedFormat { format: UnsupportedFormat },

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
    fn unsupported_format_display() {
        let err = ImportError::UnsupportedFormat {
            format: UnsupportedFormat::Pdf,
        };
        assert_eq!(err.to_string(), "unsupported PDF format");
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
