use baho_model::{ExactDecimal, TextMatchPolicy};
use serde::{Deserialize, Serialize};

/// Plan schema version 1: select, distinct, and `is_not_blank` filters only.
pub const PLAN_SCHEMA_VERSION_1: u32 = 1;

/// Plan schema version 2: adds typed literals and recursive predicates.
///
/// Version 1 plans keep their existing semantics and expression set; the
/// extended expression kinds are never emitted under schema version 1.
pub const PLAN_SCHEMA_VERSION_2: u32 = 2;

/// Plan schema version 3: text `=`/`!=` become case-insensitive.
///
/// Version 3 defines text `=`/`!=` as case-insensitive: Unicode full
/// lowercase conversion (`str::to_lowercase`) is applied to both the source
/// text and the literal before exact comparison (Epic 008 locked decision 1,
/// [`TextMatchPolicy::UnicodeLowercase`]). Versions 1 and 2 retain exact
/// case-sensitive equality. The expression set is unchanged from version 2
/// (`is_not_blank`, `compare`, `and`, `or`, `not`); ordered text comparison
/// remains rejected by structural validation.
pub const PLAN_SCHEMA_VERSION_3: u32 = 3;

/// A versioned, serializable operation plan bound to a specific source revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Schema version: 1, 2, or 3.
    pub schema_version: u32,
    /// The source this plan was built for.
    pub source: PlanSource,
    /// Ordered operation steps.
    pub steps: Vec<PlanStep>,
}

impl Plan {
    /// The text-match policy for text `=`/`!=` under this plan's schema
    /// version (Epic 008 locked decision 2).
    ///
    /// Version 3 folds both sides with Unicode full lowercase conversion;
    /// versions 1 and 2 retain exact case-sensitive equality. Unknown
    /// versions, which structural validation rejects, fall back to
    /// [`TextMatchPolicy::Exact`].
    pub fn text_match_policy(&self) -> TextMatchPolicy {
        match self.schema_version {
            PLAN_SCHEMA_VERSION_3 => TextMatchPolicy::UnicodeLowercase,
            _ => TextMatchPolicy::Exact,
        }
    }
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
///
/// The `is_not_blank` JSON shape is unchanged from plan schema version 1;
/// `compare`, `and`, `or`, and `not` require plan schema version 2 or later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Expression {
    /// True when the cell value is not blank.
    IsNotBlank { column: String },
    /// Compare one column against a typed literal.
    Compare {
        column: String,
        operator: ComparisonOperator,
        literal: Literal,
    },
    /// True when every predicate holds.
    And { predicates: Vec<Expression> },
    /// True when at least one predicate holds.
    Or { predicates: Vec<Expression> },
    /// Three-valued negation of one predicate.
    Not { predicate: Box<Expression> },
}

impl Expression {
    /// Operation tag of this expression, matching its serialized `"op"` value.
    pub fn op(&self) -> &'static str {
        match self {
            Expression::IsNotBlank { .. } => "is_not_blank",
            Expression::Compare { .. } => "compare",
            Expression::And { .. } => "and",
            Expression::Or { .. } => "or",
            Expression::Not { .. } => "not",
        }
    }

    /// The column referenced by a single-column leaf, if this expression is one.
    pub fn column(&self) -> Option<&str> {
        match self {
            Expression::IsNotBlank { column } | Expression::Compare { column, .. } => Some(column),
            Expression::And { .. } | Expression::Or { .. } | Expression::Not { .. } => None,
        }
    }

    /// Every column referenced anywhere in this predicate, in deterministic
    /// visitation order.
    pub fn columns(&self) -> Vec<&str> {
        let mut columns = Vec::new();
        self.collect_columns(&mut columns);
        columns
    }

    /// Number of nodes in this predicate tree; each expression counts once.
    pub fn node_count(&self) -> usize {
        1 + match self {
            Expression::IsNotBlank { .. } | Expression::Compare { .. } => 0,
            Expression::And { predicates } | Expression::Or { predicates } => {
                predicates.iter().map(Expression::node_count).sum()
            }
            Expression::Not { predicate } => predicate.node_count(),
        }
    }

    /// Depth of this predicate tree; a leaf is depth 1.
    pub fn depth(&self) -> usize {
        1 + match self {
            Expression::IsNotBlank { .. } | Expression::Compare { .. } => 0,
            Expression::And { predicates } | Expression::Or { predicates } => {
                predicates.iter().map(Expression::depth).max().unwrap_or(0)
            }
            Expression::Not { predicate } => predicate.depth(),
        }
    }

    fn collect_columns<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expression::IsNotBlank { column } | Expression::Compare { column, .. } => {
                out.push(column);
            }
            Expression::And { predicates } | Expression::Or { predicates } => {
                for predicate in predicates {
                    predicate.collect_columns(out);
                }
            }
            Expression::Not { predicate } => predicate.collect_columns(out),
        }
    }
}

