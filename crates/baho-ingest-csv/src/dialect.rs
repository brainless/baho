use std::path::Path;

use serde::{Deserialize, Serialize};

/// CSV dialect configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsvDialect {
    pub schema_version: u32,
    pub delimiter: u8,
    pub quote: u8,
    pub quote_escape: u8,
    pub has_header: bool,
}

impl Default for CsvDialect {
    fn default() -> Self {
        Self {
            schema_version: 1,
            delimiter: b',',
            quote: b'"',
            quote_escape: b'"',
            has_header: true,
        }
    }
}

impl CsvDialect {
    /// Detect dialect from file extension.
    ///
    /// Returns tab-delimited dialect for `.tsv` extensions,
    /// comma-delimited dialect for everything else.
    pub fn for_path(path: &Path) -> Self {
        let is_tsv = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("tsv"))
            .unwrap_or(false);

        if is_tsv {
            Self {
                delimiter: b'\t',
                ..Self::default()
            }
        } else {
            Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values() {
        let d = CsvDialect::default();
        assert_eq!(d.schema_version, 1);
        assert_eq!(d.delimiter, b',');
        assert_eq!(d.quote, b'"');
        assert_eq!(d.quote_escape, b'"');
        assert!(d.has_header);
    }

    #[test]
    fn serde_round_trip() {
        let d = CsvDialect::default();
        let json = serde_json::to_string(&d).unwrap();
        let back: CsvDialect = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn for_path_tsv() {
        let d = CsvDialect::for_path(Path::new("data.tsv"));
        assert_eq!(d.delimiter, b'\t');
        assert_eq!(d.quote, b'"');
    }

    #[test]
    fn for_path_tsv_uppercase() {
        let d = CsvDialect::for_path(Path::new("data.TSV"));
        assert_eq!(d.delimiter, b'\t');
    }

    #[test]
    fn for_path_csv() {
        let d = CsvDialect::for_path(Path::new("data.csv"));
        assert_eq!(d.delimiter, b',');
    }

    #[test]
    fn for_path_txt_defaults_to_comma() {
        let d = CsvDialect::for_path(Path::new("data.txt"));
        assert_eq!(d.delimiter, b',');
    }

    #[test]
    fn for_path_no_extension_defaults_to_comma() {
        let d = CsvDialect::for_path(Path::new("data"));
        assert_eq!(d.delimiter, b',');
    }
}
