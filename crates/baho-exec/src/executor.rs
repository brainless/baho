use std::collections::HashSet;

use baho_model::column::ColumnDefinition;
use baho_model::diagnostic::{Diagnostic, Severity};
use baho_model::document::{CellAddress, Value};
use baho_model::materialized::{MaterializedRow, MaterializedView, RowProvenance};
use baho_plan::plan::{DistinctKeep, Expression, Plan, PlanStep};

use crate::error::ExecutionError;

/// Grid data provided to the executor as input.
pub struct GridInput {
    pub table_id: String,
    pub source_revision: String,
    pub columns: Vec<ColumnDefinition>,
    pub rows: Vec<Vec<Option<Value>>>,
    pub source_rows: Vec<usize>,
}

/// The result of executing a plan.
#[derive(Debug)]
pub struct ExecutionResult {
    pub view: MaterializedView,
    pub diagnostics: Vec<Diagnostic>,
    pub rows_processed: usize,
    pub rows_output: usize,
}

/// Executes a validated plan against the provided grid data.
pub fn execute_plan(plan: &Plan, grid: &GridInput) -> Result<ExecutionResult, ExecutionError> {
    if plan.source.table_id != grid.table_id {
        return Err(ExecutionError::TableNotFound {
            table_id: plan.source.table_id.clone(),
        });
    }

    if plan.source.revision != grid.source_revision {
        return Err(ExecutionError::SourceRevisionMismatch {
            expected: plan.source.revision.clone(),
            actual: grid.source_revision.clone(),
        });
    }

    // Build initial column index lookup by id
    let mut col_index: std::collections::HashMap<String, usize> = grid
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.clone(), i))
        .collect();

    // Validate all column references exist
    validate_column_refs(plan, &col_index)?;

    let mut diagnostics = Vec::new();
    let rows_processed = grid.rows.len();

    // Working data: (row_values, source_row_index)
    let mut working: Vec<(Vec<Option<Value>>, usize)> = grid
        .rows
        .iter()
        .zip(grid.source_rows.iter())
        .map(|(row, &src)| (row.clone(), src))
        .collect();

    // Track output columns (updated by select)
    let mut current_columns = grid.columns.clone();

    // Execute steps sequentially
    for step in &plan.steps {
        match step {
            PlanStep::Filter { predicate } => {
                let col_idx = resolve_column_index(predicate.column(), &col_index)?;
                let before = working.len();
                working = apply_filter(working, col_idx, predicate);
                let removed = before - working.len();
                if removed > 0 {
                    diagnostics.push(Diagnostic {
                        code: "exec.rows_filtered".to_string(),
                        severity: Severity::Info,
                        stage: "exec".to_string(),
                        message: format!("{removed} rows removed by filter"),
                        location: None,
                    });
                }
            }
            PlanStep::Select { columns } => {
                let indices: Vec<usize> = columns
                    .iter()
                    .map(|c| resolve_column_index(c, &col_index))
                    .collect::<Result<_, _>>()?;
                working = apply_select(working, &indices);
                // Rebuild column layout and index for subsequent steps
                current_columns = indices
                    .iter()
                    .enumerate()
                    .map(|(new_ord, &old_idx)| {
                        let mut col = grid.columns[old_idx].clone();
                        col.ordinal = new_ord;
                        col
                    })
                    .collect();
                col_index = current_columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (c.id.clone(), i))
                    .collect();
            }
            PlanStep::Distinct { columns, keep } => {
                let indices: Vec<usize> = columns
                    .iter()
                    .map(|c| resolve_column_index(c, &col_index))
                    .collect::<Result<_, _>>()?;
                let before = working.len();
                working = apply_distinct(working, &indices, keep);
                let removed = before - working.len();
                if removed > 0 {
                    diagnostics.push(Diagnostic {
                        code: "exec.duplicates_removed".to_string(),
                        severity: Severity::Info,
                        stage: "exec".to_string(),
                        message: format!("{removed} duplicate rows removed"),
                        location: None,
                    });
                }
            }
        }
    }

    let rows_output = working.len();

    // Build materialized rows and provenance
    let materialized_rows: Vec<MaterializedRow> = working
        .iter()
        .map(|(values, _)| MaterializedRow {
            values: values.clone(),
        })
        .collect();

    let provenance: Vec<RowProvenance> = working
        .iter()
        .map(|(_, source_row)| RowProvenance {
            source_row: *source_row,
            source_address: CellAddress {
                sheet_index: 0,
                row: *source_row,
                col: 0,
            },
        })
        .collect();

    let view = MaterializedView {
        columns: current_columns,
        rows: materialized_rows,
        provenance,
    };

    Ok(ExecutionResult {
        view,
        diagnostics,
        rows_processed,
        rows_output,
    })
}