/// A typed literal operand of a comparison predicate.
///
/// The variant tag records the literal kind (`"text"`, `"decimal"`, or
/// `"deferred"`). [`Literal::Deferred`] is an Epic 008 recognition-time
/// placeholder for a format-shaped numeric token that must be resolved under
/// the bound column's selected policy before compilation; structural
/// validation rejects it in any persisted or executable plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Literal {
    /// A text literal.
    Text(String),
    /// An exact decimal literal.
    Decimal(ExactDecimal),
    /// Raw numeric text awaiting policy-aware resolution (Epic 008 locked
    /// decision 7).
    Deferred(String),
}

/// Comparison operators for atomic predicates.
///
/// Serialized as the surface grammar symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonOperator {
    #[serde(rename = "=")]
    Equal,
    #[serde(rename = "!=")]
    NotEqual,
    #[serde(rename = "<")]
    Less,
    #[serde(rename = "<=")]
    LessOrEqual,
    #[serde(rename = ">")]
    Greater,
    #[serde(rename = ">=")]
    GreaterOrEqual,
}

impl ComparisonOperator {
    /// Surface form used in the constrained grammar and plan JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            ComparisonOperator::Equal => "=",
            ComparisonOperator::NotEqual => "!=",
            ComparisonOperator::Less => "<",
            ComparisonOperator::LessOrEqual => "<=",
            ComparisonOperator::Greater => ">",
            ComparisonOperator::GreaterOrEqual => ">=",
        }
    }

    /// Whether this is an ordered comparison (`<`, `<=`, `>`, `>=`), which
    /// requires a decimal literal.
    pub fn is_ordered(self) -> bool {
        matches!(
            self,
            ComparisonOperator::Less
                | ComparisonOperator::LessOrEqual
                | ComparisonOperator::Greater
                | ComparisonOperator::GreaterOrEqual
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> ExactDecimal {
        ExactDecimal::parse(text).unwrap()
    }

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

    /// `List rows where Job = unemployed or Annual Income < 10000`
    fn sample_v2_plan() -> Plan {
        Plan {
            schema_version: 2,
            source: PlanSource {
                revision: "abc123".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Filter {
                predicate: Expression::Or {
                    predicates: vec![
                        Expression::Compare {
                            column: "column-0".to_string(),
                            operator: ComparisonOperator::Equal,
                            literal: Literal::Text("unemployed".to_string()),
                        },
                        Expression::Compare {
                            column: "column-1".to_string(),
                            operator: ComparisonOperator::Less,
                            literal: Literal::Decimal(dec("10000")),
                        },
                    ],
                },
            }],
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

    #[test]
    fn is_not_blank_json_shape_unchanged() {
        let expr = Expression::IsNotBlank {
            column: "column-1".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&expr).unwrap(),
            serde_json::json!({ "op": "is_not_blank", "column": "column-1" })
        );
    }

    #[test]
    fn v1_plan_json_round_trip_is_stable() {
        let json = serde_json::json!({
            "schema_version": 1,
            "source": { "revision": "abc123", "table_id": "table-0" },
            "steps": [
                {
                    "op": "filter",
                    "predicate": { "op": "is_not_blank", "column": "column-1" }
                },
                { "op": "select", "columns": ["column-1"] },
                { "op": "distinct", "columns": ["column-1"], "keep": "first" }
            ]
        });
        let plan: Plan = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&plan).unwrap(), json);
    }

    #[test]
    fn v2_predicate_tree_serde_round_trip() {
        let plan = sample_v2_plan();
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }

    #[test]
    fn v2_predicate_tree_json_structure() {
        let json = serde_json::to_value(sample_v2_plan()).unwrap();
        assert_eq!(json["schema_version"], 2);
        let predicate = &json["steps"][0]["predicate"];
        assert_eq!(predicate["op"], "or");
        let operands = predicate["predicates"].as_array().unwrap();
        assert_eq!(operands.len(), 2);
        assert_eq!(operands[0]["op"], "compare");
        assert_eq!(operands[0]["column"], "column-0");
        assert_eq!(operands[0]["operator"], "=");
        assert_eq!(
            operands[0]["literal"],
            serde_json::json!({ "text": "unemployed" })
        );
        assert_eq!(operands[1]["op"], "compare");
        assert_eq!(operands[1]["column"], "column-1");
        assert_eq!(operands[1]["operator"], "<");
        assert_eq!(
            operands[1]["literal"],
            serde_json::json!({ "decimal": "10000" })
        );
    }

    #[test]
    fn text_match_policy_derives_from_schema_version() {
        let mut plan = sample_v2_plan();
        assert_eq!(plan.text_match_policy(), TextMatchPolicy::Exact);
        plan.schema_version = PLAN_SCHEMA_VERSION_1;
        assert_eq!(plan.text_match_policy(), TextMatchPolicy::Exact);
        plan.schema_version = PLAN_SCHEMA_VERSION_2;
        assert_eq!(plan.text_match_policy(), TextMatchPolicy::Exact);
        plan.schema_version = PLAN_SCHEMA_VERSION_3;
        assert_eq!(plan.text_match_policy(), TextMatchPolicy::UnicodeLowercase);
        // Unknown versions fall back to exact; structural validation
        // rejects them before execution can consult the policy.
        plan.schema_version = 4;
        assert_eq!(plan.text_match_policy(), TextMatchPolicy::Exact);
    }

    #[test]
    fn v3_plan_json_round_trip_is_stable() {
        let mut plan = sample_v2_plan();
        plan.schema_version = PLAN_SCHEMA_VERSION_3;
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
        assert_eq!(serde_json::to_value(&back).unwrap()["schema_version"], 3);
    }

    #[test]
    fn nested_predicate_json_shape() {
        let expr = Expression::Not {
            predicate: Box::new(Expression::And {
                predicates: vec![
                    Expression::IsNotBlank {
                        column: "column-0".to_string(),
                    },
                    Expression::Compare {
                        column: "column-0".to_string(),
                        operator: ComparisonOperator::NotEqual,
                        literal: Literal::Text("x".to_string()),
                    },
                ],
            }),
        };
        assert_eq!(
            serde_json::to_value(&expr).unwrap(),
            serde_json::json!({
                "op": "not",
                "predicate": {
                    "op": "and",
                    "predicates": [
                        { "op": "is_not_blank", "column": "column-0" },
                        {
                            "op": "compare",
                            "column": "column-0",
                            "operator": "!=",
                            "literal": { "text": "x" }
                        }
                    ]
                }
            })
        );
    }

    #[test]
    fn comparison_operators_serialize_as_grammar_symbols() {
        for (operator, symbol) in [
            (ComparisonOperator::Equal, "="),
            (ComparisonOperator::NotEqual, "!="),
            (ComparisonOperator::Less, "<"),
            (ComparisonOperator::LessOrEqual, "<="),
            (ComparisonOperator::Greater, ">"),
            (ComparisonOperator::GreaterOrEqual, ">="),
        ] {
            assert_eq!(
                serde_json::to_value(operator).unwrap(),
                serde_json::json!(symbol)
            );
            assert_eq!(operator.as_str(), symbol);
            let back: ComparisonOperator = serde_json::from_str(&format!("\"{symbol}\"")).unwrap();
            assert_eq!(back, operator);
        }
        assert!(!ComparisonOperator::Equal.is_ordered());
        assert!(!ComparisonOperator::NotEqual.is_ordered());
        assert!(ComparisonOperator::Less.is_ordered());
        assert!(ComparisonOperator::LessOrEqual.is_ordered());
        assert!(ComparisonOperator::Greater.is_ordered());
        assert!(ComparisonOperator::GreaterOrEqual.is_ordered());
    }

    #[test]
    fn decimal_literal_serializes_canonical_string() {
        let literal = Literal::Decimal(dec("1.10"));
        assert_eq!(
            serde_json::to_value(&literal).unwrap(),
            serde_json::json!({ "decimal": "1.1" })
        );
        let back: Literal = serde_json::from_str(r#"{"decimal":"-2.50"}"#).unwrap();
        assert_eq!(back, Literal::Decimal(dec("-2.5")));
    }

    #[test]
    fn predicate_shape_accessors() {
        let expr = Expression::Not {
            predicate: Box::new(Expression::And {
                predicates: vec![
                    Expression::IsNotBlank {
                        column: "a".to_string(),
                    },
                    Expression::Compare {
                        column: "b".to_string(),
                        operator: ComparisonOperator::Less,
                        literal: Literal::Decimal(dec("1")),
                    },
                ],
            }),
        };
        assert_eq!(expr.op(), "not");
        assert_eq!(expr.column(), None);
        assert_eq!(expr.columns(), ["a", "b"]);
        assert_eq!(expr.node_count(), 4);
        assert_eq!(expr.depth(), 3);
        let leaf = Expression::IsNotBlank {
            column: "a".to_string(),
        };
        assert_eq!(leaf.column(), Some("a"));
        assert_eq!(leaf.node_count(), 1);
        assert_eq!(leaf.depth(), 1);
    }
}
