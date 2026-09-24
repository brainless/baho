use std::collections::HashSet;

use thiserror::Error;

use crate::plan::{
    ComparisonOperator, Expression, Literal, PLAN_SCHEMA_VERSION_1, PLAN_SCHEMA_VERSION_2,
    PLAN_SCHEMA_VERSION_3, Plan, PlanStep,
};

/// Maximum predicate depth accepted by structural validation (locked
/// decision 7). A leaf is depth 1.
pub const MAX_PREDICATE_DEPTH: usize = 16;

/// Maximum predicate node count accepted by structural validation (locked
/// decision 7). Each expression node counts once.
pub const MAX_PREDICATE_NODES: usize = 64;

/// Errors returned by plan validation.
#[derive(Debug, Clone, Error)]
pub enum PlanValidationError {
    #[error("unsupported plan schema version: {version}")]
    UnsupportedSchemaVersion { version: u32 },

    #[error("plan must contain at least one step")]
    EmptySteps,

    #[error("select step must reference at least one column")]
    EmptySelectColumns,

    #[error("distinct step must reference at least one column")]
    EmptyDistinctColumns,

    #[error("unknown step operation: {op}")]
    UnknownStepOp { op: String },

    #[error("invalid column reference '{column}': {detail}")]
    InvalidColumnReference { column: String, detail: String },

    #[error("invalid source: {detail}")]
    InvalidSource { detail: String },

    #[error("expression '{op}' is not allowed in plan schema version {version}")]
    ExpressionNotAllowedInVersion { op: String, version: u32 },

    #[error("boolean expression '{op}' must contain at least one predicate")]
    EmptyBooleanOperands { op: String },

    #[error("incompatible column and literal types: {detail}")]
    TypeMismatch { detail: String },

    #[error("excessive expression depth or size: {detail}")]
    ExpressionLimitExceeded { detail: String },

    #[error("unresolved deferred literal: {detail}")]
    UnresolvedLiteral { detail: String },
}

impl PlanValidationError {
    /// Stable diagnostic code derived from this validation error.
    ///
    /// Type mismatches and expression-limit failures carry the finalized Epic
    /// 006 codes; every other validation failure remains `plan.invalid`.
    pub fn diagnostic_code(&self) -> &'static str {
        match self {
            PlanValidationError::TypeMismatch { .. } => "plan.type_mismatch",
            PlanValidationError::ExpressionLimitExceeded { .. } => "plan.expression_limit_exceeded",
            _ => "plan.invalid",
        }
    }
}

/// Validates the structural well-formedness of a plan without reference to a
/// specific table schema.
///
/// Version 1 plans may contain only `is_not_blank` expressions. Versions 2
/// and 3 recursively validate predicate structure, column references, Boolean
/// operand lists, depth and node limits, and comparison/literal type
/// compatibility. Version 3 allows the same expression set as version 2; it
/// only changes text `=`/`!=` comparison semantics at execution time.
pub fn validate_plan_structure(plan: &Plan) -> Result<(), PlanValidationError> {
    match plan.schema_version {
        PLAN_SCHEMA_VERSION_1 | PLAN_SCHEMA_VERSION_2 | PLAN_SCHEMA_VERSION_3 => {}
        version => {
            return Err(PlanValidationError::UnsupportedSchemaVersion { version });
        }
    }

    if plan.source.revision.is_empty() {
        return Err(PlanValidationError::InvalidSource {
            detail: "revision must not be empty".to_string(),
        });
    }

    if plan.source.table_id.is_empty() {
        return Err(PlanValidationError::InvalidSource {
            detail: "table_id must not be empty".to_string(),
        });
    }

    if plan.steps.is_empty() {
        return Err(PlanValidationError::EmptySteps);
    }

    for step in &plan.steps {
        validate_step_structure(step, plan.schema_version)?;
    }

    Ok(())
}

fn validate_step_structure(step: &PlanStep, version: u32) -> Result<(), PlanValidationError> {
    match step {
        PlanStep::Filter { predicate } => validate_expression(predicate, version, 1, &mut 0),
        PlanStep::Select { columns } => {
            if columns.is_empty() {
                return Err(PlanValidationError::EmptySelectColumns);
            }
            check_no_duplicates(columns, "select")?;
            Ok(())
        }
        PlanStep::Distinct { columns, .. } => {
            if columns.is_empty() {
                return Err(PlanValidationError::EmptyDistinctColumns);
            }
            check_no_duplicates(columns, "distinct")?;
            Ok(())
        }
    }
}

