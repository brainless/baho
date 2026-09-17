use serde::{Deserialize, Serialize};

/// Configuration for format inspection and import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectOptions {
    /// Maximum number of records to sample during inspection.
    pub max_sample_records: usize,
    /// Maximum size in bytes for a single field.
    pub max_field_size: usize,
    /// Force a specific encoding instead of auto-detecting.
    pub force_encoding: Option<String>,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            max_sample_records: 1000,
            max_field_size: 1_048_576, // 1 MB
            force_encoding: None,
        }
    }
}

/// Physical inspection results from a source file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputProfile {
    /// Detected or forced encoding (e.g. "utf-8").
    pub encoding: String,
    /// Detected field delimiter.
    pub detected_delimiter: Option<char>,
    /// Detected quote character.
    pub detected_quote: Option<char>,
    /// Total logical record count, if the file was fully read.
    pub logical_record_count: Option<usize>,
    /// Minimum field width observed across sampled records.
    pub sampled_width_min: Option<usize>,
    /// Maximum field width observed across sampled records.
    pub sampled_width_max: Option<usize>,
    /// Number of completely blank records encountered.
    pub blank_record_count: usize,
    /// Number of malformed records encountered.
    pub malformed_record_count: usize,
    /// Names of limits that were reached during inspection.
    pub limits_reached: Vec<String>,
}

impl Default for InputProfile {
    fn default() -> Self {
        Self {
            encoding: "utf-8".to_string(),
            detected_delimiter: None,
            detected_quote: None,
            logical_record_count: None,
            sampled_width_min: None,
            sampled_width_max: None,
            blank_record_count: 0,
            malformed_record_count: 0,
            limits_reached: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_options_defaults() {
        let opts = InspectOptions::default();
        assert_eq!(opts.max_sample_records, 1000);
        assert_eq!(opts.max_field_size, 1_048_576);
        assert!(opts.force_encoding.is_none());
    }

    #[test]
    fn input_profile_defaults() {
        let profile = InputProfile::default();
        assert_eq!(profile.encoding, "utf-8");
        assert!(profile.detected_delimiter.is_none());
        assert!(profile.detected_quote.is_none());
        assert!(profile.logical_record_count.is_none());
        assert!(profile.sampled_width_min.is_none());
        assert!(profile.sampled_width_max.is_none());
        assert_eq!(profile.blank_record_count, 0);
        assert_eq!(profile.malformed_record_count, 0);
        assert!(profile.limits_reached.is_empty());
    }

    #[test]
    fn inspect_options_with_forced_encoding() {
        let opts = InspectOptions {
            force_encoding: Some("latin-1".to_string()),
            ..Default::default()
        };
        assert_eq!(opts.force_encoding.as_deref(), Some("latin-1"));
    }

    #[test]
    fn input_profile_serde_round_trip() {
        let profile = InputProfile {
            encoding: "utf-8".to_string(),
            detected_delimiter: Some(','),
            detected_quote: Some('"'),
            logical_record_count: Some(42),
            sampled_width_min: Some(3),
            sampled_width_max: Some(8),
            blank_record_count: 5,
            malformed_record_count: 1,
            limits_reached: vec!["max_sample_records".to_string()],
        };
        let json = serde_json::to_string(&profile).unwrap();
        let back: InputProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, back);
    }
}
