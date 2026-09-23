//! Strict typed parsing of compared CSV columns.
//!
//! This module owns strict CSV value profiling and parsing evidence for
//! columns referenced by typed comparison predicates: blank, missing,
//! malformed, and exact-decimal outcomes, the mixed-column refusal
//! threshold, and bounded `parse.value_malformed` / `parse.column_mixed`
//! diagnostics. It does not own prompt recognition or Boolean evaluation.

use serde::{Deserialize, Serialize};

use baho_model::column::{ColumnDefinition, NumericParsePolicy, ParsedColumn};
use baho_model::decimal::DecimalParseError;
use baho_model::diagnostic::{Diagnostic, DiagnosticLocation, Severity};
use baho_model::document::{CellAddress, ParsedCell, SourcedCell};

use crate::config::NormalizationConfig;
use crate::inspector::LogicalRecord;

/// Locked decision 6: at most 3 sample cell locations per failure kind.
pub const MAX_MALFORMED_SAMPLE_CELLS: usize = 3;

/// Locked decision 5: a compared column is refused when the malformed share
/// of nonblank cells exceeds this percentage. The canonical definition lives
/// in `baho-model` alongside the shared inferred-column-type rule.
pub const MIXED_COLUMN_MALFORMED_SHARE_PERCENT: u64 =
    baho_model::column::MIXED_COLUMN_MALFORMED_SHARE_PERCENT;

/// Whether an accepted column still carries malformed cells or is refused
/// outright per locked decision 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ParseVerdict {
    /// The column may be compared; malformed cells evaluate to `unknown`.
    Accepted,
    /// The column is refused for typed comparison with `parse.column_mixed`.
    Mixed { reason: MixedRefusalReason },
}

/// Why a compared column was refused under locked decision 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixedRefusalReason {
    /// No nonblank cell parsed successfully, so the column provides no
    /// evidence of being numeric.
    NoParseableValues,
    /// The malformed share of nonblank cells exceeds
    /// [`MIXED_COLUMN_MALFORMED_SHARE_PERCENT`].
    MalformedShareExceeded,
}

/// Outcome counts for one compared column; total rows equals the number of
/// body records examined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnParseCounts {
    pub rows: usize,
    pub missing: usize,
    pub blank: usize,
    pub valid: usize,
    pub malformed: usize,
}

impl ColumnParseCounts {
    /// Cells that carry a value: valid plus malformed outcomes.
    pub fn nonblank(&self) -> usize {
        self.valid + self.malformed
    }
}

/// One bounded evidence record per column per malformed failure kind.
///
/// Sample cells appear in source order, limited to
/// [`MAX_MALFORMED_SAMPLE_CELLS`]; entries appear in order of first
/// occurrence in source order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MalformedValuesEvidence {
    pub column_id: String,
    pub reason: DecimalParseError,
    pub total_count: usize,
    pub sample_cells: Vec<CellAddress>,
}

/// Strict typed parsing result for one compared column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparedColumnParse {
    /// Parsed cells with raw text and source coordinates.
    pub column: ParsedColumn,
    /// Zero-based ordinal of the compared column within the table.
    pub column_ordinal: usize,
    pub verdict: ParseVerdict,
    pub malformed_evidence: Vec<MalformedValuesEvidence>,
}

impl ComparedColumnParse {
    /// Outcome counts derived from the parsed cells.
    pub fn counts(&self) -> ColumnParseCounts {
        parse_counts(self.column.cells.len(), &self.column.cells)
    }

