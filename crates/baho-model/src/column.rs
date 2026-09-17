use serde::{Deserialize, Serialize};

/// Metadata for a single column in a materialized view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDefinition {
    /// Stable column identifier, e.g. "column-0".
    pub id: String,
    /// Zero-based ordinal position.
    pub ordinal: usize,
    /// Raw header text from the source, if available.
    pub source_header_raw: Option<String>,
    /// Normalized (trimmed, folded) header text, if available.
    pub source_header_normalized: Option<String>,
    /// Human-readable display name for presentation.
    pub display_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construct_column_definition() {
        let col = ColumnDefinition {
            id: "column-0".to_string(),
            ordinal: 0,
            source_header_raw: Some("Name".to_string()),
            source_header_normalized: Some("name".to_string()),
            display_name: "Name".to_string(),
        };
        assert_eq!(col.id, "column-0");
        assert_eq!(col.ordinal, 0);
        assert_eq!(col.display_name, "Name");
    }

    #[test]
    fn optional_headers_can_be_none() {
        let col = ColumnDefinition {
            id: "column-1".to_string(),
            ordinal: 1,
            source_header_raw: None,
            source_header_normalized: None,
            display_name: "Column 2".to_string(),
        };
        assert!(col.source_header_raw.is_none());
        assert!(col.source_header_normalized.is_none());
    }

    #[test]
    fn serde_round_trip() {
        let col = ColumnDefinition {
            id: "column-5".to_string(),
            ordinal: 5,
            source_header_raw: Some("Revenue ($)".to_string()),
            source_header_normalized: Some("revenue ($)".to_string()),
            display_name: "Revenue ($)".to_string(),
        };
        let json = serde_json::to_string(&col).unwrap();
        let back: ColumnDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(col, back);
    }
}
