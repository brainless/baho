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
}