    /// Deterministic diagnostics: `parse.column_mixed` when refused, then
    /// one `parse.value_malformed` per failure kind. Malformed sample raw
    /// text stays in [`Self::column`]; the diagnostics carry counts and
    /// locations only.
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();
        if let ParseVerdict::Mixed { reason } = self.verdict {
            let counts = self.counts();
            let scope = format!("column '{}'", self.column.column_id);
            let message = match reason {
                MixedRefusalReason::NoParseableValues if counts.nonblank() == 0 => {
                    format!(
                        "{scope}: no nonblank values, so the column cannot be verified as numeric"
                    )
                }
                MixedRefusalReason::NoParseableValues => {
                    format!(
                        "{scope}: 0 of {} nonblank values parse as strict decimals",
                        counts.nonblank()
                    )
                }
                MixedRefusalReason::MalformedShareExceeded => {
                    format!(
                        "{scope}: {} of {} nonblank values are malformed, exceeding the {}% limit",
                        counts.malformed,
                        counts.nonblank(),
                        MIXED_COLUMN_MALFORMED_SHARE_PERCENT
                    )
                }
            };
            diagnostics.push(Diagnostic {
                code: "parse.column_mixed".to_string(),
                severity: Severity::Error,
                stage: PARSE_DIAGNOSTIC_STAGE.to_string(),
                message,
                location: Some(DiagnosticLocation {
                    row: None,
                    col: Some(self.column_ordinal),
                    cell: None,
                    cells: Vec::new(),
                }),
            });
        }
        for evidence in &self.malformed_evidence {
            diagnostics.push(Diagnostic {
                code: "parse.value_malformed".to_string(),
                severity: Severity::Warning,
                stage: PARSE_DIAGNOSTIC_STAGE.to_string(),
                message: format!(
                    "column '{}': {} malformed value(s) for strict decimal parsing (reason: {})",
                    evidence.column_id, evidence.total_count, evidence.reason
                ),
                location: Some(DiagnosticLocation {
                    row: None,
                    col: None,
                    cell: evidence.sample_cells.first().cloned(),
                    cells: evidence.sample_cells.clone(),
                }),
            });
        }
        diagnostics
    }
}

const PARSE_DIAGNOSTIC_STAGE: &str = "ingest-csv";

/// Parse one compared column over the selected region's body records.
///
/// Every record yields exactly one [`SourcedCell`]: `Missing` when the
/// physical record ends before this column, `Blank` when the present text is
/// blank under `blank_rule`, and otherwise strict decimal parse outcome under
/// `policy`. The mixed-column verdict follows locked decision 5.
pub fn parse_compared_column(
    records: &[LogicalRecord],
    sheet_index: usize,
    column: &ColumnDefinition,
    policy: NumericParsePolicy,
    blank_rule: &NormalizationConfig,
) -> ComparedColumnParse {
    let mut cells = Vec::with_capacity(records.len());
    for record in records {
        let address = CellAddress {
            sheet_index,
            row: record.index,
            col: column.ordinal,
        };
        let sourced = match record.fields.get(column.ordinal) {
            None => SourcedCell {
                address,
                raw_text: None,
                parsed: ParsedCell::Missing,
            },
            Some(text) if blank_rule.is_blank(text) => SourcedCell {
                address,
                raw_text: Some(text.clone()),
                parsed: ParsedCell::Blank,
            },
            Some(text) => SourcedCell {
                address,
                raw_text: Some(text.clone()),
                parsed: match policy.parse_decimal(text) {
                    Ok(value) => ParsedCell::Valid(value),
                    Err(reason) => ParsedCell::Malformed {
                        raw_text: text.clone(),
                        reason,
                    },
                },
            },
        };
        cells.push(sourced);
    }

    let counts = parse_counts(records.len(), &cells);
    let verdict = match mixed_refusal_reason(counts.valid, counts.malformed) {
        Some(reason) => ParseVerdict::Mixed { reason },
        None => ParseVerdict::Accepted,
    };
    let malformed_evidence = collect_malformed_evidence(&column.id, &cells);

    ComparedColumnParse {
        column: ParsedColumn {
            column_id: column.id.clone(),
            policy,
            cells,
        },
        column_ordinal: column.ordinal,
        verdict,
        malformed_evidence,
    }
}

/// Why a compared column is refused with `parse.column_mixed` under locked
/// decision 5.
fn mixed_refusal_reason(valid: usize, malformed: usize) -> Option<MixedRefusalReason> {
    if valid == 0 {
        Some(MixedRefusalReason::NoParseableValues)
    } else if malformed_share_exceeds_limit(valid, malformed) {
        Some(MixedRefusalReason::MalformedShareExceeded)
    } else {
        None
    }
}