fn validate_expression(
    expr: &Expression,
    version: u32,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), PlanValidationError> {
    *nodes += 1;
    if *nodes > MAX_PREDICATE_NODES {
        return Err(PlanValidationError::ExpressionLimitExceeded {
            detail: format!("predicate node count exceeds {MAX_PREDICATE_NODES}"),
        });
    }
    if depth > MAX_PREDICATE_DEPTH {
        return Err(PlanValidationError::ExpressionLimitExceeded {
            detail: format!("predicate depth exceeds {MAX_PREDICATE_DEPTH}"),
        });
    }
    match expr {
        Expression::IsNotBlank { column } => check_nonempty_column(column),
        Expression::Compare {
            column,
            operator,
            literal,
        } => {
            if version < PLAN_SCHEMA_VERSION_2 {
                return Err(PlanValidationError::ExpressionNotAllowedInVersion {
                    op: expr.op().to_string(),
                    version,
                });
            }
            check_nonempty_column(column)?;
            check_comparison_types(column, *operator, literal)
        }
        Expression::And { predicates } | Expression::Or { predicates } => {
            if version < PLAN_SCHEMA_VERSION_2 {
                return Err(PlanValidationError::ExpressionNotAllowedInVersion {
                    op: expr.op().to_string(),
                    version,
                });
            }
            if predicates.is_empty() {
                return Err(PlanValidationError::EmptyBooleanOperands {
                    op: expr.op().to_string(),
                });
            }
            for predicate in predicates {
                validate_expression(predicate, version, depth + 1, nodes)?;
            }
            Ok(())
        }
        Expression::Not { predicate } => {
            if version < PLAN_SCHEMA_VERSION_2 {
                return Err(PlanValidationError::ExpressionNotAllowedInVersion {
                    op: expr.op().to_string(),
                    version,
                });
            }
            validate_expression(predicate, version, depth + 1, nodes)
        }
    }
}

fn check_nonempty_column(column: &str) -> Result<(), PlanValidationError> {
    if column.is_empty() {
        return Err(PlanValidationError::InvalidColumnReference {
            column: column.to_string(),
            detail: "column must not be empty".to_string(),
        });
    }
    Ok(())
}

/// Type-checks one comparison as far as structure allows: ordered comparisons
/// require a decimal literal, while `=` and `!=` accept text or decimal.
/// Deferred literals must be resolved before compilation (Epic 008 locked
/// decision 7) and are never valid in a persisted plan. Column-type
/// compatibility is checked later against the selected table.
fn check_comparison_types(
    column: &str,
    operator: ComparisonOperator,
    literal: &Literal,
) -> Result<(), PlanValidationError> {
    if let Literal::Deferred(raw) = literal {
        return Err(PlanValidationError::UnresolvedLiteral {
            detail: format!(
                "column '{column}' still carries deferred numeric literal '{raw}'; resolve it under the column's parse policy before compilation"
            ),
        });
    }
    if operator.is_ordered() && !matches!(literal, Literal::Decimal(_)) {
        return Err(PlanValidationError::TypeMismatch {
            detail: format!(
                "column '{column}' with ordered comparison '{}' requires a decimal literal",
                operator.as_str()
            ),
        });
    }
    Ok(())
}

fn check_no_duplicates(columns: &[String], _step_op: &str) -> Result<(), PlanValidationError> {
    let mut seen = HashSet::new();
    for col in columns {
        if !seen.insert(col) {
            return Err(PlanValidationError::InvalidColumnReference {
                column: col.clone(),
                detail: "duplicate column reference within step".to_string(),
            });
        }
    }
    Ok(())
}

/// Validates that all column references in the plan exist in the provided
/// column list. Called after a table is selected and its columns are known.
pub fn validate_plan_references(
    plan: &Plan,
    available_columns: &[String],
) -> Result<(), PlanValidationError> {
    let available: HashSet<&str> = available_columns.iter().map(|s| s.as_str()).collect();

    for step in &plan.steps {
        match step {
            PlanStep::Filter { predicate } => {
                for column in predicate.columns() {
                    check_column_ref(column, &available)?;
                }
            }
            PlanStep::Select { columns } => {
                for col in columns {
                    check_column_ref(col, &available)?;
                }
            }
            PlanStep::Distinct { columns, .. } => {
                for col in columns {
                    check_column_ref(col, &available)?;
                }
            }
        }
    }

    Ok(())
}