fn validate_column_refs(
    plan: &Plan,
    col_index: &std::collections::HashMap<String, usize>,
) -> Result<(), ExecutionError> {
    for step in &plan.steps {
        match step {
            PlanStep::Filter { predicate } => {
                resolve_column_index(predicate.column(), col_index)?;
            }
            PlanStep::Select { columns } => {
                for col in columns {
                    resolve_column_index(col, col_index)?;
                }
            }
            PlanStep::Distinct { columns, .. } => {
                for col in columns {
                    resolve_column_index(col, col_index)?;
                }
            }
        }
    }
    Ok(())
}

fn resolve_column_index(
    column: &str,
    col_index: &std::collections::HashMap<String, usize>,
) -> Result<usize, ExecutionError> {
    col_index
        .get(column)
        .copied()
        .ok_or_else(|| ExecutionError::ColumnNotFound {
            column_id: column.to_string(),
        })
}

fn apply_filter(
    rows: Vec<(Vec<Option<Value>>, usize)>,
    col_idx: usize,
    predicate: &Expression,
) -> Vec<(Vec<Option<Value>>, usize)> {
    match predicate {
        Expression::IsNotBlank { .. } => rows
            .into_iter()
            .filter(|(values, _)| values.get(col_idx).map_or(false, |v| is_not_blank(v)))
            .collect(),
    }
}

fn is_not_blank(value: &Option<Value>) -> bool {
    match value {
        None => false,
        Some(Value::Blank) => false,
        Some(Value::Text(s)) => !s.trim().is_empty(),
        Some(_) => true,
    }
}

fn apply_select(
    rows: Vec<(Vec<Option<Value>>, usize)>,
    indices: &[usize],
) -> Vec<(Vec<Option<Value>>, usize)> {
    rows.into_iter()
        .map(|(values, src)| {
            let selected: Vec<Option<Value>> = indices
                .iter()
                .map(|&i| values.get(i).cloned().flatten())
                .collect();
            (selected, src)
        })
        .collect()
}

