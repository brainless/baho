use serde::{Deserialize, Serialize};

/// Stable identity of an input file at a point in time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRevision {
    /// SHA-256 hex digest of the file contents.
    pub content_hash: String,
    /// File size in bytes.
    pub file_size: u64,
    /// Optional filesystem modification time as an ISO-8601 string.
    pub modified_time: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construct_and_access_fields() {
        let rev = SourceRevision {
            content_hash: "abc123".to_string(),
            file_size: 1024,
            modified_time: Some("2025-01-01T00:00:00Z".to_string()),
        };
        assert_eq!(rev.content_hash, "abc123");
        assert_eq!(rev.file_size, 1024);
        assert!(rev.modified_time.is_some());
    }

    #[test]
    fn modified_time_can_be_none() {
        let rev = SourceRevision {
            content_hash: "def456".to_string(),
            file_size: 0,
            modified_time: None,
        };
        assert!(rev.modified_time.is_none());
    }

    #[test]
    fn serde_round_trip() {
        let rev = SourceRevision {
            content_hash: "aaa".to_string(),
            file_size: 512,
            modified_time: None,
        };
        let json = serde_json::to_string(&rev).unwrap();
        let back: SourceRevision = serde_json::from_str(&json).unwrap();
        assert_eq!(rev, back);
    }
}
