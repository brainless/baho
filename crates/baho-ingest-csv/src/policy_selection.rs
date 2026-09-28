//! Column-level numeric parse policy selection.
//!
//! Epic 008 locked decisions 5–6: per-value separator shapes are merged into
//! role assignments for the decimal and grouping marks. Conflicts refuse with
//! `parse.format_ambiguous`; a single consistent assignment selects a
//! [`NumericParsePolicy`]; undecided-only evidence falls back to the locked
//! grouping preference when exactly one separator character occurs.

use serde::{Deserialize, Serialize};

use baho_model::column::NumericParsePolicy;
use baho_model::numeric_shape::{NumberShape, SeparatorRole, classify_number_shape};

/// Why column policy selection refused (locked decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAmbiguityReason {
    /// Per-value role assignments conflict: one separator as both marks, both
    /// separators as decimal marks, or both separators as grouping marks.
    ConflictingRoles,
    /// No decided evidence and both separator characters occur, so grouping
    /// and decimal readings both remain plausible.
    BothSeparatorsUndecided,
}

/// Bounded evidence for one policy selection, recorded in run artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySelectionEvidence {
    /// Values with no separator characters.
    pub neutral: usize,
    /// Values that decided at least one role.
    pub decided: usize,
    /// Values whose separator roles stayed undecided.
    pub undecided: usize,
    /// Merged role assignments in deterministic first-occurrence order.
    pub role_assignments: Vec<(char, SeparatorRole)>,
    /// Separator chosen by the locked grouping preference, if any.
    pub locked_preference: Option<char>,
}

/// Successful policy selection with its supporting evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySelection {
    pub policy: NumericParsePolicy,
    pub evidence: PolicySelectionEvidence,
}

/// Refused policy selection with the evidence that made it ambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySelectionRefused {
    pub reason: PolicyAmbiguityReason,
    pub evidence: PolicySelectionEvidence,
}

/// Outcome of selecting a policy for one compared column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PolicySelectionOutcome {
    Selected(PolicySelection),
    Ambiguous(PolicySelectionRefused),
}

/// Constant-size numeric format evidence for a streamed column.
#[derive(Debug, Default, Clone)]
pub struct PolicyEvidenceAccumulator {
    neutral: usize,
    decided: usize,
    undecided: usize,
    role_assignments: Vec<(char, SeparatorRole)>,
    conflicting_roles: bool,
    comma_seen: bool,
    dot_seen: bool,
}

impl PolicyEvidenceAccumulator {
    pub fn observe(&mut self, text: &str) {
        match classify_number_shape(text) {
            NumberShape::Neutral => self.neutral += 1,
            NumberShape::Undecided { separator } => {
                self.undecided += 1;
                mark_seen(separator, &mut self.comma_seen, &mut self.dot_seen);
            }
            NumberShape::Decided { assignments } => {
                self.decided += 1;
                for (separator, role) in assignments {
                    mark_seen(separator, &mut self.comma_seen, &mut self.dot_seen);
                    match self
                        .role_assignments
                        .iter()
                        .find(|(existing, _)| *existing == separator)
                    {
                        Some((_, existing)) if *existing != role => self.conflicting_roles = true,
                        Some(_) => {}
                        None => self.role_assignments.push((separator, role)),
                    }
                }
            }
        }
    }

    pub fn finish(self) -> Result<PolicySelection, PolicySelectionRefused> {
        let Self {
            neutral,
            decided,
            undecided,
            role_assignments,
            conflicting_roles,
            comma_seen,
            dot_seen,
        } = self;
        select_from_evidence(
            neutral,
            decided,
            undecided,
            role_assignments,
            conflicting_roles,
            comma_seen,
            dot_seen,
        )
    }
}

/// Select the numeric parse policy for one compared column's nonblank texts.
///
/// Blank and missing cells must be excluded by the caller. Classification is
/// pure per-value shape evidence; the merged decision follows locked
/// decision 6.
pub fn select_numeric_policy<'a, I>(texts: I) -> Result<PolicySelection, PolicySelectionRefused>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut accumulator = PolicyEvidenceAccumulator::default();
    for text in texts {
        accumulator.observe(text);
    }
    accumulator.finish()
}

