use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Take};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Bounded inputs and deterministic candidate order used for dialect detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialectDetectionConfig {
    pub candidate_delimiters: Vec<u8>,
    pub max_bytes: u64,
    pub max_records: usize,
    pub extension_tie_breaker: bool,
}

impl Default for DialectDetectionConfig {
    fn default() -> Self {
        Self {
            candidate_delimiters: vec![b',', b'\t', b';', b'|'],
            max_bytes: 64 * 1024,
            max_records: 64,
            extension_tie_breaker: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DialectDetectionError {
    #[error("I/O error reading `{path}` during dialect detection: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("delimiter evidence is ambiguous among {delimiters:?}")]
    Ambiguous { delimiters: Vec<u8> },
}

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
    /// Detect the delimiter from a bounded prefix of logical CSV records.
    ///
    /// Delimiters inside quoted fields do not contribute evidence. A known
    /// extension is considered only when the strongest content evidence is
    /// exactly tied.
    pub fn detect(
        path: &Path,
        config: &DialectDetectionConfig,
    ) -> Result<Self, DialectDetectionError> {
        let file = File::open(path).map_err(|source| DialectDetectionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let evidence = collect_evidence(file.take(config.max_bytes), config).map_err(|source| {
            DialectDetectionError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;

        let best_score = evidence
            .iter()
            .map(|candidate| candidate.score)
            .max()
            .unwrap_or_default();
        let tied = evidence
            .iter()
            .filter(|candidate| candidate.score == best_score)
            .map(|candidate| candidate.delimiter)
            .collect::<Vec<_>>();

        let delimiter = if tied.len() == 1 {
            tied[0]
        } else if config.extension_tie_breaker {
            extension_delimiter(path)
                .filter(|delimiter| tied.contains(delimiter))
                .ok_or_else(|| DialectDetectionError::Ambiguous {
                    delimiters: tied.clone(),
                })?
        } else {
            return Err(DialectDetectionError::Ambiguous { delimiters: tied });
        };

        Ok(Self {
            delimiter,
            ..Self::default()
        })
    }
}

#[derive(Debug)]
struct DelimiterEvidence {
    delimiter: u8,
    // Lexicographic score: stable rows first, then breadth of evidence, then
    // the modal field-boundary count. All inputs are bounded by the config.
    score: (usize, usize, usize),
}

fn collect_evidence(
    mut reader: Take<File>,
    config: &DialectDetectionConfig,
) -> Result<Vec<DelimiterEvidence>, std::io::Error> {
    let mut bytes = Vec::with_capacity(config.max_bytes.min(64 * 1024) as usize);
    reader.read_to_end(&mut bytes)?;

    let mut record_counts = Vec::new();
    let mut counts = vec![0usize; config.candidate_delimiters.len()];
    let mut in_quotes = false;
    let mut record_has_content = false;
    let mut index = 0usize;

    while index < bytes.len() && record_counts.len() < config.max_records {
        let byte = bytes[index];
        if byte == b'"' {
            if in_quotes && bytes.get(index + 1) == Some(&b'"') {
                index += 2;
                record_has_content = true;
                continue;
            }
            in_quotes = !in_quotes;
            record_has_content = true;
        } else if !in_quotes && (byte == b'\n' || byte == b'\r') {
            if record_has_content || counts.iter().any(|count| *count > 0) {
                record_counts.push(std::mem::take(&mut counts));
                counts.resize(config.candidate_delimiters.len(), 0);
            }
            record_has_content = false;
            if byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
        } else if !in_quotes {
            if let Some(candidate_index) = config
                .candidate_delimiters
                .iter()
                .position(|delimiter| *delimiter == byte)
            {
                counts[candidate_index] += 1;
            } else if !byte.is_ascii_whitespace() {
                record_has_content = true;
            }
        } else {
            record_has_content = true;
        }
        index += 1;
    }

    if record_counts.len() < config.max_records
        && !in_quotes
        && (record_has_content || counts.iter().any(|count| *count > 0))
    {
        record_counts.push(counts);
    }

    Ok(config
        .candidate_delimiters
        .iter()
        .enumerate()
        .map(|(candidate_index, delimiter)| {
            let positive = record_counts
                .iter()
                .map(|counts| counts[candidate_index])
                .filter(|count| *count > 0)
                .collect::<Vec<_>>();
            let mut frequencies = BTreeMap::new();
            for count in &positive {
                *frequencies.entry(*count).or_insert(0usize) += 1;
            }
            let (modal_count, stable_rows) = frequencies
                .into_iter()
                .max_by_key(|(count, frequency)| (*frequency, *count))
                .unwrap_or((0, 0));
            DelimiterEvidence {
                delimiter: *delimiter,
                score: (stable_rows, positive.len(), modal_count),
            }
        })
        .collect())
}

fn extension_delimiter(path: &Path) -> Option<u8> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("csv") => Some(b','),
        Some("tsv") => Some(b'\t'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn detect_named(name: &str, content: &str) -> Result<CsvDialect, DialectDetectionError> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        CsvDialect::detect(&path, &DialectDetectionConfig::default())
    }

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
    fn detects_semicolon_from_csv_content() {
        let dialect = detect_named(
            "misnamed.csv",
            "Name;City;Code\nAlice;Pune;A1\nBob;Delhi;B2\n",
        )
        .unwrap();

        assert_eq!(dialect.delimiter, b';');
    }

    #[test]
    fn detects_tab_from_txt_content() {
        let dialect = detect_named("report.txt", "Name\tValue\nAlice\t10\nBob\t20\n").unwrap();

        assert_eq!(dialect.delimiter, b'\t');
    }

    #[test]
    fn ignores_candidate_delimiters_inside_quoted_fields() {
        let dialect = detect_named(
            "contacts.txt",
            "Name;Comment\nAlice;\"comma, inside\"\nBob;\"another, comma\"\n",
        )
        .unwrap();

        assert_eq!(dialect.delimiter, b';');
    }

    #[test]
    fn extension_breaks_equivalent_content_evidence() {
        let dialect = detect_named("mixed.csv", "A,B;C\n1,2;3\n").unwrap();

        assert_eq!(dialect.delimiter, b',');
    }

    #[test]
    fn equivalent_content_without_extension_is_ambiguous() {
        let error = detect_named("mixed", "A,B;C\n1,2;3\n").unwrap_err();

        assert!(matches!(
            error,
            DialectDetectionError::Ambiguous { delimiters }
                if delimiters == vec![b',', b';']
        ));
    }
}
