use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use baho_model::column::{ColumnDefinition, InferredColumnType, ParsedColumn};
use baho_model::diagnostic::{Diagnostic, Severity};
use baho_model::document::{CellAddress, ParsedCell, SourcedCell, Value};
use baho_model::materialized::{MaterializedRow, MaterializedView, RowProvenance};
use baho_model::text_match::{TextMatchPolicy, text_cell_equality};
use baho_plan::plan::{ComparisonOperator, DistinctKeep, Expression, Literal, Plan, PlanStep};
use baho_plan::validation::validate_plan_structure;

use crate::error::{ColumnMixedReason, ExecutionError};

/// Grid data provided to the executor as input.
pub struct GridInput {
    pub table_id: String,
    pub source_revision: String,
    pub source_sheet_index: usize,
    pub columns: Vec<ColumnDefinition>,
    pub rows: Vec<Vec<Option<Value>>>,
    pub source_rows: Vec<usize>,
    /// Typed parse evidence for columns referenced by comparisons, keyed by
    /// column ID. A decimal comparison requires its column here. A text
    /// comparison uses it only to reject numeric columns and to treat valid
    /// decimals in a mixed column as `unknown`; a text-compared column with
    /// no entry falls back to string comparison under the plan's
    /// [`TextMatchPolicy`].
    ///
    /// Each entry aligns its cells with `source_rows` through
    /// `SourcedCell::address.row`, so filtering steps that drop rows never
    /// invalidate the evidence.
    pub typed_columns: BTreeMap<String, ParsedColumn>,
}

/// Three-valued predicate result.
///
/// Truth tables follow the Epic 006 Boolean semantics: `not unknown` is
/// `unknown`, `false and unknown` is `false`, and `true or unknown` is `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruthValue {
    True,
    False,
    Unknown,
}

impl TruthValue {
    pub fn from_bool(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }

    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    pub fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }

    pub fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }
}

