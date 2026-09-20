use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::ImportedDocument;
use crate::error::{ImportError, UnsupportedFormat};
use crate::profile::InspectOptions;
use crate::traits::{FormatImporter, FormatInspector};

/// A registered pair of inspector and importer for one format.
struct RegisteredFormat {
    inspector: Box<dyn FormatInspector>,
    importer: Box<dyn FormatImporter>,
}

/// The result of format dispatch before a format-specific importer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedFormat {
    Csv,
}

/// Classify an input without invoking a format-specific parser.
///
/// Recognized formats without an importer are returned as typed errors. An
/// input that is neither a supported CSV-like text file nor a recognized
/// future format remains a format-detection failure.
pub fn detect_format(path: &Path, options: &InspectOptions) -> Result<DetectedFormat, ImportError> {
    let header = read_header(path, options.max_field_size)?;
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if header.starts_with(b"%PDF-") || extension == "pdf" {
        return Err(ImportError::UnsupportedFormat {
            format: UnsupportedFormat::Pdf,
        });
    }
    if extension == "ods" {
        return Err(ImportError::UnsupportedFormat {
            format: UnsupportedFormat::Ods,
        });
    }
    if matches!(extension.as_str(), "xlsx" | "xls")
        || header.starts_with(b"PK\x03\x04")
        || header.starts_with(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1")
    {
        return Err(ImportError::UnsupportedFormat {
            format: UnsupportedFormat::Excel,
        });
    }

    let text_like = header
        .iter()
        .all(|&byte| byte == b'\n' || byte == b'\r' || (0x09..=0x7e).contains(&byte));
    if matches!(extension.as_str(), "csv" | "tsv" | "txt") || text_like {
        return Ok(DetectedFormat::Csv);
    }

    Err(ImportError::FormatDetectionFailed {
        detail: format!("no supported format claimed `{}`", path.display()),
    })
}

/// Registry of available format importers.
pub struct ImportRegistry {
    formats: Vec<RegisteredFormat>,
}

impl ImportRegistry {
    pub fn new() -> Self {
        Self {
            formats: Vec::new(),
        }
    }

    /// Register an inspector/importer pair for a format.
    pub fn register(
        &mut self,
        inspector: Box<dyn FormatInspector>,
        importer: Box<dyn FormatImporter>,
    ) {
        self.formats.push(RegisteredFormat {
            inspector,
            importer,
        });
    }

    /// Detect the file's format and import it.
    ///
    /// Reads a bounded header prefix for format detection, then delegates
    /// to the first matching importer.
    pub fn detect_and_import(
        &self,
        path: &Path,
        options: &InspectOptions,
    ) -> Result<ImportedDocument, ImportError> {
        let header = read_header(path, options.max_field_size)?;
        let format = self
            .formats
            .iter()
            .find(|f| f.inspector.can_inspect(path, &header))
            .ok_or_else(|| ImportError::FormatDetectionFailed {
                detail: format!("no registered importer claimed `{}`", path.display()),
            })?;
        format.importer.import(path, options)
    }
}

impl Default for ImportRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a bounded prefix of the file for format detection.
fn read_header(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ImportError> {
    let mut file = File::open(path).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut buf = vec![0u8; max_bytes.min(8192)];
    let n = file.read(&mut buf).map_err(|source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    buf.truncate(n);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::InputProfile;
    use baho_model::document::{Cell, CellAddress, Document, Row, Sheet};
    use baho_model::revision::SourceRevision;
    use std::io::Write;
    use tempfile::NamedTempFile;

    struct MockInspector;
    impl FormatInspector for MockInspector {
        fn name(&self) -> &str {
            "mock"
        }
        fn can_inspect(&self, _path: &Path, header_bytes: &[u8]) -> bool {
            header_bytes.starts_with(b"MOCK")
        }
    }

    struct MockImporter;
    impl FormatImporter for MockImporter {
        fn name(&self) -> &str {
            "mock"
        }
        fn import(
            &self,
            path: &Path,
            _options: &InspectOptions,
        ) -> Result<ImportedDocument, ImportError> {
            Ok(ImportedDocument {
                document: Document {
                    source: SourceRevision {
                        content_hash: "mock-hash".to_string(),
                        file_size: 0,
                        modified_time: None,
                    },
                    sheets: vec![Sheet {
                        index: 0,
                        name: None,
                        rows: vec![Row {
                            index: 0,
                            cells: vec![Cell {
                                address: CellAddress {
                                    sheet_index: 0,
                                    row: 0,
                                    col: 0,
                                },
                                raw_text: "hello".to_string(),
                                interpreted: None,
                            }],
                        }],
                    }],
                    path: path.to_string_lossy().to_string(),
                },
                input_profile: InputProfile::default(),
                diagnostics: Vec::new(),
            })
        }
    }

    #[test]
    fn no_matching_importer_returns_error() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"UNKNOWN").unwrap();

        let registry = ImportRegistry::new();
        let opts = InspectOptions::default();
        let err = registry.detect_and_import(file.path(), &opts).unwrap_err();
        assert!(matches!(err, ImportError::FormatDetectionFailed { .. }));
    }

    #[test]
    fn dispatch_distinguishes_supported_csv_unknown_and_unsupported_formats() {
        let cases = [
            (
                "table.csv",
                b"a,b\n1,2\n".as_slice(),
                Ok(DetectedFormat::Csv),
            ),
            (
                "report.xlsx",
                b"not csv".as_slice(),
                Err(UnsupportedFormat::Excel),
            ),
            (
                "report.ods",
                b"not csv".as_slice(),
                Err(UnsupportedFormat::Ods),
            ),
            (
                "report.pdf",
                b"%PDF-1.7".as_slice(),
                Err(UnsupportedFormat::Pdf),
            ),
        ];

        for (name, bytes, expected) in cases {
            let mut file = tempfile::Builder::new().suffix(name).tempfile().unwrap();
            file.write_all(bytes).unwrap();
            let result = detect_format(file.path(), &InspectOptions::default());
            match expected {
                Ok(format) => assert_eq!(result.unwrap(), format),
                Err(format) => assert!(matches!(
                    result,
                    Err(ImportError::UnsupportedFormat { format: actual }) if actual == format
                )),
            }
        }

        let mut unknown = NamedTempFile::new().unwrap();
        unknown.write_all(b"\0\x01\x02").unwrap();
        assert!(matches!(
            detect_format(unknown.path(), &InspectOptions::default()),
            Err(ImportError::FormatDetectionFailed { .. })
        ));
    }

    #[test]
    fn pdf_signature_wins_over_csv_extension() {
        let mut file = tempfile::Builder::new().suffix(".csv").tempfile().unwrap();
        file.write_all(b"%PDF-1.7").unwrap();
        assert!(matches!(
            detect_format(file.path(), &InspectOptions::default()),
            Err(ImportError::UnsupportedFormat {
                format: UnsupportedFormat::Pdf
            })
        ));
    }

    #[test]
    fn matching_importer_succeeds() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"MOCK,data").unwrap();

        let mut registry = ImportRegistry::new();
        registry.register(Box::new(MockInspector), Box::new(MockImporter));

        let opts = InspectOptions::default();
        let result = registry.detect_and_import(file.path(), &opts).unwrap();
        assert_eq!(result.document.sheets.len(), 1);
        assert_eq!(result.document.sheets[0].rows[0].cells[0].raw_text, "hello");
    }

    #[test]
    fn first_matching_importer_wins() {
        struct SecondInspector;
        impl FormatInspector for SecondInspector {
            fn name(&self) -> &str {
                "second"
            }
            fn can_inspect(&self, _path: &Path, header_bytes: &[u8]) -> bool {
                header_bytes.starts_with(b"MOCK")
            }
        }

        struct SecondImporter;
        impl FormatImporter for SecondImporter {
            fn name(&self) -> &str {
                "second"
            }
            fn import(
                &self,
                _path: &Path,
                _options: &InspectOptions,
            ) -> Result<ImportedDocument, ImportError> {
                panic!("second importer should not be called")
            }
        }

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"MOCK,data").unwrap();

        let mut registry = ImportRegistry::new();
        registry.register(Box::new(MockInspector), Box::new(MockImporter));
        registry.register(Box::new(SecondInspector), Box::new(SecondImporter));

        let opts = InspectOptions::default();
        let result = registry.detect_and_import(file.path(), &opts).unwrap();
        assert_eq!(result.document.path, file.path().to_string_lossy());
    }
}