/// Exact rational comparison `malformed / nonblank > PERCENT / 100`, so the
/// boundary case (shares that equal the limit exactly) stays accepted; `u128`
/// arithmetic cannot overflow for any countable number of cells.
fn malformed_share_exceeds_limit(valid: usize, malformed: usize) -> bool {
    let valid = valid as u128;
    let malformed = malformed as u128;
    malformed * 100 > (valid + malformed) * u128::from(MIXED_COLUMN_MALFORMED_SHARE_PERCENT)
}

fn collect_malformed_evidence(
    column_id: &str,
    cells: &[SourcedCell],
) -> Vec<MalformedValuesEvidence> {
    let mut kinds: Vec<(DecimalParseError, Vec<CellAddress>)> = Vec::new();
    for cell in cells {
        if let ParsedCell::Malformed { reason, .. } = cell.parsed {
            match kinds.iter_mut().find(|(kind, _)| kind == &reason) {
                Some((_, addresses)) => addresses.push(cell.address.clone()),
                None => kinds.push((reason, vec![cell.address.clone()])),
            }
        }
    }
    kinds
        .into_iter()
        .map(|(reason, addresses)| {
            let total_count = addresses.len();
            MalformedValuesEvidence {
                column_id: column_id.to_string(),
                reason,
                total_count,
                sample_cells: addresses
                    .into_iter()
                    .take(MAX_MALFORMED_SAMPLE_CELLS)
                    .collect(),
            }
        })
        .collect()
}