fn apply_distinct(
    rows: Vec<(Vec<Option<Value>>, usize)>,
    indices: &[usize],
    keep: &DistinctKeep,
) -> Vec<(Vec<Option<Value>>, usize)> {
    match keep {
        DistinctKeep::First => {
            let mut seen = HashSet::new();
            let mut result = Vec::new();
            for (values, src) in rows {
                let key: Vec<String> = indices
                    .iter()
                    .map(|&i| match values.get(i).and_then(|v| v.as_ref()) {
                        None => "".to_string(),
                        Some(Value::Blank) => "\0blank".to_string(),
                        Some(Value::Text(s)) => format!("t:{s}"),
                        Some(Value::Number(n)) => format!("n:{n}"),
                        Some(Value::Boolean(b)) => format!("b:{b}"),
                    })
                    .collect();
                if seen.insert(key) {
                    result.push((values, src));
                }
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_model::column::ColumnDefinition;
    use baho_model::document::Value;
    use baho_plan::plan::*;

    fn grid_columns() -> Vec<ColumnDefinition> {
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
            ColumnDefinition {
                id: "column-2".to_string(),
                ordinal: 2,
                source_header_raw: Some("Value".to_string()),
                source_header_normalized: Some("value".to_string()),
                display_name: "Value".to_string(),
            },
        ]
    }

    fn text(val: &str) -> Option<Value> {
        Some(Value::Text(val.to_string()))
    }

    fn blank() -> Option<Value> {
        Some(Value::Blank)
    }

    fn none_val() -> Option<Value> {
        None
    }

    #[test]
    fn filter_removes_blank_rows() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![
                vec![text("A"), text("Type A")],
                vec![blank(), text("Type B")],
                vec![text("B"), text("Type C")],
                vec![text(""), text("Type D")],
                vec![text("C"), text("Type E")],
            ],
            source_rows: vec![0, 1, 2, 3, 4],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Filter {
                predicate: Expression::IsNotBlank {
                    column: "column-0".to_string(),
                },
            }],
        };
        let result = execute_plan(&plan, &grid).unwrap();
        assert_eq!(result.rows_output, 3);
        assert_eq!(result.rows_processed, 5);
        assert_eq!(result.view.provenance[0].source_row, 0);
        assert_eq!(result.view.provenance[1].source_row, 2);
        assert_eq!(result.view.provenance[2].source_row, 4);
    }

    #[test]
    fn select_reorders_columns() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B"), text("C")]],
            source_rows: vec![0],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Select {
                columns: vec!["column-2".to_string(), "column-0".to_string()],
            }],
        };
        let result = execute_plan(&plan, &grid).unwrap();
        assert_eq!(result.view.columns.len(), 2);
        assert_eq!(result.view.columns[0].id, "column-2");
        assert_eq!(result.view.columns[1].id, "column-0");
        assert_eq!(result.view.rows[0].values[0], text("C"));
        assert_eq!(result.view.rows[0].values[1], text("A"));
    }

    #[test]
    fn distinct_keeps_first_occurrence() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![
                vec![text("A"), text("Type A")],
                vec![text("B"), text("Type B")],
                vec![text("C"), text("Type A")],
                vec![text("D"), text("Type C")],
                vec![text("E"), text("Type B")],
            ],
            source_rows: vec![0, 1, 2, 3, 4],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Distinct {
                columns: vec!["column-1".to_string()],
                keep: DistinctKeep::First,
            }],
        };
        let result = execute_plan(&plan, &grid).unwrap();
        assert_eq!(result.rows_output, 3);
        assert_eq!(result.view.rows[0].values[1], text("Type A"));
        assert_eq!(result.view.rows[1].values[1], text("Type B"));
        assert_eq!(result.view.rows[2].values[1], text("Type C"));
    }

    #[test]
    fn full_pipeline_filter_select_distinct() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![
                vec![text("X"), text("Type A")],
                vec![text("Y"), text("Type B")],
                vec![text("Z"), text("Type A")],
                vec![text("W"), none_val()],
                vec![text("V"), text("Type C")],
                vec![text("U"), text("Type B")],
                vec![text("T"), text("Type A")],
            ],
            source_rows: vec![0, 1, 2, 3, 4, 5, 6],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
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
        };
        let result = execute_plan(&plan, &grid).unwrap();
        assert_eq!(result.rows_output, 3);
        assert_eq!(result.view.columns.len(), 1);
        assert_eq!(result.view.columns[0].id, "column-1");
        assert_eq!(result.view.rows[0].values[0], text("Type A"));
        assert_eq!(result.view.rows[1].values[0], text("Type B"));
        assert_eq!(result.view.rows[2].values[0], text("Type C"));
        assert_eq!(result.view.provenance[0].source_row, 0);
        assert_eq!(result.view.provenance[1].source_row, 1);
        assert_eq!(result.view.provenance[2].source_row, 4);
    }

    #[test]
    fn blank_exclusion_covers_all_blank_types() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![
                vec![text("valid"), text("A")],
                vec![blank(), text("B")],
                vec![text(""), text("C")],
                vec![text("   "), text("D")],
                vec![none_val(), text("E")],
            ],
            source_rows: vec![0, 1, 2, 3, 4],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Filter {
                predicate: Expression::IsNotBlank {
                    column: "column-0".to_string(),
                },
            }],
        };
        let result = execute_plan(&plan, &grid).unwrap();
        assert_eq!(result.rows_output, 1);
        assert_eq!(result.view.rows[0].values[0], text("valid"));
    }

    #[test]
    fn provenance_preserved_through_filter_and_distinct() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![
                vec![text("A"), text("X")],
                vec![blank(), text("Y")],
                vec![text("B"), text("X")],
                vec![text("C"), text("Z")],
            ],
            source_rows: vec![10, 11, 12, 13],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![
                PlanStep::Filter {
                    predicate: Expression::IsNotBlank {
                        column: "column-0".to_string(),
                    },
                },
                PlanStep::Distinct {
                    columns: vec!["column-1".to_string()],
                    keep: DistinctKeep::First,
                },
            ],
        };
        let result = execute_plan(&plan, &grid).unwrap();
        assert_eq!(result.rows_output, 2);
        assert_eq!(result.view.provenance[0].source_row, 10);
        assert_eq!(result.view.provenance[1].source_row, 13);
    }

    #[test]
    fn empty_input_returns_empty_result() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![],
            source_rows: vec![],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Filter {
                predicate: Expression::IsNotBlank {
                    column: "column-0".to_string(),
                },
            }],
        };
        let result = execute_plan(&plan, &grid).unwrap();
        assert_eq!(result.rows_output, 0);
        assert_eq!(result.rows_processed, 0);
        assert!(result.view.rows.is_empty());
    }

    #[test]
    fn table_not_found_error() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B")]],
            source_rows: vec![0],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "wrong-table".to_string(),
            },
            steps: vec![PlanStep::Select {
                columns: vec!["column-0".to_string()],
            }],
        };
        let err = execute_plan(&plan, &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::TableNotFound { table_id } if table_id == "wrong-table"
        ));
    }

    #[test]
    fn column_not_found_error() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B")]],
            source_rows: vec![0],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Select {
                columns: vec!["column-99".to_string()],
            }],
        };
        let err = execute_plan(&plan, &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::ColumnNotFound { column_id } if column_id == "column-99"
        ));
    }

    #[test]
    fn source_revision_mismatch_error() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "actual-hash".to_string(),
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B"), text("C")]],
            source_rows: vec![0],
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "different-hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Select {
                columns: vec!["column-0".to_string()],
            }],
        };
        let err = execute_plan(&plan, &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::SourceRevisionMismatch { expected, actual }
                if expected == "different-hash" && actual == "actual-hash"
        ));
    }
}