fn select_from_evidence(
    neutral: usize,
    decided: usize,
    undecided: usize,
    role_assignments: Vec<(char, SeparatorRole)>,
    conflicting_roles: bool,
    comma_seen: bool,
    dot_seen: bool,
) -> Result<PolicySelection, PolicySelectionRefused> {
    let mut locked_preference: Option<char> = None;

    let evidence = |role_assignments: Vec<(char, SeparatorRole)>,
                    locked_preference: Option<char>| {
        PolicySelectionEvidence {
            neutral,
            decided,
            undecided,
            role_assignments,
            locked_preference,
        }
    };

    if conflicting_roles {
        return Err(PolicySelectionRefused {
            reason: PolicyAmbiguityReason::ConflictingRoles,
            evidence: evidence(Vec::new(), None),
        });
    }
    // Two distinct grouping marks cannot describe one convention.
    let grouping_marks = role_assignments
        .iter()
        .filter(|(_, role)| *role == SeparatorRole::GroupingMark)
        .count();
    let decimal_marks = role_assignments
        .iter()
        .filter(|(_, role)| *role == SeparatorRole::DecimalMark)
        .count();
    if grouping_marks > 1 || decimal_marks > 1 {
        return Err(PolicySelectionRefused {
            reason: PolicyAmbiguityReason::ConflictingRoles,
            evidence: evidence(role_assignments, None),
        });
    }

    let decimal_mark = role_assignments
        .iter()
        .find(|(_, role)| *role == SeparatorRole::DecimalMark)
        .map(|(separator, _)| *separator);
    let grouping_mark = role_assignments
        .iter()
        .find(|(_, role)| *role == SeparatorRole::GroupingMark)
        .map(|(separator, _)| *separator);

    let policy = match (decimal_mark, grouping_mark, comma_seen, dot_seen) {
        // Decided comma decimal: EU convention, dot grouping when present.
        (Some(','), _, _, _) => Some(NumericParsePolicy::CommaDecimalDotGrouping),
        // Decided dot decimal: US convention when a comma occurs, strict when
        // no comma is evidenced.
        (Some('.'), _, true, _) => Some(NumericParsePolicy::DotDecimalCommaGrouping),
        (Some('.'), _, false, _) => Some(NumericParsePolicy::StrictDecimal),
        // Grouping-only evidence: the other mark is the decimal mark.
        (None, Some(','), _, _) => Some(NumericParsePolicy::DotDecimalCommaGrouping),
        (None, Some('.'), _, _) => Some(NumericParsePolicy::CommaDecimalDotGrouping),
        // No decided evidence: locked grouping preference for a single
        // separator character, strict when no separator occurs, and refusal
        // when both separator characters occur undecided.
        (None, None, true, false) => {
            locked_preference = Some(',');
            Some(NumericParsePolicy::DotDecimalCommaGrouping)
        }
        (None, None, false, true) => {
            locked_preference = Some('.');
            Some(NumericParsePolicy::CommaDecimalDotGrouping)
        }
        (None, None, true, true) => {
            return Err(PolicySelectionRefused {
                reason: PolicyAmbiguityReason::BothSeparatorsUndecided,
                evidence: evidence(role_assignments, None),
            });
        }
        (None, None, false, false) => Some(NumericParsePolicy::StrictDecimal),
        // One grouping mark plus one decimal mark is already covered above;
        // unreachable combinations refuse rather than guess.
        _ => {
            return Err(PolicySelectionRefused {
                reason: PolicyAmbiguityReason::ConflictingRoles,
                evidence: evidence(role_assignments, None),
            });
        }
    };

    match policy {
        Some(policy) => Ok(PolicySelection {
            policy,
            evidence: evidence(role_assignments, locked_preference),
        }),
        None => Err(PolicySelectionRefused {
            reason: PolicyAmbiguityReason::ConflictingRoles,
            evidence: evidence(role_assignments, None),
        }),
    }
}

