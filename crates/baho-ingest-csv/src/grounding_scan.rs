//! Complete selected-table scans for grounding evidence.
//!
//! The caller supplies every selected data row, including rows beyond the
//! bounded inspection sample. No result is returned if the source ends in an
//! error or the configured cell budget is exhausted before the iterator ends.

use std::collections::{BTreeMap, BTreeSet};

use baho_model::column::{
    ColumnDefinition, MIXED_COLUMN_MALFORMED_SHARE_PERCENT, NumericParsePolicy,
};
use baho_model::column::{InferredColumnType, ParsedColumn};
use baho_model::document::{CellAddress, Value};
use baho_model::text_match::{TextMatchPolicy, text_cell_equality};

use crate::config::NormalizationConfig;
use crate::inspector::LogicalRecord;
use crate::policy_selection::PolicyEvidenceAccumulator;

/// Compact type and match evidence from one verified selected-table pass.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamedColumnEvidence {
    pub column_id: String,
    pub ordinal: usize,
    pub inferred_type: InferredColumnType,
    pub numeric_policy: Option<NumericParsePolicy>,
    pub nonblank_count: u64,
    pub flag_shaped: bool,
    pub matches: Vec<ColumnValueMatches>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamedGroundingEvidence {
    pub scan: CompleteScan,
    pub columns: Vec<StreamedColumnEvidence>,
}

const POLICIES: [NumericParsePolicy; 3] = [
    NumericParsePolicy::StrictDecimal,
    NumericParsePolicy::DotDecimalCommaGrouping,
    NumericParsePolicy::CommaDecimalDotGrouping,
];

struct PolicyCounts {
    valid: u64,
    malformed: u64,
    matches: Vec<ColumnValueMatches>,
}

struct PendingColumn {
    column_id: String,
    ordinal: usize,
    shape: PolicyEvidenceAccumulator,
    nonblank: u64,
    flag_shaped: bool,
    policies: Vec<PolicyCounts>,
}

