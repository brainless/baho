use std::collections::HashSet;

use baho_model::candidate::TableCandidate;
use baho_model::column::ColumnDefinition;
use baho_plan::plan::{Plan, PlanStep};

use crate::error::ExecutionError;

/// Validates that a plan can execute against the provided table and columns.
///
/// Checks:
/// - The `table_id` referenced in the plan source exists in available tables.
/// - All column references in plan steps exist in the provided column definitions.
pub fn validate_execution_context(
    plan: &Plan,
    table: &TableCandidate,
    columns: &[ColumnDefinition],
) -> Result<(), ExecutionError> {
    if plan.source.table_id != table.id {
        return Err(ExecutionError::TableNotFound {
            table_id: plan.source.table_id.clone(),
        });
    }

    let available: HashSet<&str> = columns.iter().map(|c| c.id.as_str()).collect();

    for step in &plan.steps {
        match step {
            PlanStep::Filter { predicate } => {
                check_column_ref(predicate.column(), &available)?;
            }
            PlanStep::Select { columns: cols } => {
                for col in cols {
                    check_column_ref(col, &available)?;
                }
            }
            PlanStep::Distinct { columns: cols, .. } => {
                for col in cols {
                    check_column_ref(col, &available)?;
                }
            }
        }
    }

    Ok(())
}

fn check_column_ref(column: &str, available: &HashSet<&str>) -> Result<(), ExecutionError> {
    if !available.contains(column) {
        return Err(ExecutionError::ColumnNotFound {
            column_id: column.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_model::candidate::*;
    use baho_model::column::ColumnDefinition;
    use baho_model::grid::GridRegion;
    use baho_plan::plan::*;

    fn sample_table() -> TableCandidate {
        TableCandidate {
            id: "table-0".to_string(),
            region: GridRegion {
                id: "region-0".to_string(),
                header_row: Some(0),
                body_start_row: 1,
                body_end_row: 5,
                col_start: 0,
                col_end: 2,
            },
            header: HeaderDecision {
                source_row: 0,
                cells: vec![
                    HeaderCell {
                        col: 0,
                        raw_text: "Name".to_string(),
                        normalized_text: "name".to_string(),
                        column_id: "column-0".to_string(),
                    },
                    HeaderCell {
                        col: 1,
                        raw_text: "Type".to_string(),
                        normalized_text: "type".to_string(),
                        column_id: "column-1".to_string(),
                    },
                ],
            },
            body_row_classifications: vec![],
            score: CandidateScore {
                total: 0.9,
                components: vec![],
            },
            selected: true,
        }
    }

    fn sample_columns() -> Vec<ColumnDefinition> {
        vec![
            ColumnDefinition {
                id: "column-0".to_string(),
                ordinal: 0,
                source_header_raw: Some("Name".to_string()),
                source_header_normalized: Some("name".to_string()),
                display_name: "Name".to_string(),
            },
            ColumnDefinition {
                id: "column-1".to_string(),
                ordinal: 1,
                source_header_raw: Some("Type".to_string()),
                source_header_normalized: Some("type".to_string()),
                display_name: "Type".to_string(),
            },
        ]
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

    #[test]
    fn valid_execution_context_passes() {
        let plan = sample_plan();
        let table = sample_table();
        let columns = sample_columns();
        assert!(validate_execution_context(&plan, &table, &columns).is_ok());
    }

    #[test]
    fn unknown_table_id_rejected() {
        let mut plan = sample_plan();
        plan.source.table_id = "table-99".to_string();
        let table = sample_table();
        let columns = sample_columns();
        let err = validate_execution_context(&plan, &table, &columns).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::TableNotFound { table_id } if table_id == "table-99"
        ));
    }

    #[test]
    fn unknown_column_reference_rejected() {
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "abc123".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Select {
                columns: vec!["column-99".to_string()],
            }],
        };
        let table = sample_table();
        let columns = sample_columns();
        let err = validate_execution_context(&plan, &table, &columns).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::ColumnNotFound { column_id } if column_id == "column-99"
        ));
    }
}
