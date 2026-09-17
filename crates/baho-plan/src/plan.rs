use serde::{Deserialize, Serialize};

/// A versioned, serializable operation plan bound to a specific source revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Schema version; must be 1.
    pub schema_version: u32,
    /// The source this plan was built for.
    pub source: PlanSource,
    /// Ordered operation steps.
    pub steps: Vec<PlanStep>,
}

/// Binds a plan to a specific table within a source revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSource {
    /// Content hash of the source file.
    pub revision: String,
    /// Stable table ID (e.g. "table-0").
    pub table_id: String,
}

/// A single operation step within a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlanStep {
    /// Keep only rows matching the predicate.
    Filter { predicate: Expression },
    /// Project the listed columns.
    Select { columns: Vec<String> },
    /// Deduplicate rows by the listed columns.
    Distinct {
        columns: Vec<String>,
        keep: DistinctKeep,
    },
}

/// Which row to retain when deduplicating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistinctKeep {
    /// Keep the first occurrence in source order.
    First,
}

/// An expression used within plan steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Expression {
    /// True when the cell value is not blank.
    IsNotBlank { column: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> Plan {
        Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "abc123".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![
                PlanStep::Filter {
                    predicate: Expression::IsNotBlank {
                        column: "column-1".to_string(),
                    },
                },
                PlanStep::Select {
                    columns: vec!["column-1".to_string()],
                },
                PlanStep::Distinct {
                    columns: vec!["column-1".to_string()],
                    keep: DistinctKeep::First,
                },
            ],
        }
    }

    #[test]
    fn construct_filter_select_distinct_plan() {
        let plan = sample_plan();
        assert_eq!(plan.schema_version, 1);
        assert_eq!(plan.steps.len(), 3);
    }

    #[test]
    fn serde_round_trip() {
        let plan = sample_plan();
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn json_structure_matches_expected_format() {
        let plan = sample_plan();
        let json: serde_json::Value = serde_json::to_value(&plan).unwrap();

        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["source"]["revision"], "abc123");
        assert_eq!(json["source"]["table_id"], "table-0");

        let steps = json["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 3);

        assert_eq!(steps[0]["op"], "filter");
        assert_eq!(steps[0]["predicate"]["op"], "is_not_blank");
        assert_eq!(steps[0]["predicate"]["column"], "column-1");

        assert_eq!(steps[1]["op"], "select");
        assert_eq!(steps[1]["columns"], serde_json::json!(["column-1"]));

        assert_eq!(steps[2]["op"], "distinct");
        assert_eq!(steps[2]["columns"], serde_json::json!(["column-1"]));
        assert_eq!(steps[2]["keep"], "first");
    }

    #[test]
    fn default_plan_source_construction() {
        let source = PlanSource {
            revision: "sha256:abcdef".to_string(),
            table_id: "table-0".to_string(),
        };
        assert_eq!(source.revision, "sha256:abcdef");
        assert_eq!(source.table_id, "table-0");
    }
}