fn check_column_ref(column: &str, available: &HashSet<&str>) -> Result<(), PlanValidationError> {
    if !available.contains(column) {
        return Err(PlanValidationError::InvalidColumnReference {
            column: column.to_string(),
            detail: "column does not exist in the selected table".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::*;
    use baho_model::ExactDecimal;

    fn dec(text: &str) -> ExactDecimal {
        ExactDecimal::parse(text).unwrap()
    }

    fn valid_plan() -> Plan {
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

    fn leaf(column: &str) -> Expression {
        Expression::IsNotBlank {
            column: column.to_string(),
        }
    }

    fn not(predicate: Expression) -> Expression {
        Expression::Not {
            predicate: Box::new(predicate),
        }
    }

    fn and(predicates: Vec<Expression>) -> Expression {
        Expression::And { predicates }
    }

    fn or(predicates: Vec<Expression>) -> Expression {
        Expression::Or { predicates }
    }

    fn compare(column: &str, operator: ComparisonOperator, literal: Literal) -> Expression {
        Expression::Compare {
            column: column.to_string(),
            operator,
            literal,
        }
    }

    fn filter_plan(version: u32, predicate: Expression) -> Plan {
        Plan {
            schema_version: version,
            source: PlanSource {
                revision: "abc123".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Filter { predicate }],
        }
    }

    #[test]
    fn valid_plan_passes_structure_validation() {
        let plan = valid_plan();
        assert!(validate_plan_structure(&plan).is_ok());
    }

    #[test]
    fn accept_schema_version_2_with_version_1_expressions() {
        let mut plan = valid_plan();
        plan.schema_version = 2;
        assert!(validate_plan_structure(&plan).is_ok());
    }

    #[test]
    fn reject_unsupported_schema_version() {
        for version in [0, 4, 5, u32::MAX] {
            let mut plan = valid_plan();
            plan.schema_version = version;
            let err = validate_plan_structure(&plan).unwrap_err();
            assert!(matches!(
                err,
                PlanValidationError::UnsupportedSchemaVersion { version: v } if v == version
            ));
        }
    }

    #[test]
    fn accept_schema_version_3_with_version_2_expressions() {
        let mut plan = valid_plan();
        plan.schema_version = PLAN_SCHEMA_VERSION_3;
        assert!(validate_plan_structure(&plan).is_ok());

        let predicate = or(vec![
            compare(
                "column-0",
                ComparisonOperator::Equal,
                Literal::Text("unemployed".to_string()),
            ),
            not(and(vec![
                leaf("column-1"),
                compare(
                    "column-1",
                    ComparisonOperator::Less,
                    Literal::Decimal(dec("10000")),
                ),
            ])),
        ]);
        let plan = filter_plan(PLAN_SCHEMA_VERSION_3, predicate);
        assert!(validate_plan_structure(&plan).is_ok());
    }

    #[test]
    fn reject_empty_steps() {
        let mut plan = valid_plan();
        plan.steps = vec![];
        let err = validate_plan_structure(&plan).unwrap_err();
        assert!(matches!(err, PlanValidationError::EmptySteps));
    }

    #[test]
    fn reject_empty_select_columns() {
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "abc".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Select { columns: vec![] }],
        };
        let err = validate_plan_structure(&plan).unwrap_err();
        assert!(matches!(err, PlanValidationError::EmptySelectColumns));
    }

    #[test]
    fn reject_empty_distinct_columns() {
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "abc".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Distinct {
                columns: vec![],
                keep: DistinctKeep::First,
            }],
        };
        let err = validate_plan_structure(&plan).unwrap_err();
        assert!(matches!(err, PlanValidationError::EmptyDistinctColumns));
    }

    #[test]
    fn valid_references_pass() {
        let plan = valid_plan();
        let columns = vec![
            "column-0".to_string(),
            "column-1".to_string(),
            "column-2".to_string(),
        ];
        assert!(validate_plan_references(&plan, &columns).is_ok());
    }

    #[test]
    fn unknown_column_reference_rejected() {
        let plan = valid_plan();
        let columns = vec!["column-0".to_string(), "column-2".to_string()];
        let err = validate_plan_references(&plan, &columns).unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::InvalidColumnReference { column, .. } if column == "column-1"
        ));
    }

    #[test]
    fn epic_desired_behavior_plan_validates() {
        let json = r#"{
            "schema_version": 1,
            "source": {
                "revision": "sha256:abcdef1234567890",
                "table_id": "table-0"
            },
            "steps": [
                {
                    "op": "filter",
                    "predicate": {
                        "op": "is_not_blank",
                        "column": "column-1"
                    }
                },
                {
                    "op": "select",
                    "columns": ["column-1"]
                },
                {
                    "op": "distinct",
                    "columns": ["column-1"],
                    "keep": "first"
                }
            ]
        }"#;

        let plan: Plan = serde_json::from_str(json).unwrap();
        assert!(validate_plan_structure(&plan).is_ok());

        let columns = vec![
            "column-0".to_string(),
            "column-1".to_string(),
            "column-2".to_string(),
        ];
        assert!(validate_plan_references(&plan, &columns).is_ok());
    }

    #[test]
    fn v1_plans_accept_is_not_blank_only() {
        assert!(validate_plan_structure(&filter_plan(1, leaf("column-1"))).is_ok());
    }

    #[test]
    fn v1_plans_reject_new_expression_kinds() {
        let new_kinds = [
            compare(
                "column-1",
                ComparisonOperator::Equal,
                Literal::Text("x".to_string()),
            ),
            and(vec![leaf("column-1")]),
            or(vec![leaf("column-1")]),
            not(leaf("column-1")),
        ];
        for predicate in new_kinds {
            let err = validate_plan_structure(&filter_plan(1, predicate)).unwrap_err();
            assert!(matches!(
                err,
                PlanValidationError::ExpressionNotAllowedInVersion { version: 1, .. }
            ));
            assert_eq!(err.diagnostic_code(), "plan.invalid");
        }
    }

    #[test]
    fn v2_predicate_tree_validates() {
        let predicate = or(vec![
            compare(
                "column-0",
                ComparisonOperator::Equal,
                Literal::Text("unemployed".to_string()),
            ),
            compare(
                "column-1",
                ComparisonOperator::Less,
                Literal::Decimal(dec("10000")),
            ),
        ]);
        let plan = filter_plan(2, predicate);
        assert!(validate_plan_structure(&plan).is_ok());
        let columns = vec!["column-0".to_string(), "column-1".to_string()];
        assert!(validate_plan_references(&plan, &columns).is_ok());
    }

    #[test]
    fn reject_empty_and_or_operand_lists() {
        for predicate in [and(vec![]), or(vec![])] {
            let err = validate_plan_structure(&filter_plan(2, predicate)).unwrap_err();
            assert!(matches!(
                err,
                PlanValidationError::EmptyBooleanOperands { .. }
            ));
            assert_eq!(err.diagnostic_code(), "plan.invalid");
        }
    }

    #[test]
    fn accepts_predicate_depth_16_and_rejects_17() {
        let mut at_limit = leaf("column-1");
        for _ in 0..MAX_PREDICATE_DEPTH - 1 {
            at_limit = not(at_limit);
        }
        assert_eq!(at_limit.depth(), MAX_PREDICATE_DEPTH);
        assert!(validate_plan_structure(&filter_plan(2, at_limit)).is_ok());

        let mut over_limit = leaf("column-1");
        for _ in 0..MAX_PREDICATE_DEPTH {
            over_limit = not(over_limit);
        }
        assert_eq!(over_limit.depth(), MAX_PREDICATE_DEPTH + 1);
        let err = validate_plan_structure(&filter_plan(2, over_limit)).unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::ExpressionLimitExceeded { .. }
        ));
        assert_eq!(err.diagnostic_code(), "plan.expression_limit_exceeded");
    }

    #[test]
    fn accepts_predicate_node_count_64_and_rejects_65() {
        let at_limit = and((0..MAX_PREDICATE_NODES - 1)
            .map(|_| leaf("column-1"))
            .collect());
        assert_eq!(at_limit.node_count(), MAX_PREDICATE_NODES);
        assert!(validate_plan_structure(&filter_plan(2, at_limit)).is_ok());

        let over_limit = and((0..MAX_PREDICATE_NODES).map(|_| leaf("column-1")).collect());
        assert_eq!(over_limit.node_count(), MAX_PREDICATE_NODES + 1);
        let err = validate_plan_structure(&filter_plan(2, over_limit)).unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::ExpressionLimitExceeded { .. }
        ));
        assert_eq!(err.diagnostic_code(), "plan.expression_limit_exceeded");
    }

    #[test]
    fn reject_ordered_comparison_with_text_literal() {
        for operator in [
            ComparisonOperator::Less,
            ComparisonOperator::LessOrEqual,
            ComparisonOperator::Greater,
            ComparisonOperator::GreaterOrEqual,
        ] {
            let predicate = compare("column-1", operator, Literal::Text("10000".to_string()));
            let err = validate_plan_structure(&filter_plan(2, predicate)).unwrap_err();
            assert!(matches!(err, PlanValidationError::TypeMismatch { .. }));
            assert_eq!(err.diagnostic_code(), "plan.type_mismatch");
        }
    }

    #[test]
    fn reject_unresolved_deferred_literals_in_persisted_plans() {
        // Epic 008 locked decision 7: deferred numerics must be resolved
        // before compilation and are never valid in a persisted plan.
        for operator in [
            ComparisonOperator::Equal,
            ComparisonOperator::NotEqual,
            ComparisonOperator::Less,
        ] {
            let predicate = compare(
                "column-1",
                operator,
                Literal::Deferred("10,000".to_string()),
            );
            let err = validate_plan_structure(&filter_plan(3, predicate)).unwrap_err();
            assert!(
                matches!(err, PlanValidationError::UnresolvedLiteral { .. }),
                "got {err:?}"
            );
            assert_eq!(err.diagnostic_code(), "plan.invalid");
        }
    }

    #[test]
    fn accept_ordered_comparisons_with_decimal_literals() {
        for operator in [
            ComparisonOperator::Less,
            ComparisonOperator::LessOrEqual,
            ComparisonOperator::Greater,
            ComparisonOperator::GreaterOrEqual,
        ] {
            let predicate = compare("column-1", operator, Literal::Decimal(dec("-2.5")));
            assert!(validate_plan_structure(&filter_plan(2, predicate)).is_ok());
        }
    }

    #[test]
    fn accept_equality_comparisons_with_text_and_decimal_literals() {
        for literal in [Literal::Text("x".to_string()), Literal::Decimal(dec("1.5"))] {
            for operator in [ComparisonOperator::Equal, ComparisonOperator::NotEqual] {
                let predicate = compare("column-1", operator, literal.clone());
                assert!(validate_plan_structure(&filter_plan(2, predicate)).is_ok());
            }
        }
    }

    #[test]
    fn type_mismatch_nested_in_predicate_tree_is_rejected() {
        let predicate = not(and(vec![
            leaf("column-0"),
            or(vec![
                leaf("column-1"),
                compare(
                    "column-2",
                    ComparisonOperator::GreaterOrEqual,
                    Literal::Text("10".to_string()),
                ),
            ]),
        ]));
        let err = validate_plan_structure(&filter_plan(2, predicate)).unwrap_err();
        assert!(matches!(err, PlanValidationError::TypeMismatch { .. }));
    }

    #[test]
    fn reject_unknown_columns_recursively() {
        let predicate = or(vec![
            leaf("column-0"),
            and(vec![not(compare(
                "missing-column",
                ComparisonOperator::Equal,
                Literal::Text("x".to_string()),
            ))]),
        ]);
        let plan = filter_plan(2, predicate);
        assert!(validate_plan_structure(&plan).is_ok());
        let err = validate_plan_references(&plan, &["column-0".to_string()]).unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::InvalidColumnReference { column, .. } if column == "missing-column"
        ));
    }

    #[test]
    fn reject_empty_column_names_recursively() {
        let predicate = or(vec![leaf("column-0"), not(leaf(""))]);
        let err = validate_plan_structure(&filter_plan(2, predicate)).unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::InvalidColumnReference { column, .. } if column.is_empty()
        ));
    }

    #[test]
    fn diagnostic_codes_are_stable() {
        assert_eq!(
            PlanValidationError::TypeMismatch {
                detail: "x".to_string()
            }
            .diagnostic_code(),
            "plan.type_mismatch"
        );
        assert_eq!(
            PlanValidationError::ExpressionLimitExceeded {
                detail: "x".to_string()
            }
            .diagnostic_code(),
            "plan.expression_limit_exceeded"
        );
        for err in [
            PlanValidationError::UnsupportedSchemaVersion { version: 9 },
            PlanValidationError::EmptySteps,
            PlanValidationError::EmptySelectColumns,
            PlanValidationError::EmptyDistinctColumns,
            PlanValidationError::UnknownStepOp {
                op: "frobnicate".to_string(),
            },
            PlanValidationError::InvalidColumnReference {
                column: "c".to_string(),
                detail: "x".to_string(),
            },
            PlanValidationError::InvalidSource {
                detail: "x".to_string(),
            },
            PlanValidationError::ExpressionNotAllowedInVersion {
                op: "and".to_string(),
                version: 1,
            },
            PlanValidationError::EmptyBooleanOperands {
                op: "and".to_string(),
            },
        ] {
            assert_eq!(err.diagnostic_code(), "plan.invalid");
        }
    }
}