fn mark_seen(separator: char, comma_seen: &mut bool, dot_seen: &mut bool) {
    match separator {
        ',' => *comma_seen = true,
        '.' => *dot_seen = true,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(texts: &[&str]) -> NumericParsePolicy {
        select_numeric_policy(texts.iter().copied())
            .unwrap_or_else(|refused| panic!("expected selection for {texts:?}, got {refused:?}"))
            .policy
    }

    fn refuse(texts: &[&str]) -> PolicyAmbiguityReason {
        select_numeric_policy(texts.iter().copied())
            .err()
            .unwrap_or_else(|| panic!("expected refusal for {texts:?}"))
            .reason
    }

    #[test]
    fn policy_selection_examples_table() {
        assert_eq!(
            select(&["500", "1000", "2.5"]),
            NumericParsePolicy::StrictDecimal
        );
        assert_eq!(
            select(&["50000", "10,000", "75000"]),
            NumericParsePolicy::DotDecimalCommaGrouping
        );
        assert_eq!(
            select(&["1,23", "1,234"]),
            NumericParsePolicy::CommaDecimalDotGrouping
        );
        assert_eq!(
            select(&["1,234.56", "12.34"]),
            NumericParsePolicy::DotDecimalCommaGrouping
        );
        assert_eq!(
            select(&["1.234,56", "12,34"]),
            NumericParsePolicy::CommaDecimalDotGrouping
        );
        assert_eq!(
            select(&["1,234", "1.56"]),
            NumericParsePolicy::DotDecimalCommaGrouping
        );
        assert_eq!(
            select(&["1.234", "1.56"]),
            NumericParsePolicy::StrictDecimal
        );
        assert_eq!(
            refuse(&["1,234", "1.234"]),
            PolicyAmbiguityReason::BothSeparatorsUndecided
        );
        assert_eq!(
            refuse(&["1,23", "1,234,567"]),
            PolicyAmbiguityReason::ConflictingRoles
        );
        assert_eq!(
            refuse(&["1,23", "1.56"]),
            PolicyAmbiguityReason::ConflictingRoles
        );
        assert_eq!(
            select(&["1.234", "2.567"]),
            NumericParsePolicy::CommaDecimalDotGrouping
        );
    }

    #[test]
    fn neutral_only_columns_select_strict_decimal() {
        assert_eq!(select(&[]), NumericParsePolicy::StrictDecimal);
        assert_eq!(
            select(&["500", "abc", "12"]),
            NumericParsePolicy::StrictDecimal
        );
    }

    #[test]
    fn locked_grouping_preference_is_recorded() {
        let selection =
            select_numeric_policy(["50000", "10,000", "75000"].iter().copied()).unwrap();
        assert_eq!(
            selection.policy,
            NumericParsePolicy::DotDecimalCommaGrouping
        );
        assert_eq!(selection.evidence.locked_preference, Some(','));
        assert_eq!(selection.evidence.neutral, 2);
        assert_eq!(selection.evidence.undecided, 1);
        assert!(selection.evidence.role_assignments.is_empty());

        let selection = select_numeric_policy(["1.234", "2.567"].iter().copied()).unwrap();
        assert_eq!(
            selection.policy,
            NumericParsePolicy::CommaDecimalDotGrouping
        );
        assert_eq!(selection.evidence.locked_preference, Some('.'));
    }

    #[test]
    fn role_assignments_merge_in_first_occurrence_order() {
        let selection = select_numeric_policy(["1,234.56", "9.99"].iter().copied()).unwrap();
        assert_eq!(
            selection.policy,
            NumericParsePolicy::DotDecimalCommaGrouping
        );
        assert_eq!(
            selection.evidence.role_assignments,
            vec![
                (',', SeparatorRole::GroupingMark),
                ('.', SeparatorRole::DecimalMark),
            ]
        );
    }

    #[test]
    fn both_grouping_marks_refuse_as_conflicting() {
        assert_eq!(
            refuse(&["1,234,567", "1.234.567"]),
            PolicyAmbiguityReason::ConflictingRoles
        );
    }

    #[test]
    fn selection_outcome_serializes_deterministically() {
        let outcome = PolicySelectionOutcome::Selected(
            select_numeric_policy(["10,000"].iter().copied()).unwrap(),
        );
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["status"], "selected");
        assert_eq!(json["policy"], "dot_decimal_comma_grouping");
        assert_eq!(json["evidence"]["locked_preference"], ",");
    }
}