impl fmt::Display for TruthValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::True => "true",
            Self::False => "false",
            Self::Unknown => "unknown",
        };
        f.write_str(name)
    }
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
    validate_plan_structure(plan)?;

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

    let typed = build_typed_columns(grid);
    let text_match = plan.text_match_policy();

    let mut col_index: HashMap<String, usize> = grid
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.clone(), i))
        .collect();

    validate_column_refs(plan, &col_index)?;
    validate_typed_predicates(plan, &typed)?;

    let mut diagnostics = Vec::new();
    let rows_processed = grid.rows.len();

    // Working data: (row_values, source_row_index, source_column_indices)
    let source_columns: Vec<usize> = grid.columns.iter().map(|column| column.ordinal).collect();
    let mut working: Vec<(Vec<Option<Value>>, usize, Vec<usize>)> = grid
        .rows
        .iter()
        .zip(grid.source_rows.iter())
        .map(|(row, &src)| (row.clone(), src, source_columns.clone()))
        .collect();

    // Track output columns (updated by select)
    let mut current_columns = grid.columns.clone();

    for step in &plan.steps {
        match step {
            PlanStep::Filter { predicate } => {
                let evaluator = PredicateEvaluator {
                    col_index: &col_index,
                    typed: &typed,
                    text_match,
                };
                let before = working.len();
                let mut retained = Vec::with_capacity(before);
                for (values, source_row, source_columns) in working {
                    let truth = evaluate_expression(predicate, &evaluator, &values, source_row)?;
                    if truth == TruthValue::True {
                        retained.push((values, source_row, source_columns));
                    }
                }
                working = retained;
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
                        let mut col = current_columns[old_idx].clone();
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

    let materialized_rows: Vec<MaterializedRow> = working
        .iter()
        .map(|(values, _, _)| MaterializedRow {
            values: values.clone(),
        })
        .collect();

    let provenance: Vec<RowProvenance> = working
        .iter()
        .map(|(_, source_row, source_columns)| RowProvenance {
            source_row: *source_row,
            source_addresses: source_columns
                .iter()
                .map(|source_column| CellAddress {
                    sheet_index: grid.source_sheet_index,
                    row: *source_row,
                    col: *source_column,
                })
                .collect(),
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
    col_index: &HashMap<String, usize>,
) -> Result<(), ExecutionError> {
    for step in &plan.steps {
        match step {
            PlanStep::Filter { predicate } => {
                for column in predicate.columns() {
                    resolve_column_index(column, col_index)?;
                }
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
    col_index: &HashMap<String, usize>,
) -> Result<usize, ExecutionError> {
    col_index
        .get(column)
        .copied()
        .ok_or_else(|| ExecutionError::ColumnNotFound {
            column_id: column.to_string(),
        })
}

/// Typed parse evidence for one column plus its source-row lookup index.
struct TypedColumnData<'a> {
    parsed: &'a ParsedColumn,
    inferred: InferredColumnType,
    row_to_cell: HashMap<usize, usize>,
}

type TypedColumns<'a> = BTreeMap<&'a str, TypedColumnData<'a>>;

fn build_typed_columns(grid: &GridInput) -> TypedColumns<'_> {
    grid.typed_columns
        .iter()
        .map(|(column_id, parsed)| {
            let row_to_cell = parsed
                .cells
                .iter()
                .enumerate()
                .map(|(index, cell)| (cell.address.row, index))
                .collect();
            (
                column_id.as_str(),
                TypedColumnData {
                    parsed,
                    inferred: parsed.inferred_type(),
                    row_to_cell,
                },
            )
        })
        .collect()
}

/// Decimal comparisons enforce Epic 006 locked decision 5 at the crate
/// boundary: a referenced column must be inferred [`InferredColumnType::Numeric`].
/// Zero parseable nonblank values (Blank or Text) and a malformed share above
/// [`baho_model::column::MIXED_COLUMN_MALFORMED_SHARE_PERCENT`] (Mixed) are
/// refused before evaluation. Malformed values within the accepted threshold
/// are expected here and evaluate to `unknown` per row.
fn validate_typed_predicates(plan: &Plan, typed: &TypedColumns<'_>) -> Result<(), ExecutionError> {
    for step in &plan.steps {
        if let PlanStep::Filter { predicate } = step {
            validate_expression_typed(predicate, typed)?;
        }
    }
    Ok(())
}

fn validate_expression_typed(
    expression: &Expression,
    typed: &TypedColumns<'_>,
) -> Result<(), ExecutionError> {
    match expression {
        Expression::IsNotBlank { .. } => Ok(()),
        Expression::Compare {
            column,
            literal: Literal::Decimal(_),
            ..
        } => {
            let data =
                typed
                    .get(column.as_str())
                    .ok_or_else(|| ExecutionError::MissingTypedParse {
                        column_id: column.clone(),
                    })?;
            let reason = match data.inferred {
                InferredColumnType::Numeric => return Ok(()),
                InferredColumnType::Blank | InferredColumnType::Text => {
                    ColumnMixedReason::NoParseableValues
                }
                InferredColumnType::Mixed => ColumnMixedReason::MalformedShareExceeded,
            };
            Err(ExecutionError::ColumnMixed {
                column_id: column.clone(),
                reason,
            })
        }
        Expression::Compare {
            column,
            literal: Literal::Text(_),
            ..
        } => {
            // A text literal against a numerically profiled column is a plan
            // type mismatch (Epic 006 `plan.type_mismatch`). A column with no
            // typed profile falls back to string comparison under the plan's
            // text-match policy so direct executor callers keep Epic 002
            // behavior.
            if let Some(data) = typed.get(column.as_str()) {
                if data.inferred == InferredColumnType::Numeric {
                    return Err(ExecutionError::TypeMismatch {
                        column_id: column.clone(),
                        expected: "text".to_string(),
                        actual: "numeric".to_string(),
                    });
                }
            }
            Ok(())
        }
        Expression::Compare {
            column,
            literal: Literal::Deferred(raw),
            ..
        } => Err(ExecutionError::UnresolvedLiteral {
            detail: format!("column '{column}' still carries deferred numeric literal '{raw}'"),
        }),
        Expression::And { predicates } | Expression::Or { predicates } => {
            for predicate in predicates {
                validate_expression_typed(predicate, typed)?;
            }
            Ok(())
        }
        Expression::Not { predicate } => validate_expression_typed(predicate, typed),
    }
}

struct PredicateEvaluator<'a> {
    col_index: &'a HashMap<String, usize>,
    typed: &'a TypedColumns<'a>,
    text_match: TextMatchPolicy,
}

impl PredicateEvaluator<'_> {
    fn raw_value<'r>(
        &self,
        values: &'r [Option<Value>],
        column: &str,
    ) -> Result<Option<&'r Value>, ExecutionError> {
        let index = resolve_column_index(column, self.col_index)?;
        Ok(values.get(index).and_then(|value| value.as_ref()))
    }

    fn typed_cell(&self, column: &str, source_row: usize) -> Result<&SourcedCell, ExecutionError> {
        let data = self
            .typed
            .get(column)
            .ok_or_else(|| ExecutionError::MissingTypedParse {
                column_id: column.to_string(),
            })?;
        let index = data.row_to_cell.get(&source_row).ok_or_else(|| {
            ExecutionError::TypedParseMismatch {
                column_id: column.to_string(),
                source_row,
            }
        })?;
        Ok(&data.parsed.cells[*index])
    }
}

/// Evaluates one predicate over one row with Kleene three-valued semantics.
///
/// All Boolean operands are evaluated in listed order; no short-circuiting is
/// applied, and the evaluation itself emits no diagnostics, so parse evidence
/// reporting never depends on evaluation order.
fn evaluate_expression(
    expression: &Expression,
    evaluator: &PredicateEvaluator<'_>,
    values: &[Option<Value>],
    source_row: usize,
) -> Result<TruthValue, ExecutionError> {
    match expression {
        Expression::IsNotBlank { column } => {
            let value = evaluator.raw_value(values, column)?;
            Ok(TruthValue::from_bool(is_not_blank_value(value)))
        }
        Expression::Compare {
            column,
            operator,
            literal,
        } => evaluate_compare(column, *operator, literal, evaluator, values, source_row),
        Expression::And { predicates } => {
            let mut result = TruthValue::True;
            for predicate in predicates {
                result = result.and(evaluate_expression(
                    predicate, evaluator, values, source_row,
                )?);
            }
            Ok(result)
        }
        Expression::Or { predicates } => {
            let mut result = TruthValue::False;
            for predicate in predicates {
                result = result.or(evaluate_expression(
                    predicate, evaluator, values, source_row,
                )?);
            }
            Ok(result)
        }
        Expression::Not { predicate } => {
            Ok(evaluate_expression(predicate, evaluator, values, source_row)?.not())
        }
    }
}

fn evaluate_compare(
    column: &str,
    operator: ComparisonOperator,
    literal: &Literal,
    evaluator: &PredicateEvaluator<'_>,
    values: &[Option<Value>],
    source_row: usize,
) -> Result<TruthValue, ExecutionError> {
    match literal {
        Literal::Text(expected) => {
            // In a materially mixed column, cells that strict-parse as valid
            // decimals are type-incompatible with a text literal and evaluate
            // to `unknown`; malformed non-numeric cells still string-compare.
            let typed = evaluator.typed.get(column);
            let parsed = if typed.is_some_and(|data| data.inferred == InferredColumnType::Mixed) {
                Some(&evaluator.typed_cell(column, source_row)?.parsed)
            } else {
                None
            };
            let value = evaluator.raw_value(values, column)?;
            let equal = text_cell_equality(
                value,
                parsed,
                typed.map(|data| data.inferred),
                expected,
                evaluator.text_match,
            );
            Ok(match (operator, equal) {
                (ComparisonOperator::Equal, Some(equal)) => TruthValue::from_bool(equal),
                (ComparisonOperator::NotEqual, Some(equal)) => TruthValue::from_bool(!equal),
                // Ordered text comparisons are rejected by plan structural
                // validation; unknown keeps evaluation total for direct callers.
                _ => TruthValue::Unknown,
            })
        }
        Literal::Decimal(expected) => {
            let cell = evaluator.typed_cell(column, source_row)?;
            Ok(match &cell.parsed {
                ParsedCell::Valid(value) => {
                    TruthValue::from_bool(apply_operator(operator, value.cmp(expected)))
                }
                ParsedCell::Missing | ParsedCell::Blank | ParsedCell::Malformed { .. } => {
                    TruthValue::Unknown
                }
            })
        }
        Literal::Deferred(raw) => Err(ExecutionError::UnresolvedLiteral {
            detail: format!("column '{column}' still carries deferred numeric literal '{raw}'"),
        }),
    }
}

fn apply_operator(operator: ComparisonOperator, ordering: Ordering) -> bool {
    match operator {
        ComparisonOperator::Equal => ordering == Ordering::Equal,
        ComparisonOperator::NotEqual => ordering != Ordering::Equal,
        ComparisonOperator::Less => ordering == Ordering::Less,
        ComparisonOperator::LessOrEqual => ordering != Ordering::Greater,
        ComparisonOperator::Greater => ordering == Ordering::Greater,
        ComparisonOperator::GreaterOrEqual => ordering != Ordering::Less,
    }
}

fn is_not_blank_value(value: Option<&Value>) -> bool {
    match value {
        None => false,
        Some(Value::Blank) => false,
        Some(Value::Text(s)) => !s.trim().is_empty(),
        Some(_) => true,
    }
}

fn apply_select(
    rows: Vec<(Vec<Option<Value>>, usize, Vec<usize>)>,
    indices: &[usize],
) -> Vec<(Vec<Option<Value>>, usize, Vec<usize>)> {
    rows.into_iter()
        .map(|(values, src, source_columns)| {
            let selected: Vec<Option<Value>> = indices
                .iter()
                .map(|&i| values.get(i).cloned().flatten())
                .collect();
            let selected_source_columns = indices
                .iter()
                .filter_map(|&i| source_columns.get(i).copied())
                .collect();
            (selected, src, selected_source_columns)
        })
        .collect()
}

fn apply_distinct(
    rows: Vec<(Vec<Option<Value>>, usize, Vec<usize>)>,
    indices: &[usize],
    keep: &DistinctKeep,
) -> Vec<(Vec<Option<Value>>, usize, Vec<usize>)> {
    match keep {
        DistinctKeep::First => {
            let mut seen = HashSet::new();
            let mut result = Vec::new();
            for (values, src, source_columns) in rows {
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
                    result.push((values, src, source_columns));
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
    use baho_model::column::ParsedColumn;
    use baho_model::document::Value;
    use baho_model::document::{CellAddress, ParsedCell, SourcedCell};
    use baho_plan::plan::*;
    use std::collections::BTreeMap;

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
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("A"), text("Type A")],
                vec![blank(), text("Type B")],
                vec![text("B"), text("Type C")],
                vec![text(""), text("Type D")],
                vec![text("C"), text("Type E")],
            ],
            source_rows: vec![0, 1, 2, 3, 4],
            typed_columns: BTreeMap::new(),
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
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B"), text("C")]],
            source_rows: vec![0],
            typed_columns: BTreeMap::new(),
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
        assert_eq!(
            result.view.provenance[0]
                .source_addresses
                .iter()
                .map(|address| address.col)
                .collect::<Vec<_>>(),
            vec![2, 0]
        );
    }

    #[test]
    fn distinct_keeps_first_occurrence() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("A"), text("Type A")],
                vec![text("B"), text("Type B")],
                vec![text("C"), text("Type A")],
                vec![text("D"), text("Type C")],
                vec![text("E"), text("Type B")],
            ],
            source_rows: vec![0, 1, 2, 3, 4],
            typed_columns: BTreeMap::new(),
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
            source_sheet_index: 0,
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
            typed_columns: BTreeMap::new(),
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
        assert!(
            result
                .view
                .provenance
                .iter()
                .all(|provenance| provenance.source_addresses[0].col == 1)
        );
    }

    #[test]
    fn blank_exclusion_covers_all_blank_types() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("valid"), text("A")],
                vec![blank(), text("B")],
                vec![text(""), text("C")],
                vec![text("   "), text("D")],
                vec![none_val(), text("E")],
            ],
            source_rows: vec![0, 1, 2, 3, 4],
            typed_columns: BTreeMap::new(),
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
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("A"), text("X")],
                vec![blank(), text("Y")],
                vec![text("B"), text("X")],
                vec![text("C"), text("Z")],
            ],
            source_rows: vec![10, 11, 12, 13],
            typed_columns: BTreeMap::new(),
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
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![],
            source_rows: vec![],
            typed_columns: BTreeMap::new(),
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
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B")]],
            source_rows: vec![0],
            typed_columns: BTreeMap::new(),
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
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B")]],
            source_rows: vec![0],
            typed_columns: BTreeMap::new(),
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
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B"), text("C")]],
            source_rows: vec![0],
            typed_columns: BTreeMap::new(),
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

    fn dec(text: &str) -> baho_model::ExactDecimal {
        baho_model::ExactDecimal::parse(text).unwrap()
    }

    fn decimal_literal(text: &str) -> Literal {
        Literal::Decimal(dec(text))
    }

    fn compare_expr(column: &str, operator: ComparisonOperator, literal: Literal) -> Expression {
        Expression::Compare {
            column: column.to_string(),
            operator,
            literal,
        }
    }

    fn text_equals(column: &str, text: &str) -> Expression {
        compare_expr(
            column,
            ComparisonOperator::Equal,
            Literal::Text(text.to_string()),
        )
    }

    fn filter_plan(schema_version: u32, predicate: Expression) -> Plan {
        Plan {
            schema_version,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Filter { predicate }],
        }
    }

    fn provenance_rows(result: &ExecutionResult) -> Vec<usize> {
        result
            .view
            .provenance
            .iter()
            .map(|provenance| provenance.source_row)
            .collect()
    }

    fn job_income_columns() -> Vec<ColumnDefinition> {
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
                source_header_raw: Some("Job".to_string()),
                source_header_normalized: Some("job".to_string()),
                display_name: "Job".to_string(),
            },
            ColumnDefinition {
                id: "column-2".to_string(),
                ordinal: 2,
                source_header_raw: Some("Annual Income".to_string()),
                source_header_normalized: Some("annual income".to_string()),
                display_name: "Annual Income".to_string(),
            },
        ]
    }

    /// Typed parse evidence for the income column (column-2, ordinal 2),
    /// mirroring ingest output: `None` records a missing cell from a ragged
    /// row, whitespace-only text records a blank cell, and other raw text is
    /// classified by the strict decimal policy.
    fn typed_income_column(values: &[(usize, Option<&str>)]) -> (String, ParsedColumn) {
        let policy = baho_model::NumericParsePolicy::StrictDecimal;
        let cells = values
            .iter()
            .map(|&(row, text)| SourcedCell {
                address: CellAddress {
                    sheet_index: 0,
                    row,
                    col: 2,
                },
                raw_text: text.map(|text| text.to_string()),
                parsed: match text {
                    None => ParsedCell::Missing,
                    Some(text) if text.trim().is_empty() => ParsedCell::Blank,
                    Some(text) => match policy.parse_decimal(text) {
                        Ok(value) => ParsedCell::Valid(value),
                        Err(reason) => ParsedCell::Malformed {
                            raw_text: text.to_string(),
                            reason,
                        },
                    },
                },
            })
            .collect();
        (
            "column-2".to_string(),
            ParsedColumn {
                column_id: "column-2".to_string(),
                policy,
                cells,
            },
        )
    }

    fn typed_columns(entries: Vec<(String, ParsedColumn)>) -> BTreeMap<String, ParsedColumn> {
        entries.into_iter().collect()
    }

    #[test]
    fn kleene_combinators_follow_the_epic_truth_table() {
        use TruthValue::{self as Truth};
        use TruthValue::{False, True, Unknown};

        for (a, expected) in [(True, False), (False, True), (Unknown, Unknown)] {
            assert_eq!(a.not(), expected, "not {a:?}");
        }
        for (a, b, expected) in [
            (False, False, False),
            (False, Unknown, False),
            (False, True, False),
            (Unknown, False, False),
            (Unknown, Unknown, Unknown),
            (Unknown, True, Unknown),
            (True, False, False),
            (True, Unknown, Unknown),
            (True, True, True),
        ] {
            assert_eq!(a.and(b), expected, "{a:?} and {b:?}");
            assert_eq!(b.and(a), expected, "commutative and: {b:?} and {a:?}");
        }
        for (a, b, expected) in [
            (False, False, False),
            (False, Unknown, Unknown),
            (False, True, True),
            (Unknown, False, Unknown),
            (Unknown, Unknown, Unknown),
            (Unknown, True, True),
            (True, False, True),
            (True, Unknown, True),
            (True, True, True),
        ] {
            assert_eq!(a.or(b), expected, "{a:?} or {b:?}");
            assert_eq!(b.or(a), expected, "commutative or: {b:?} or {a:?}");
        }
        assert_eq!(Truth::Unknown.to_string(), "unknown");
        assert_eq!(Truth::True.to_string(), "true");
        assert_eq!(Truth::False.to_string(), "false");
    }

    /// Canonical epic request:
    /// `Job = unemployed or Annual Income < 10000`
    #[test]
    fn epic_row_filter_retains_only_true_rows_with_all_columns() {
        // Truth values per source row for (job = unemployed, income < 10000):
        // 10: (true,  false) -> retained
        // 11: (false, true)  -> retained
        // 12: (false, false) -> dropped
        // 13: (unknown-blank job, unknown-blank income) -> unknown -> dropped
        // 14: (true,  unknown) -> true or unknown = true -> retained
        // 15: (true,  unknown-missing income) -> retained
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("unemployed"), text("50000")],
                vec![text("B"), text("teacher"), text("9999.99")],
                vec![text("C"), text("teacher"), text("12000")],
                vec![text("D"), blank(), blank()],
                vec![text("E"), text("unemployed"), blank()],
                vec![text("F"), text("unemployed"), none_val()],
            ],
            source_rows: vec![10, 11, 12, 13, 14, 15],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (10, Some("50000")),
                (11, Some("9999.99")),
                (12, Some("12000")),
                (13, Some("   ")),
                (14, Some("   ")),
                (15, None),
            ])]),
        };
        let predicate = Expression::Or {
            predicates: vec![
                text_equals("column-1", "unemployed"),
                compare_expr(
                    "column-2",
                    ComparisonOperator::Less,
                    decimal_literal("10000"),
                ),
            ],
        };

        let result = execute_plan(&filter_plan(2, predicate), &grid).unwrap();

        assert_eq!(result.rows_processed, 6);
        assert_eq!(result.rows_output, 4);
        assert_eq!(provenance_rows(&result), [10, 11, 14, 15]);
        // The predicate-only row request retains every source column in
        // source order.
        assert_eq!(
            result
                .view
                .columns
                .iter()
                .map(|c| (c.id.as_str(), c.ordinal))
                .collect::<Vec<_>>(),
            [("column-0", 0), ("column-1", 1), ("column-2", 2)]
        );
        assert!(result.view.rows.iter().all(|row| row.values.len() == 3));
        assert_eq!(result.view.rows[0].values[0], text("A"));
        assert_eq!(result.view.rows[1].values[0], text("B"));
        assert_eq!(result.view.rows[2].values[0], text("E"));
        assert_eq!(result.view.rows[3].values[0], text("F"));
        assert_eq!(result.view.rows[1].values[1], text("teacher"));
        assert_eq!(
            result.view.provenance[3]
                .source_addresses
                .iter()
                .map(|address| (address.sheet_index, address.row, address.col))
                .collect::<Vec<_>>(),
            [(0, 15, 0), (0, 15, 1), (0, 15, 2)]
        );
    }

    #[test]
    fn numeric_comparison_operators_cover_boundaries_negatives_and_unknown() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("n0"), text("j0"), text("10000")],
                vec![text("n1"), text("j1"), text("10000.0")],
                vec![text("n2"), text("j2"), text("9999.99")],
                vec![text("n3"), text("j3"), text("10000.0001")],
                vec![text("n4"), text("j4"), text("-2.5")],
                vec![text("n5"), text("j5"), text("0.001")],
                vec![text("n6"), text("j6"), blank()],
                vec![text("n7"), text("j7"), none_val()],
            ],
            source_rows: vec![0, 1, 2, 3, 4, 5, 6, 7],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("10000")),
                (1, Some("10000.0")),
                (2, Some("9999.99")),
                (3, Some("10000.0001")),
                (4, Some("-2.5")),
                (5, Some("0.001")),
                (6, Some("   ")),
                (7, None),
            ])]),
        };

        let cases: &[(ComparisonOperator, &str, &[usize])] = &[
            (ComparisonOperator::Less, "10000", &[2, 4, 5]),
            (ComparisonOperator::LessOrEqual, "10000", &[0, 1, 2, 4, 5]),
            (ComparisonOperator::Equal, "10000", &[0, 1]),
            (ComparisonOperator::NotEqual, "10000", &[2, 3, 4, 5]),
            (ComparisonOperator::Greater, "10000", &[3]),
            (ComparisonOperator::GreaterOrEqual, "10000", &[0, 1, 3]),
        ];

        for (operator, literal_text, expected_rows) in cases {
            let predicate = compare_expr("column-2", *operator, decimal_literal(literal_text));
            let result = execute_plan(&filter_plan(2, predicate), &grid).unwrap();
            assert_eq!(
                provenance_rows(&result),
                *expected_rows,
                "operator {operator:?} {literal_text}"
            );
            assert_eq!(result.rows_processed, 8, "operator {operator:?}");
        }
    }

    #[test]
    fn numeric_comparison_is_exact_beyond_f64_precision() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("n0"), text("j0"), text("10000000000000000001")],
                vec![text("n1"), text("j1"), text("10000000000000000002")],
            ],
            source_rows: vec![0, 1],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("10000000000000000001")),
                (1, Some("10000000000000000002")),
            ])]),
        };
        for (operator, expected_rows) in [
            (ComparisonOperator::Equal, [1usize]),
            (ComparisonOperator::Less, [0usize]),
        ] {
            let predicate = compare_expr(
                "column-2",
                operator,
                decimal_literal("10000000000000000002"),
            );
            let result = execute_plan(&filter_plan(2, predicate), &grid).unwrap();
            assert_eq!(provenance_rows(&result), expected_rows);
        }
    }

    #[test]
    fn malformed_below_threshold_evaluates_unknown() {
        // 9 valid values and 1 malformed value is accepted by the parse
        // stage; the malformed row must not be coerced into a comparison.
        let mut income = Vec::new();
        for row in 0..9 {
            income.push((row, Some("500")));
        }
        income.push((9, Some("10,000")));
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: (0..9)
                .map(|row| vec![text("n"), text("j"), text("500")])
                .chain(std::iter::once(vec![
                    text("bad"),
                    text("j"),
                    text("10,000"),
                ]))
                .collect(),
            source_rows: (0..10).collect(),
            typed_columns: typed_columns(vec![typed_income_column(&income)]),
        };

        let result = execute_plan(&filter_plan(2, income_less_than("1000")), &grid)
            .expect("parse of valid literals always succeeds");

        assert_eq!(result.rows_processed, 10);
        assert_eq!(result.rows_output, 9);
        assert_eq!(provenance_rows(&result), (0..9).collect::<Vec<_>>());
    }

    #[test]
    fn text_literal_on_numeric_profiled_column_is_type_mismatch() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![vec![text("A"), text("j"), text("500")]],
            source_rows: vec![0],
            typed_columns: typed_columns(vec![typed_income_column(&[(0, Some("500"))])]),
        };

        let err = execute_plan(&filter_plan(2, text_equals("column-2", "500")), &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::TypeMismatch { ref column_id, .. } if column_id == "column-2"
        ));
    }

    #[test]
    fn text_literal_on_text_profiled_column_still_string_compares() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("j"), text("unemployed")],
                vec![text("B"), text("j"), text("teacher")],
                vec![text("C"), text("j"), text("unemployed ")],
            ],
            source_rows: vec![0, 1, 2],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("unemployed")),
                (1, Some("teacher")),
                (2, Some("unemployed ")),
            ])]),
        };

        let result = execute_plan(
            &filter_plan(2, text_equals("column-2", "unemployed")),
            &grid,
        )
        .unwrap();
        assert_eq!(provenance_rows(&result), [0]);
    }

    #[test]
    fn mixed_column_text_comparison_marks_numeric_cells_unknown() {
        // 2 valid (500, 600) and 1 malformed (abc): a 1/3 malformed share
        // exceeds the 10% limit, so the column is materially mixed and text
        // comparison applies per cell.
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("j"), text("500")],
                vec![text("B"), text("j"), text("abc")],
                vec![text("C"), text("j"), text("600")],
            ],
            source_rows: vec![0, 1, 2],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("500")),
                (1, Some("abc")),
                (2, Some("600")),
            ])]),
        };

        let text_match =
            execute_plan(&filter_plan(2, text_equals("column-2", "abc")), &grid).unwrap();
        assert_eq!(provenance_rows(&text_match), [1]);

        // Numeric cells are type-incompatible with a text literal, so even an
        // exact surface match must not retain the row.
        let numeric_surface_match =
            execute_plan(&filter_plan(2, text_equals("column-2", "500")), &grid).unwrap();
        assert!(numeric_surface_match.view.provenance.is_empty());
    }

    #[test]
    fn mixed_column_text_comparison_folds_case_under_v3() {
        // 2 valid (500, 600) and 1 malformed (Abc): a 1/3 malformed share is
        // materially mixed. Under v3 the malformed cell's string comparison
        // folds case; valid numeric cells stay type-incompatible and unknown,
        // and raw spellings reach the output unchanged.
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("j"), text("500")],
                vec![text("B"), text("j"), text("Abc")],
                vec![text("C"), text("j"), text("600")],
            ],
            source_rows: vec![0, 1, 2],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("500")),
                (1, Some("Abc")),
                (2, Some("600")),
            ])]),
        };

        let folded = execute_plan(&filter_plan(3, text_equals("column-2", "abc")), &grid).unwrap();
        assert_eq!(provenance_rows(&folded), [1]);
        assert_eq!(folded.view.rows[0].values[2], text("Abc"));

        let exact = execute_plan(&filter_plan(2, text_equals("column-2", "abc")), &grid).unwrap();
        assert!(exact.view.provenance.is_empty());

        let numeric_surface_match =
            execute_plan(&filter_plan(3, text_equals("column-2", "500")), &grid).unwrap();
        assert!(numeric_surface_match.view.provenance.is_empty());
    }

    #[test]
    fn text_comparison_folds_case_but_not_whitespace_under_v3() {
        // Rows: 0 exact match, 1 trailing whitespace, 2 different case,
        // 3 blank, 4 numeric cell against text literal, 5 missing cell,
        // 6 different text. Under plan schema version 3 the case variant
        // matches and is equal for `!=`; trailing whitespace is never
        // trimmed (Epic 008 locked decision 11).
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("n0"), text("unemployed")],
                vec![text("n1"), text("unemployed ")],
                vec![text("n2"), text("Unemployed")],
                vec![text("n3"), blank()],
                vec![text("n4"), Some(Value::Number(1.0))],
                vec![text("n5"), none_val()],
                vec![text("n6"), text("employed")],
            ],
            source_rows: vec![0, 1, 2, 3, 4, 5, 6],
            typed_columns: BTreeMap::new(),
        };

        let equal = execute_plan(
            &filter_plan(3, text_equals("column-1", "unemployed")),
            &grid,
        )
        .unwrap();
        assert_eq!(provenance_rows(&equal), [0, 2]);
        // Materialized values retain the original raw spelling; folding is
        // comparison-only.
        assert_eq!(equal.view.rows[1].values[1], text("Unemployed"));

        let not_equal = execute_plan(
            &filter_plan(
                3,
                compare_expr(
                    "column-1",
                    ComparisonOperator::NotEqual,
                    Literal::Text("unemployed".to_string()),
                ),
            ),
            &grid,
        )
        .unwrap();
        // The case variant folds to equal so `!=` drops it; the trailing
        // whitespace row stays unequal and is retained. Blank, missing, and
        // numeric cells stay unknown and are retained by neither operator.
        assert_eq!(provenance_rows(&not_equal), [1, 6]);
    }

    #[test]
    fn text_comparison_stays_exact_under_v2() {
        // Same grid as the v3 folding test: plan schema version 2 retains
        // exact case-sensitive equality, so the case-variant row matches for
        // `!=` and never for `=`.
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("n0"), text("unemployed")],
                vec![text("n1"), text("unemployed ")],
                vec![text("n2"), text("Unemployed")],
                vec![text("n3"), blank()],
                vec![text("n4"), Some(Value::Number(1.0))],
                vec![text("n5"), none_val()],
                vec![text("n6"), text("employed")],
            ],
            source_rows: vec![0, 1, 2, 3, 4, 5, 6],
            typed_columns: BTreeMap::new(),
        };

        let equal = execute_plan(
            &filter_plan(2, text_equals("column-1", "unemployed")),
            &grid,
        )
        .unwrap();
        assert_eq!(provenance_rows(&equal), [0]);

        let not_equal = execute_plan(
            &filter_plan(
                2,
                compare_expr(
                    "column-1",
                    ComparisonOperator::NotEqual,
                    Literal::Text("unemployed".to_string()),
                ),
            ),
            &grid,
        )
        .unwrap();
        assert_eq!(provenance_rows(&not_equal), [1, 2, 6]);
    }

    #[test]
    fn text_comparison_folds_non_ascii_case_but_not_full_case_folding_under_v3() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("n0"), text("Été")],
                vec![text("n1"), text("ÉTÉ")],
                vec![text("n2"), text("ß")],
                vec![text("n3"), text("SS")],
                vec![text("n4"), text("ss")],
            ],
            source_rows: vec![0, 1, 2, 3, 4],
            typed_columns: BTreeMap::new(),
        };

        // Unicode full lowercase folds the accented pair.
        let accented =
            execute_plan(&filter_plan(3, text_equals("column-1", "ÉTÉ")), &grid).unwrap();
        assert_eq!(provenance_rows(&accented), [0, 1]);

        // `ß` lowercases to itself (lowercase conversion, not case folding),
        // so it never matches `SS`/`ss` (Epic 008 locked decision 1).
        let sharp_s = execute_plan(&filter_plan(3, text_equals("column-1", "ß")), &grid).unwrap();
        assert_eq!(provenance_rows(&sharp_s), [2]);

        let ascii_pair =
            execute_plan(&filter_plan(3, text_equals("column-1", "ss")), &grid).unwrap();
        assert_eq!(provenance_rows(&ascii_pair), [3, 4]);
    }

    #[test]
    fn negation_of_false_and_unknown_is_true_and_retains_the_row() {
        // Row 0: income is blank (unknown) and the job comparison is false,
        // so the conjunction must be false (not unknown) for negation to
        // retain the row: not (false and unknown) = not false = true.
        // Row 1 has true operands so its negation drops it.
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("teacher"), blank()],
                vec![text("B"), text("unemployed"), text("5000")],
            ],
            source_rows: vec![7, 8],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (7, Some("   ")),
                (8, Some("5000")),
            ])]),
        };
        let predicate = Expression::Not {
            predicate: Box::new(Expression::And {
                predicates: vec![
                    income_less_than("10000"),
                    text_equals("column-1", "unemployed"),
                ],
            }),
        };

        let result = execute_plan(&filter_plan(2, predicate), &grid).unwrap();
        assert_eq!(result.rows_output, 1);
        assert_eq!(provenance_rows(&result), [7]);
    }

    #[test]
    fn true_and_unknown_is_unknown_so_both_rows_drop() {
        // Row 0: name comparison true and income comparison unknown, so the
        // conjunction must be unknown (dropped). Row 1: name comparison
        // false, so the conjunction is false (dropped).
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("teacher"), blank()],
                vec![text("Z"), text("teacher"), text("20000")],
            ],
            source_rows: vec![0, 1],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("   ")),
                (1, Some("20000")),
            ])]),
        };
        let predicate = Expression::And {
            predicates: vec![text_equals("column-0", "A"), income_less_than("10000")],
        };

        let result = execute_plan(&filter_plan(2, predicate), &grid).unwrap();
        assert_eq!(result.rows_output, 0);
    }

    #[test]
    fn negated_unknown_is_unknown_and_negated_false_is_true() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            // Row 0: blank income -> not (unknown < 10000) = unknown -> dropped.
            // Row 1: income 20000 -> not (false) = true -> retained.
            rows: vec![
                vec![text("A"), text("j"), blank()],
                vec![text("B"), text("j"), text("20000")],
            ],
            source_rows: vec![0, 1],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("   ")),
                (1, Some("20000")),
            ])]),
        };

        let result = execute_plan(
            &filter_plan(
                2,
                Expression::Not {
                    predicate: Box::new(income_less_than("10000")),
                },
            ),
            &grid,
        )
        .unwrap();
        assert_eq!(result.rows_output, 1);
        assert_eq!(provenance_rows(&result), [1]);
    }

    #[test]
    fn false_or_unknown_is_unknown_through_negation() {
        // Row 0: name differs (false) and income is blank (unknown), so the
        // disjunction must stay unknown and its negation must drop the row;
        // if false or unknown collapsed to false, the negation would wrongly
        // retain it. Row 1 matches, so its disjunction is true and the
        // negation drops it.
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("teacher"), blank()],
                vec![text("zzz"), text("teacher"), text("5000")],
            ],
            source_rows: vec![0, 1],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("   ")),
                (1, Some("5000")),
            ])]),
        };
        let predicate = Expression::Not {
            predicate: Box::new(Expression::Or {
                predicates: vec![text_equals("column-0", "zzz"), income_less_than("10000")],
            }),
        };

        let result = execute_plan(&filter_plan(2, predicate), &grid).unwrap();
        assert_eq!(result.rows_output, 0);
    }

    #[test]
    fn compare_requires_typed_parse_data() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![vec![text("A"), text("j"), text("500")]],
            source_rows: vec![0],
            typed_columns: BTreeMap::new(),
        };

        let err = execute_plan(&filter_plan(2, income_less_than("10000")), &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::MissingTypedParse { ref column_id } if column_id == "column-2"
        ));
    }

    #[test]
    fn decimal_comparison_refuses_column_without_parseable_values() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("j"), text("10,000")],
                vec![text("B"), text("j"), text("oops")],
            ],
            source_rows: vec![0, 1],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("10,000")),
                (1, Some("oops")),
            ])]),
        };

        let err = execute_plan(&filter_plan(2, income_less_than("10000")), &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::ColumnMixed {
                ref column_id,
                reason: ColumnMixedReason::NoParseableValues,
            } if column_id == "column-2"
        ));
        assert_eq!(err.diagnostic_code(), "parse.column_mixed");
    }

    #[test]
    fn decimal_comparison_refuses_materially_mixed_column() {
        // 2 valid and 1 malformed value: a malformed share of 1/3 exceeds the
        // 10% limit, so a direct caller is refused before evaluation.
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("j"), text("500")],
                vec![text("B"), text("j"), text("abc")],
                vec![text("C"), text("j"), text("600")],
            ],
            source_rows: vec![0, 1, 2],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("500")),
                (1, Some("abc")),
                (2, Some("600")),
            ])]),
        };

        let err = execute_plan(&filter_plan(2, income_less_than("1000")), &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::ColumnMixed {
                ref column_id,
                reason: ColumnMixedReason::MalformedShareExceeded,
            } if column_id == "column-2"
        ));
        assert_eq!(err.diagnostic_code(), "parse.column_mixed");
    }

    #[test]
    fn decimal_comparison_refuses_all_blank_column() {
        // Zero nonblank values is a decision-5 refusal, not a Numeric column.
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("j"), blank()],
                vec![text("B"), text("j"), none_val()],
            ],
            source_rows: vec![0, 1],
            typed_columns: typed_columns(vec![typed_income_column(&[(0, Some("   ")), (1, None)])]),
        };

        let err = execute_plan(&filter_plan(2, income_less_than("10000")), &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::ColumnMixed {
                ref column_id,
                reason: ColumnMixedReason::NoParseableValues,
            } if column_id == "column-2"
        ));
        assert_eq!(err.diagnostic_code(), "parse.column_mixed");
    }

    #[test]
    fn typed_parse_must_cover_evaluated_source_rows() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("j"), text("500")],
                vec![text("B"), text("j"), text("600")],
                vec![text("C"), text("j"), text("700")],
            ],
            source_rows: vec![0, 1, 2],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("500")),
                (1, Some("600")),
            ])]),
        };

        let err = execute_plan(&filter_plan(2, income_less_than("10000")), &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::TypedParseMismatch { ref column_id, source_row: 2 }
                if column_id == "column-2"
        ));
    }

    #[test]
    fn v1_plan_rejects_v2_predicate_kinds_at_execution() {
        use baho_plan::validation::PlanValidationError;
        use std::error::Error as _;

        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B"), text("C")]],
            source_rows: vec![0],
            typed_columns: BTreeMap::new(),
        };
        let plan = filter_plan(1, text_equals("column-1", "B"));

        let err = execute_plan(&plan, &grid).unwrap_err();
        assert!(matches!(err, ExecutionError::InvalidPlan(_)));
        let validation_error = err
            .source()
            .expect("source chain")
            .downcast_ref::<PlanValidationError>()
            .expect("source is a plan validation error");
        assert_eq!(validation_error.diagnostic_code(), "plan.invalid");
    }

    #[test]
    fn execute_plan_rejects_empty_boolean_operands() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![vec![text("A"), text("B"), text("C")]],
            source_rows: vec![0],
            typed_columns: BTreeMap::new(),
        };
        let plan = filter_plan(2, Expression::And { predicates: vec![] });

        let err = execute_plan(&plan, &grid).unwrap_err();
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(
                baho_plan::validation::PlanValidationError::EmptyBooleanOperands { .. }
            )
        ));
    }

    #[test]
    fn v2_filter_with_v1_predicate_leaf_matches_v1_semantics() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: grid_columns(),
            rows: vec![
                vec![text("A"), text("x")],
                vec![blank(), text("y")],
                vec![text(""), text("z")],
            ],
            source_rows: vec![0, 1, 2],
            typed_columns: BTreeMap::new(),
        };

        let plain = execute_plan(
            &filter_plan(
                2,
                Expression::IsNotBlank {
                    column: "column-0".to_string(),
                },
            ),
            &grid,
        )
        .unwrap();
        assert_eq!(provenance_rows(&plain), [0]);

        // `is_not_blank` stays two-valued under negation: blank cells make
        // the leaf false, so its negation is true and retains the row.
        let negated = execute_plan(
            &filter_plan(
                2,
                Expression::Not {
                    predicate: Box::new(Expression::IsNotBlank {
                        column: "column-0".to_string(),
                    }),
                },
            ),
            &grid,
        )
        .unwrap();
        assert_eq!(provenance_rows(&negated), [1, 2]);
    }

    #[test]
    fn predicate_only_filter_preserves_row_order_and_provenance() {
        let columns = job_income_columns();
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: columns.clone(),
            rows: vec![
                vec![text("A"), text("j"), text("20000")],
                vec![text("B"), text("j"), text("5000")],
                vec![text("C"), text("j"), text("8000")],
            ],
            source_rows: vec![10, 3, 42],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (10, Some("20000")),
                (3, Some("5000")),
                (42, Some("8000")),
            ])]),
        };

        let result = execute_plan(&filter_plan(2, income_less_than("10000")), &grid).unwrap();

        assert_eq!(provenance_rows(&result), [3, 42]);
        assert_eq!(result.view.columns, columns);
        assert_eq!(result.view.rows.len(), 2);
        assert_eq!(result.view.rows[0].values[0], text("B"));
        assert_eq!(result.view.rows[1].values[0], text("C"));
        for provenance in &result.view.provenance {
            assert_eq!(
                provenance
                    .source_addresses
                    .iter()
                    .map(|address| address.col)
                    .collect::<Vec<_>>(),
                [0, 1, 2]
            );
        }
        assert_eq!(result.view.provenance[1].source_addresses[0].row, 42);
    }

    #[test]
    fn typed_comparison_accounts_for_prior_steps() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("n0"), text("j0"), text("20000")],
                vec![text("n1"), text("j1"), text("5000")],
                vec![text("n2"), text("j2"), text("9999")],
                vec![text("n3"), text("j3"), text("6000")],
            ],
            source_rows: vec![0, 1, 2, 3],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("20000")),
                (1, Some("5000")),
                (2, Some("9999")),
                (3, Some("6000")),
            ])]),
        };
        let plan = Plan {
            schema_version: 2,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![
                PlanStep::Filter {
                    predicate: income_less_than("10000"),
                },
                PlanStep::Select {
                    columns: ["column-0", "column-2"]
                        .iter()
                        .map(|c| c.to_string())
                        .collect(),
                },
                PlanStep::Filter {
                    predicate: compare_expr(
                        "column-2",
                        ComparisonOperator::GreaterOrEqual,
                        decimal_literal("6000"),
                    ),
                },
            ],
        };

        let result = execute_plan(&plan, &grid).unwrap();
        assert_eq!(provenance_rows(&result), [2, 3]);
        assert_eq!(result.view.columns.len(), 2);
        assert_eq!(result.view.rows[0].values[1], text("9999"));
        assert_eq!(result.view.rows[1].values[1], text("6000"));
    }

    #[test]
    fn repeated_execution_is_deterministic() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: job_income_columns(),
            rows: vec![
                vec![text("A"), text("unemployed"), text("500")],
                vec![text("B"), text("teacher"), text("9999.99")],
                vec![text("C"), text("teacher"), blank()],
            ],
            source_rows: vec![0, 1, 2],
            typed_columns: typed_columns(vec![typed_income_column(&[
                (0, Some("500")),
                (1, Some("9999.99")),
                (2, Some("   ")),
            ])]),
        };
        let predicate = Expression::Or {
            predicates: vec![
                text_equals("column-1", "unemployed"),
                income_less_than("10000"),
            ],
        };

        let first = execute_plan(&filter_plan(2, predicate.clone()), &grid).unwrap();
        let second = execute_plan(&filter_plan(2, predicate), &grid).unwrap();
        assert_eq!(first.view, second.view);
        assert_eq!(first.rows_output, second.rows_output);
        assert_eq!(first.diagnostics.len(), second.diagnostics.len());
    }

    fn income_less_than(text: &str) -> Expression {
        compare_expr("column-2", ComparisonOperator::Less, decimal_literal(text))
    }

    #[test]
    fn select_only_preserves_blanks_and_duplicates() {
        let grid = GridInput {
            table_id: "table-0".to_string(),
            source_revision: "hash".to_string(),
            source_sheet_index: 0,
            columns: vec![ColumnDefinition {
                id: "col-income".to_string(),
                ordinal: 0,
                source_header_raw: Some("Income".to_string()),
                source_header_normalized: Some("income".to_string()),
                display_name: "Income".to_string(),
            }],
            rows: vec![
                vec![text("50000")],
                vec![blank()],
                vec![text("60000")],
                vec![text("50000")],
                vec![blank()],
                vec![text("70000")],
            ],
            source_rows: vec![1, 2, 3, 4, 5, 6],
            typed_columns: BTreeMap::new(),
        };
        let plan = Plan {
            schema_version: 1,
            source: PlanSource {
                revision: "hash".to_string(),
                table_id: "table-0".to_string(),
            },
            steps: vec![PlanStep::Select {
                columns: vec!["col-income".to_string()],
            }],
        };
        let result = execute_plan(&plan, &grid).unwrap();

        // All 6 rows preserved including blanks and duplicates
        assert_eq!(result.rows_output, 6);
        assert_eq!(result.rows_processed, 6);

        // Values in source order
        assert_eq!(result.view.rows[0].values[0], text("50000"));
        assert_eq!(result.view.rows[1].values[0], blank());
        assert_eq!(result.view.rows[2].values[0], text("60000"));
        assert_eq!(result.view.rows[3].values[0], text("50000"));
        assert_eq!(result.view.rows[4].values[0], blank());
        assert_eq!(result.view.rows[5].values[0], text("70000"));

        // Provenance is correct
        assert_eq!(result.view.provenance[0].source_row, 1);
        assert_eq!(result.view.provenance[1].source_row, 2);
        assert_eq!(result.view.provenance[2].source_row, 3);
        assert_eq!(result.view.provenance[3].source_row, 4);
        assert_eq!(result.view.provenance[4].source_row, 5);
        assert_eq!(result.view.provenance[5].source_row, 6);

        // All provenance addresses point to column 0
        assert!(
            result
                .view
                .provenance
                .iter()
                .all(|p| p.source_addresses.len() == 1 && p.source_addresses[0].col == 0)
        );
    }
}