fn parse_counts(rows: usize, cells: &[SourcedCell]) -> ColumnParseCounts {
    let mut counts = ColumnParseCounts {
        rows,
        missing: 0,
        blank: 0,
        valid: 0,
        malformed: 0,
    };
    for cell in cells {
        match cell.parsed {
            ParsedCell::Missing => counts.missing += 1,
            ParsedCell::Blank => counts.blank += 1,
            ParsedCell::Valid(_) => counts.valid += 1,
            ParsedCell::Malformed { .. } => counts.malformed += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use baho_model::ExactDecimal;

    fn record(index: usize, fields: &[&str]) -> LogicalRecord {
        LogicalRecord {
            index,
            fields: fields.iter().map(|field| field.to_string()).collect(),
            is_blank: false,
        }
    }

    fn income_column(ordinal: usize) -> ColumnDefinition {
        ColumnDefinition {
            id: format!("column-{ordinal}"),
            ordinal,
            source_header_raw: Some("Annual Income".to_string()),
            source_header_normalized: Some("annual income".to_string()),
            display_name: "Annual Income".to_string(),
        }
    }

    fn parse_income(records: &[LogicalRecord]) -> ComparedColumnParse {
        parse_compared_column(
            records,
            0,
            &income_column(1),
            NumericParsePolicy::StrictDecimal,
            &NormalizationConfig::default(),
        )
    }

    #[test]
    fn parse_distinguishes_blank_missing_valid_and_malformed_cells() {
        let mut records = Vec::new();
        for (offset, value) in [
            "1000", "1001", "1002", "1003", "1004", "1005", "1006", "1007", "1008", "1009",
        ]
        .iter()
        .enumerate()
        {
            records.push(record(3 + offset, &[&format!("id-{offset}"), value]));
        }
        records.push(record(13, &["id-blank", "   "]));
        records.push(record(14, &["id-short"]));
        records.push(record(15, &["id-bad", "10,000"]));

        let parsed = parse_income(&records);

        assert_eq!(parsed.verdict, ParseVerdict::Accepted);
        assert_eq!(parsed.column.column_id, "column-1");
        assert_eq!(parsed.column.policy, NumericParsePolicy::StrictDecimal);
        assert_eq!(
            parsed.counts(),
            ColumnParseCounts {
                rows: 13,
                missing: 1,
                blank: 1,
                valid: 10,
                malformed: 1
            }
        );

        let first = &parsed.column.cells[0];
        assert_eq!(
            first.address,
            CellAddress {
                sheet_index: 0,
                row: 3,
                col: 1
            }
        );
        assert_eq!(first.raw_text.as_deref(), Some("1000"));
        assert_eq!(
            first.parsed,
            ParsedCell::Valid(ExactDecimal::parse("1000").unwrap())
        );

        let blank = &parsed.column.cells[10];
        assert_eq!(blank.address.row, 13);
        assert_eq!(blank.raw_text.as_deref(), Some("   "));
        assert_eq!(blank.parsed, ParsedCell::Blank);

        let missing = &parsed.column.cells[11];
        assert_eq!(missing.address.row, 14);
        assert_eq!(missing.raw_text, None);
        assert_eq!(missing.parsed, ParsedCell::Missing);

        let malformed = &parsed.column.cells[12];
        assert_eq!(malformed.address.row, 15);
        assert_eq!(malformed.raw_text.as_deref(), Some("10,000"));
        assert_eq!(
            malformed.parsed,
            ParsedCell::Malformed {
                raw_text: "10,000".to_string(),
                reason: DecimalParseError::InvalidCharacter
            }
        );

        assert!(
            parsed
                .column
                .cells
                .iter()
                .all(|cell| cell.address.sheet_index == 0 && cell.address.col == 1)
        );
        assert_eq!(parsed.malformed_evidence.len(), 1);
        assert_eq!(parsed.malformed_evidence[0].total_count, 1);
        assert_eq!(
            parsed.malformed_evidence[0].sample_cells,
            vec![CellAddress {
                sheet_index: 0,
                row: 15,
                col: 1
            }]
        );

        let diagnostics = parsed.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "parse.value_malformed");
        assert_eq!(diagnostics[0].severity, Severity::Warning);
        assert_eq!(diagnostics[0].stage, "ingest-csv");
        assert_eq!(
            diagnostics[0].location,
            Some(DiagnosticLocation {
                row: None,
                col: None,
                cell: Some(CellAddress {
                    sheet_index: 0,
                    row: 15,
                    col: 1
                }),
                cells: vec![CellAddress {
                    sheet_index: 0,
                    row: 15,
                    col: 1
                }]
            })
        );
    }

    #[test]
    fn valid_negative_and_decimal_values_parse_exactly() {
        let records: Vec<LogicalRecord> = ["-2.5", "+7", "1.10", "0.001", "-0.25"]
            .iter()
            .enumerate()
            .map(|(offset, value)| record(offset, &[value, "ignored"]))
            .collect();

        let parsed = parse_compared_column(
            &records,
            0,
            &income_column(0),
            NumericParsePolicy::StrictDecimal,
            &NormalizationConfig::default(),
        );

        assert_eq!(parsed.verdict, ParseVerdict::Accepted);
        let values: Vec<ExactDecimal> = parsed
            .column
            .cells
            .iter()
            .map(|cell| match &cell.parsed {
                ParsedCell::Valid(value) => *value,
                other => panic!("expected valid parse, got {other:?}"),
            })
            .collect();
        assert_eq!(values[0], ExactDecimal::parse("-2.5").unwrap());
        assert_eq!(values[0].to_canonical_string(), "-2.5");
        assert_eq!(values[1], ExactDecimal::parse("7").unwrap());
        assert_eq!(values[2], ExactDecimal::parse("1.1").unwrap());
        assert_eq!(values[3].to_canonical_string(), "0.001");
        assert_eq!(values[4], ExactDecimal::parse("-0.25").unwrap());
        assert_eq!(parsed.counts().malformed, 0);
        assert!(parsed.malformed_evidence.is_empty());
    }

    #[test]
    fn zero_parseable_nonblank_values_refuse_as_mixed() {
        let records = vec![
            record(0, &["a", "abc"]),
            record(1, &["b", "10,000"]),
            record(2, &["c", "x"]),
        ];

        let parsed = parse_income(&records);

        assert_eq!(
            parsed.verdict,
            ParseVerdict::Mixed {
                reason: MixedRefusalReason::NoParseableValues
            }
        );
        let diagnostics = parsed.diagnostics();
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].code, "parse.column_mixed");
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert_eq!(
            diagnostics[0].location,
            Some(DiagnosticLocation {
                row: None,
                col: Some(1),
                cell: None,
                cells: Vec::new()
            })
        );
        assert_eq!(diagnostics[1].code, "parse.value_malformed");
    }

    #[test]
    fn all_blank_column_refuses_as_mixed() {
        let records = vec![
            record(0, &["a", ""]),
            record(1, &["b", "   "]),
            record(2, &["c"]),
        ];

        let parsed = parse_income(&records);

        assert_eq!(
            parsed.counts(),
            ColumnParseCounts {
                rows: 3,
                missing: 1,
                blank: 2,
                valid: 0,
                malformed: 0
            }
        );
        assert_eq!(
            parsed.verdict,
            ParseVerdict::Mixed {
                reason: MixedRefusalReason::NoParseableValues
            }
        );
        assert!(parsed.malformed_evidence.is_empty());
        let diagnostics = parsed.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "parse.column_mixed");
    }

    #[test]
    fn column_absent_from_all_rows_refuses_as_mixed() {
        let records = vec![record(0, &["a"]), record(1, &["b"]), record(2, &["c"])];

        let parsed = parse_income(&records);

        assert_eq!(
            parsed.counts(),
            ColumnParseCounts {
                rows: 3,
                missing: 3,
                blank: 0,
                valid: 0,
                malformed: 0
            }
        );
        assert_eq!(
            parsed.verdict,
            ParseVerdict::Mixed {
                reason: MixedRefusalReason::NoParseableValues
            }
        );
        assert!(
            parsed
                .column
                .cells
                .iter()
                .all(|cell| cell.parsed == ParsedCell::Missing && cell.raw_text.is_none())
        );
    }

    #[test]
    fn malformed_share_above_threshold_refuses_as_mixed() {
        let mut records = Vec::new();
        for offset in 0..8 {
            records.push(record(offset, &[&format!("id-{offset}"), "500"]));
        }
        records.push(record(8, &["id-bad", "10,000"]));

        let parsed = parse_income(&records);

        assert_eq!(
            parsed.counts(),
            ColumnParseCounts {
                rows: 9,
                missing: 0,
                blank: 0,
                valid: 8,
                malformed: 1
            }
        );
        assert_eq!(
            parsed.verdict,
            ParseVerdict::Mixed {
                reason: MixedRefusalReason::MalformedShareExceeded
            }
        );
        let diagnostics = parsed.diagnostics();
        assert_eq!(diagnostics[0].code, "parse.column_mixed");
        assert!(diagnostics[0].message.contains("exceeding the 10% limit"));
    }

    #[test]
    fn malformed_share_exactly_at_threshold_is_accepted() {
        let mut records = Vec::new();
        for offset in 0..9 {
            records.push(record(offset, &[&format!("id-{offset}"), "500"]));
        }
        records.push(record(9, &["id-bad", "10,000"]));
        let parsed = parse_income(&records);
        assert_eq!(
            parsed.counts(),
            ColumnParseCounts {
                rows: 10,
                missing: 0,
                blank: 0,
                valid: 9,
                malformed: 1
            }
        );
        assert_eq!(parsed.verdict, ParseVerdict::Accepted);

        let mut records = Vec::new();
        for offset in 0..180 {
            records.push(record(offset, &[&format!("id-{offset}"), "500"]));
        }
        for offset in 180..200 {
            records.push(record(offset, &[&format!("id-{offset}"), "10,000"]));
        }
        let parsed = parse_income(&records);
        assert_eq!(
            parsed.counts(),
            ColumnParseCounts {
                rows: 200,
                missing: 0,
                blank: 0,
                valid: 180,
                malformed: 20
            }
        );
        assert_eq!(parsed.verdict, ParseVerdict::Accepted);
    }

    #[test]
    fn malformed_samples_are_bounded_with_total_count() {
        let mut records = Vec::new();
        for offset in 0..45 {
            records.push(record(offset, &[&format!("id-{offset}"), "500"]));
        }
        for offset in 45..50 {
            records.push(record(offset, &[&format!("id-{offset}"), "10,000"]));
        }

        let parsed = parse_income(&records);

        assert_eq!(
            parsed.counts(),
            ColumnParseCounts {
                rows: 50,
                missing: 0,
                blank: 0,
                valid: 45,
                malformed: 5
            }
        );
        assert_eq!(parsed.verdict, ParseVerdict::Accepted);
        assert_eq!(parsed.malformed_evidence.len(), 1);
        let evidence = &parsed.malformed_evidence[0];
        assert_eq!(evidence.total_count, 5);
        assert_eq!(evidence.sample_cells.len(), MAX_MALFORMED_SAMPLE_CELLS);
        assert_eq!(
            evidence.sample_cells,
            vec![
                CellAddress {
                    sheet_index: 0,
                    row: 45,
                    col: 1
                },
                CellAddress {
                    sheet_index: 0,
                    row: 46,
                    col: 1
                },
                CellAddress {
                    sheet_index: 0,
                    row: 47,
                    col: 1
                },
            ]
        );
        assert_eq!(parsed.diagnostics().len(), 1);

        let diagnostics = parsed.diagnostics();
        let location = diagnostics[0]
            .location
            .as_ref()
            .expect("malformed diagnostic carries a location");
        assert_eq!(
            location.cells, evidence.sample_cells,
            "structured sample cells must match the bounded evidence in order"
        );
        assert!(
            location.cells.len() <= MAX_MALFORMED_SAMPLE_CELLS,
            "sample cell locations stay bounded"
        );
        assert_eq!(
            location.cell,
            evidence.sample_cells.first().cloned(),
            "cell remains the first sample for backward compatibility"
        );
    }

    #[test]
    fn one_diagnostic_per_failure_kind_in_first_occurrence_order() {
        let kinds = ["10,000", ".5", "5.", "1.2.3"];
        let mut records = Vec::new();
        for offset in 0..36 {
            records.push(record(offset, &[&format!("id-{offset}"), "500"]));
        }
        for (offset, kind) in kinds.iter().enumerate() {
            records.push(record(36 + offset, &[&format!("id-{offset}"), kind]));
        }

        let parsed = parse_income(&records);

        assert_eq!(
            parsed.counts(),
            ColumnParseCounts {
                rows: 40,
                missing: 0,
                blank: 0,
                valid: 36,
                malformed: 4
            }
        );
        assert_eq!(parsed.verdict, ParseVerdict::Accepted);
        let reasons: Vec<DecimalParseError> = parsed
            .malformed_evidence
            .iter()
            .map(|evidence| evidence.reason)
            .collect();
        assert_eq!(
            reasons,
            vec![
                DecimalParseError::InvalidCharacter,
                DecimalParseError::MissingIntegerDigits,
                DecimalParseError::MissingFractionDigits,
                DecimalParseError::MultipleDecimalPoints,
            ]
        );
        for evidence in &parsed.malformed_evidence {
            assert_eq!(evidence.total_count, 1);
            assert_eq!(evidence.sample_cells.len(), 1);
        }
        let diagnostics = parsed.diagnostics();
        assert_eq!(diagnostics.len(), 4);
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code == "parse.value_malformed")
        );
    }

    #[test]
    fn parse_evidence_is_deterministic_across_runs() {
        let records = vec![
            record(0, &["a", "100"]),
            record(1, &["b", "oops"]),
            record(2, &["c", "5."]),
            record(3, &["d", "200"]),
        ];
        let first = parse_income(&records);
        let second = parse_income(&records);
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_value(&first.malformed_evidence).unwrap(),
            serde_json::to_value(&second.malformed_evidence).unwrap()
        );
    }

    #[test]
    fn compared_column_parse_serde_round_trip() {
        let mut records = Vec::new();
        for offset in 0..9 {
            records.push(record(offset, &[&format!("id-{offset}"), "100"]));
        }
        records.push(record(9, &["id-blank", ""]));
        records.push(record(10, &["id-short"]));
        records.push(record(11, &["id-bad", "oops"]));
        let parsed = parse_income(&records);
        assert_eq!(parsed.verdict, ParseVerdict::Accepted);

        let json = serde_json::to_value(&parsed).unwrap();
        assert_eq!(json["column"]["column_id"], "column-1");
        assert_eq!(json["column"]["policy"], "strict_decimal");
        assert_eq!(json["column_ordinal"], 1);
        assert_eq!(json["verdict"]["status"], "accepted");
        assert_eq!(json["malformed_evidence"][0]["reason"], "invalid_character");

        let back: ComparedColumnParse = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, back);
    }
}