/// Scan every selected data row once. Only counts and bounded coordinates are
/// retained; no per-row typed cells or copied table is built. Every eligible
/// cell counts once against the cumulative grounding budget.
pub fn scan_grounding_evidence<I, E>(
    selected_rows: I,
    columns: &[ColumnDefinition],
    literals: &[&str],
    policy: TextMatchPolicy,
    normalization: &NormalizationConfig,
    max_grounding_cells_scanned: u64,
    max_sample_cells_per_column: usize,
) -> Result<StreamedGroundingEvidence, CompleteScanError<E>>
where
    I: IntoIterator<Item = Result<LogicalRecord, E>>,
{
    let mut pending = columns
        .iter()
        .map(|column| PendingColumn {
            column_id: column.id.clone(),
            ordinal: column.ordinal,
            shape: PolicyEvidenceAccumulator::default(),
            nonblank: 0,
            flag_shaped: true,
            policies: POLICIES
                .iter()
                .map(|_| PolicyCounts {
                    valid: 0,
                    malformed: 0,
                    matches: literals
                        .iter()
                        .map(|_| ColumnValueMatches {
                            column_id: column.id.clone(),
                            ordinal: column.ordinal,
                            match_count: 0,
                            sample_cells: Vec::new(),
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    pending.sort_by_key(|column| column.ordinal);
    let ordinals = pending
        .iter()
        .map(|column| column.ordinal)
        .collect::<Vec<_>>();
    let scan = scan_complete_table(
        selected_rows,
        &ordinals,
        max_grounding_cells_scanned,
        |source_row, ordinal, raw| {
            let Some(raw) = raw.filter(|raw| !normalization.is_blank(raw)) else {
                return;
            };
            let index = ordinals.binary_search(&ordinal).expect("eligible ordinal");
            let column = &mut pending[index];
            column.nonblank += 1;
            column.flag_shaped &= policy.text_eq(raw, "true") || policy.text_eq(raw, "false");
            column.shape.observe(raw);
            for (policy_index, numeric_policy) in POLICIES.iter().enumerate() {
                let counts = &mut column.policies[policy_index];
                if numeric_policy.parse_decimal(raw).is_ok() {
                    counts.valid += 1;
                } else {
                    counts.malformed += 1;
                    for (literal, matches) in literals.iter().zip(&mut counts.matches) {
                        if policy.text_eq(raw, literal) {
                            matches.match_count += 1;
                            if matches.sample_cells.len() < max_sample_cells_per_column {
                                matches.sample_cells.push(CellAddress {
                                    sheet_index: 0,
                                    row: source_row,
                                    col: ordinal,
                                });
                            }
                        }
                    }
                }
            }
        },
    )?;
    let columns = pending
        .into_iter()
        .map(|column| {
            let selection = column.shape.finish().ok();
            let selected_policy = selection.as_ref().map(|selection| selection.policy);
            let policy_index = selected_policy
                .and_then(|policy| POLICIES.iter().position(|candidate| *candidate == policy))
                .unwrap_or(0);
            let counts = &column.policies[policy_index];
            let inferred_type = if column.nonblank == 0 {
                InferredColumnType::Blank
            } else if counts.valid == 0 {
                InferredColumnType::Text
            } else if u128::from(counts.malformed) * 100
                > u128::from(column.nonblank) * u128::from(MIXED_COLUMN_MALFORMED_SHARE_PERCENT)
            {
                InferredColumnType::Mixed
            } else {
                InferredColumnType::Numeric
            };
            let matches = if inferred_type == InferredColumnType::Numeric {
                counts
                    .matches
                    .iter()
                    .map(|found| ColumnValueMatches {
                        column_id: found.column_id.clone(),
                        ordinal: found.ordinal,
                        match_count: 0,
                        sample_cells: Vec::new(),
                    })
                    .collect()
            } else {
                counts.matches.clone()
            };
            StreamedColumnEvidence {
                column_id: column.column_id,
                ordinal: column.ordinal,
                inferred_type,
                nonblank_count: column.nonblank,
                numeric_policy: (inferred_type == InferredColumnType::Numeric)
                    .then_some(selected_policy)
                    .flatten(),
                flag_shaped: column.flag_shaped && column.nonblank > 0,
                matches,
            }
        })
        .collect();
    Ok(StreamedGroundingEvidence { scan, columns })
}

/// Counts produced only after every selected row and eligible column is visited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteScan {
    pub selected_rows_scanned: u64,
    pub total_cells_scanned: u64,
    /// Zero-based eligible columns in deterministic source order.
    pub eligible_columns: Vec<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum CompleteScanError<E> {
    #[error("selected-table source scan failed: {0}")]
    Source(E),
    #[error("grounding scan exceeds max_grounding_cells_scanned ({limit})")]
    ResourceLimitExceeded { limit: u64, cells_scanned: u64 },
    #[error("typed evidence is missing for column {column_id} at source row {source_row}")]
    MissingTypedCell {
        column_id: String,
        source_row: usize,
    },
}

/// One selected column. Parsed evidence is required when the column is
/// materially mixed, because numeric cells cannot satisfy text equality.
pub struct LookupColumn<'a> {
    pub column_id: &'a str,
    pub ordinal: usize,
    pub parsed: Option<&'a ParsedColumn>,
}

/// Bounded evidence for one column; observed cell values are never retained.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnValueMatches {
    pub column_id: String,
    pub ordinal: usize,
    pub match_count: u64,
    pub sample_cells: Vec<CellAddress>,
}

/// Exact matches certified only after every eligible selected-table cell was
/// scanned. Columns remain in source order, including columns with no match.
#[derive(Debug, Clone, PartialEq)]
pub struct ExactValueLookup {
    pub scan: CompleteScan,
    pub columns: Vec<ColumnValueMatches>,
}

/// Complete, bounded-memory classification of a selected column's flag shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnFlagShape {
    pub ordinal: usize,
    pub nonblank_count: u64,
    pub flag_shaped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagShapeLookup {
    pub scan: CompleteScan,
    pub columns: Vec<ColumnFlagShape>,
}

/// Classify selected columns over every data row. Any nonblank value other
/// than true or false makes a column non-flag-shaped; a wholly blank column
/// is also not flag-shaped. An incomplete scan returns no classifications.
pub fn scan_flag_shapes<I, E>(
    selected_rows: I,
    column_ordinals: &[usize],
    policy: TextMatchPolicy,
    normalization: &NormalizationConfig,
    max_grounding_cells_scanned: u64,
) -> Result<FlagShapeLookup, CompleteScanError<E>>
where
    I: IntoIterator<Item = Result<LogicalRecord, E>>,
{
    let mut ordinals = column_ordinals.to_vec();
    ordinals.sort_unstable();
    ordinals.dedup();
    let mut columns = ordinals
        .iter()
        .map(|&ordinal| ColumnFlagShape {
            ordinal,
            nonblank_count: 0,
            flag_shaped: true,
        })
        .collect::<Vec<_>>();
    let scan = scan_complete_table(
        selected_rows,
        &ordinals,
        max_grounding_cells_scanned,
        |_, ordinal, raw| {
            let Some(raw) = raw.filter(|raw| !normalization.is_blank(raw)) else {
                return;
            };
            let index = ordinals.binary_search(&ordinal).expect("eligible ordinal");
            let column = &mut columns[index];
            column.nonblank_count += 1;
            column.flag_shaped &= policy.text_eq(raw, "true") || policy.text_eq(raw, "false");
        },
    )?;
    for column in &mut columns {
        column.flag_shaped &= column.nonblank_count > 0;
    }
    Ok(FlagShapeLookup { scan, columns })
}

/// Search a selected-table stream using the same text-cell rule as execution.
/// The caller must supply all selected data rows and every column that could
/// satisfy the compiled equality predicate. A profile sample is insufficient.
/// `max_sample_cells_per_column` bounds retained coordinates, not the scan.
pub fn lookup_exact_value<I, E>(
    selected_rows: I,
    columns: &[LookupColumn<'_>],
    literal: &str,
    policy: TextMatchPolicy,
    normalization: &NormalizationConfig,
    max_grounding_cells_scanned: u64,
    max_sample_cells_per_column: usize,
) -> Result<ExactValueLookup, CompleteScanError<E>>
where
    I: IntoIterator<Item = Result<LogicalRecord, E>>,
{
    let mut ordered = columns.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|column| column.ordinal);
    ordered.dedup_by_key(|column| column.ordinal);
    let ordinals = ordered
        .iter()
        .map(|column| column.ordinal)
        .collect::<Vec<_>>();
    let typed_rows = ordered
        .iter()
        .map(|column| {
            column.parsed.map(|parsed| {
                parsed
                    .cells
                    .iter()
                    .map(|cell| (cell.address.row, &cell.parsed))
                    .collect::<BTreeMap<_, _>>()
            })
        })
        .collect::<Vec<_>>();
    let mut matches = ordered
        .iter()
        .map(|column| ColumnValueMatches {
            column_id: column.column_id.to_owned(),
            ordinal: column.ordinal,
            match_count: 0,
            sample_cells: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut missing_typed = None;
    let scan = scan_complete_table(
        selected_rows,
        &ordinals,
        max_grounding_cells_scanned,
        |source_row, ordinal, raw| {
            let index = ordinals.binary_search(&ordinal).expect("eligible ordinal");
            let inferred = ordered[index].parsed.map(ParsedColumn::inferred_type);
            // Execution rejects text comparisons against numeric-profiled
            // columns before evaluating cells, so they cannot be offered as
            // selectable text-equality interpretations.
            if inferred == Some(InferredColumnType::Numeric) {
                return;
            }
            let parsed = typed_rows[index]
                .as_ref()
                .and_then(|rows| rows.get(&source_row))
                .copied();
            if inferred == Some(InferredColumnType::Mixed) && parsed.is_none() {
                missing_typed.get_or_insert((index, source_row));
                return;
            }
            let value = raw.and_then(|text| {
                (!normalization.is_blank(text)).then(|| Value::Text(text.to_owned()))
            });
            if text_cell_equality(value.as_ref(), parsed, inferred, literal, policy) == Some(true) {
                matches[index].match_count += 1;
                if matches[index].sample_cells.len() < max_sample_cells_per_column {
                    matches[index].sample_cells.push(CellAddress {
                        sheet_index: 0,
                        row: source_row,
                        col: ordinal,
                    });
                }
            }
        },
    )?;
    if let Some((index, source_row)) = missing_typed {
        return Err(CompleteScanError::MissingTypedCell {
            column_id: ordered[index].column_id.to_owned(),
            source_row,
        });
    }
    Ok(ExactValueLookup {
        scan,
        columns: matches,
    })
}

/// Visit each eligible cell in each selected row, including physically absent
/// fields as `None`. `eligible_columns` must contain all columns whose target
/// predicate could be true; a profiling sample must not narrow this set.
///
/// A callback may gather bounded evidence, but must discard it on error. Only
/// `Ok` certifies complete verification. A limit equal to the exact number of
/// eligible cells succeeds; the next cell refuses before it is visited.
pub fn scan_complete_table<I, E, F>(
    selected_rows: I,
    eligible_columns: &[usize],
    max_grounding_cells_scanned: u64,
    mut visit: F,
) -> Result<CompleteScan, CompleteScanError<E>>
where
    I: IntoIterator<Item = Result<LogicalRecord, E>>,
    F: FnMut(usize, usize, Option<&str>),
{
    let eligible_columns = eligible_columns
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut selected_rows_scanned = 0u64;
    let mut total_cells_scanned = 0u64;
    for result in selected_rows {
        let row = result.map_err(CompleteScanError::Source)?;
        for &column in &eligible_columns {
            if total_cells_scanned >= max_grounding_cells_scanned {
                return Err(CompleteScanError::ResourceLimitExceeded {
                    limit: max_grounding_cells_scanned,
                    cells_scanned: total_cells_scanned,
                });
            }
            visit(
                row.index,
                column,
                row.fields.get(column).map(String::as_str),
            );
            total_cells_scanned += 1;
        }
        selected_rows_scanned += 1;
    }
    Ok(CompleteScan {
        selected_rows_scanned,
        total_cells_scanned,
        eligible_columns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_model::ExactDecimal;
    use baho_model::column::NumericParsePolicy;
    use baho_model::document::{ParsedCell, SourcedCell};

    fn row(index: usize, fields: &[&str]) -> Result<LogicalRecord, &'static str> {
        Ok(LogicalRecord {
            index,
            fields: fields.iter().map(|value| (*value).to_owned()).collect(),
            is_blank: false,
        })
    }

    #[test]
    fn streamed_evidence_uses_executor_mixed_cell_eligibility() {
        let column = ColumnDefinition {
            id: "column-0".into(),
            ordinal: 0,
            source_header_raw: Some("Value".into()),
            source_header_normalized: Some("Value".into()),
            display_name: "Value".into(),
        };
        let rows = (1..=12).map(|index| row(index, &[if index <= 10 { "500" } else { "500x" }]));
        let evidence = scan_grounding_evidence(
            rows,
            &[column],
            &["500", "500x"],
            TextMatchPolicy::Exact,
            &NormalizationConfig::default(),
            12,
            1,
        )
        .unwrap();
        let column = &evidence.columns[0];
        assert_eq!(column.inferred_type, InferredColumnType::Mixed);
        assert_eq!(column.matches[0].match_count, 0);
        assert_eq!(column.matches[1].match_count, 2);
        assert_eq!(column.matches[1].sample_cells.len(), 1);
        assert_eq!(column.matches[1].sample_cells[0].row, 11);
    }

    #[test]
    fn verifies_late_matches_in_every_eligible_column() {
        let rows = vec![
            row(1, &["target", "other"]),
            row(2, &["other", "other"]),
            row(1001, &["other", "target"]),
        ];
        let mut matches = Vec::new();
        let complete = scan_complete_table(rows, &[1, 0, 1], 6, |source_row, column, raw| {
            if raw == Some("target") {
                matches.push((source_row, column));
            }
        })
        .unwrap();

        assert_eq!(complete.selected_rows_scanned, 3);
        assert_eq!(complete.total_cells_scanned, 6);
        assert_eq!(complete.eligible_columns, vec![0, 1]);
        assert_eq!(matches, vec![(1, 0), (1001, 1)]);
    }

    #[test]
    fn counts_missing_cells_and_refuses_before_an_unverified_cell() {
        let rows = vec![row(4, &["a"]), row(5, &["b"])];
        let mut visited = Vec::new();
        let error = scan_complete_table(rows, &[0, 1], 3, |source_row, column, raw| {
            visited.push((source_row, column, raw.map(str::to_owned)));
        })
        .unwrap_err();

        assert!(matches!(
            error,
            CompleteScanError::ResourceLimitExceeded {
                limit: 3,
                cells_scanned: 3
            }
        ));
        assert_eq!(
            visited,
            vec![
                (4, 0, Some("a".into())),
                (4, 1, None),
                (5, 0, Some("b".into()))
            ]
        );
    }

    #[test]
    fn source_failure_never_returns_a_complete_certificate() {
        let rows = vec![row(1, &["a"]), Err("read failed")];
        let error = scan_complete_table(rows, &[0], 10, |_, _, _| {}).unwrap_err();
        assert!(matches!(error, CompleteScanError::Source("read failed")));
    }

    #[test]
    fn zero_limit_refuses_nonempty_scan() {
        let error = scan_complete_table(vec![row(1, &["a"])], &[0], 0, |_, _, _| {}).unwrap_err();
        assert!(matches!(
            error,
            CompleteScanError::ResourceLimitExceeded {
                limit: 0,
                cells_scanned: 0
            }
        ));
    }

    #[test]
    fn lookup_finds_late_matches_in_multiple_columns_and_bounds_locations() {
        let mut rows = (1..=20)
            .map(|index| row(index, &["other", "other"]))
            .collect::<Vec<_>>();
        rows[0] = row(1, &["Target", "other"]);
        rows[18] = row(19, &["other", "TARGET"]);
        rows[19] = row(20, &["target", "target"]);
        let columns = [
            LookupColumn {
                column_id: "column-0",
                ordinal: 0,
                parsed: None,
            },
            LookupColumn {
                column_id: "column-1",
                ordinal: 1,
                parsed: None,
            },
        ];
        let result = lookup_exact_value(
            rows,
            &columns,
            "target",
            TextMatchPolicy::UnicodeLowercase,
            &NormalizationConfig::default(),
            40,
            1,
        )
        .unwrap();
        assert_eq!(result.scan.total_cells_scanned, 40);
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.match_count)
                .collect::<Vec<_>>(),
            vec![2, 2]
        );
        assert_eq!(result.columns[0].sample_cells[0].row, 1);
        assert_eq!(result.columns[1].sample_cells[0].row, 19);
        assert_eq!(result.columns[0].sample_cells.len(), 1);
    }

    #[test]
    fn lookup_excludes_blank_missing_and_mixed_numeric_cells() {
        let typed = ParsedColumn {
            column_id: "column-0".into(),
            policy: NumericParsePolicy::StrictDecimal,
            cells: (0..=11)
                .map(|source_row| {
                    let parsed = if source_row >= 10 {
                        ParsedCell::Malformed {
                            raw_text: "500x".into(),
                            reason: ExactDecimal::parse("500x").unwrap_err(),
                        }
                    } else {
                        ParsedCell::Valid(ExactDecimal::parse("500").unwrap())
                    };
                    SourcedCell {
                        address: CellAddress {
                            sheet_index: 0,
                            row: source_row,
                            col: 0,
                        },
                        raw_text: Some(if source_row >= 10 { "500x" } else { "500" }.into()),
                        parsed,
                    }
                })
                .collect(),
        };
        assert_eq!(typed.inferred_type(), InferredColumnType::Mixed);
        let columns = [
            LookupColumn {
                column_id: "column-0",
                ordinal: 0,
                parsed: Some(&typed),
            },
            LookupColumn {
                column_id: "column-1",
                ordinal: 1,
                parsed: None,
            },
        ];
        let mut rows = (0..=11)
            .map(|index| row(index, &[if index >= 10 { "500x" } else { "500" }, " "]))
            .collect::<Vec<_>>();
        rows[11] = row(11, &["500x"]);
        let result = lookup_exact_value(
            rows.clone(),
            &columns,
            "500",
            TextMatchPolicy::Exact,
            &NormalizationConfig::default(),
            24,
            2,
        )
        .unwrap();
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.match_count)
                .collect::<Vec<_>>(),
            vec![0, 0]
        );
        let result = lookup_exact_value(
            rows,
            &columns,
            "500x",
            TextMatchPolicy::Exact,
            &NormalizationConfig::default(),
            24,
            2,
        )
        .unwrap();
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.match_count)
                .collect::<Vec<_>>(),
            vec![2, 0]
        );
    }

    #[test]
    fn lookup_refuses_when_limit_prevents_complete_verification() {
        let columns = [LookupColumn {
            column_id: "column-0",
            ordinal: 0,
            parsed: None,
        }];
        let error = lookup_exact_value(
            vec![row(1, &["target"]), row(2, &["other"])],
            &columns,
            "target",
            TextMatchPolicy::Exact,
            &NormalizationConfig::default(),
            1,
            1,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CompleteScanError::ResourceLimitExceeded {
                limit: 1,
                cells_scanned: 1
            }
        ));
    }

    #[test]
    fn lookup_refuses_incomplete_mixed_column_evidence() {
        let mut cells = Vec::new();
        for index in 0..9 {
            cells.push(SourcedCell {
                address: CellAddress {
                    sheet_index: 0,
                    row: index,
                    col: 0,
                },
                raw_text: Some("500".into()),
                parsed: ParsedCell::Valid(ExactDecimal::parse("500").unwrap()),
            });
        }
        for index in 9..11 {
            cells.push(SourcedCell {
                address: CellAddress {
                    sheet_index: 0,
                    row: index,
                    col: 0,
                },
                raw_text: Some("500x".into()),
                parsed: ParsedCell::Malformed {
                    raw_text: "500x".into(),
                    reason: ExactDecimal::parse("500x").unwrap_err(),
                },
            });
        }
        let typed = ParsedColumn {
            column_id: "column-0".into(),
            policy: NumericParsePolicy::StrictDecimal,
            cells,
        };
        assert_eq!(typed.inferred_type(), InferredColumnType::Mixed);
        let error = lookup_exact_value(
            vec![row(11, &["500x"])],
            &[LookupColumn {
                column_id: "column-0",
                ordinal: 0,
                parsed: Some(&typed),
            }],
            "500x",
            TextMatchPolicy::Exact,
            &NormalizationConfig::default(),
            1,
            1,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CompleteScanError::MissingTypedCell { source_row: 11, .. }
        ));
    }

    #[test]
    fn flag_shape_requires_complete_boolean_nonblank_cells() {
        let rows = vec![
            row(1, &["True", "yes", " "]),
            row(2, &["FALSE", "false"]),
            row(3, &["", "true"]),
        ];
        let result = scan_flag_shapes(
            rows,
            &[2, 1, 0],
            TextMatchPolicy::UnicodeLowercase,
            &NormalizationConfig::default(),
            9,
        )
        .unwrap();
        assert_eq!(result.scan.total_cells_scanned, 9);
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.flag_shaped)
                .collect::<Vec<_>>(),
            vec![true, false, false]
        );
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.nonblank_count)
                .collect::<Vec<_>>(),
            vec![2, 3, 0]
        );
    }

    #[test]
    fn flag_shape_refuses_partial_scan() {
        let result = scan_flag_shapes(
            vec![row(1, &["true"]), row(2, &["other"])],
            &[0],
            TextMatchPolicy::UnicodeLowercase,
            &NormalizationConfig::default(),
            1,
        );
        assert!(matches!(
            result,
            Err(CompleteScanError::ResourceLimitExceeded { .. })
        ));
    }
}
