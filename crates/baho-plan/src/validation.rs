use std::collections::HashSet;

use thiserror::Error;

use crate::plan::{Expression, Plan, PlanStep};

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
}

/// Validates the structural well-formedness of a plan without reference to a
/// specific table schema.
pub fn validate_plan_structure(plan: &Plan) -> Result<(), PlanValidationError> {
    if plan.schema_version != 1 {
        return Err(PlanValidationError::UnsupportedSchemaVersion {
            version: plan.schema_version,
        });
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
        validate_step_structure(step)?;
    }

    Ok(())
}

fn validate_step_structure(step: &PlanStep) -> Result<(), PlanValidationError> {
    match step {
        PlanStep::Filter { predicate } => validate_expression(predicate),
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

fn validate_expression(expr: &Expression) -> Result<(), PlanValidationError> {
    match expr {
        Expression::IsNotBlank { column } => {
            if column.is_empty() {
                return Err(PlanValidationError::InvalidColumnReference {
                    column: column.clone(),
                    detail: "column must not be empty".to_string(),
                });
            }
            Ok(())
        }
    }
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
                check_column_ref(predicate.column(), &available)?;
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

impl Expression {
    /// Returns the column this expression references.
    pub fn column(&self) -> &str {
        match self {
            Expression::IsNotBlank { column } => column,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::*;

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

    #[test]
    fn valid_plan_passes_structure_validation() {
        let plan = valid_plan();
        assert!(validate_plan_structure(&plan).is_ok());
    }

    #[test]
    fn reject_unsupported_schema_version() {
        let mut plan = valid_plan();
        plan.schema_version = 2;
        let err = validate_plan_structure(&plan).unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::UnsupportedSchemaVersion { version: 2 }
        ));
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
}
