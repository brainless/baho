use std::fmt;

use thiserror::Error;

use baho_model::column::MIXED_COLUMN_MALFORMED_SHARE_PERCENT;
use baho_plan::validation::PlanValidationError;

/// Why a compared column is refused under Epic 006 locked decision 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnMixedReason {
    /// No nonblank value parsed as a strict decimal, so the column provides no
    /// evidence of being numeric.
    NoParseableValues,
    /// The malformed share of nonblank values exceeds
    /// [`MIXED_COLUMN_MALFORMED_SHARE_PERCENT`].
    MalformedShareExceeded,
}

impl fmt::Display for ColumnMixedReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoParseableValues => {
                formatter.write_str("no nonblank value parses as a strict decimal")
            }
            Self::MalformedShareExceeded => write!(
                formatter,
                "the malformed share of nonblank values exceeds the {MIXED_COLUMN_MALFORMED_SHARE_PERCENT}% limit"
            ),
        }
    }
}

/// Errors returned during plan execution.
#[derive(Debug, Clone, Error)]
pub enum ExecutionError {
    #[error("table not found: {table_id}")]
    TableNotFound { table_id: String },

    #[error("column not found: {column_id}")]
    ColumnNotFound { column_id: String },

    #[error("type mismatch for column '{column_id}': expected {expected}, got {actual}")]
    TypeMismatch {
        column_id: String,
        expected: String,
        actual: String,
    },

    #[error("limit exceeded for {limit}: {detail}")]
    LimitExceeded { limit: String, detail: String },

    #[error("empty input: {detail}")]
    EmptyInput { detail: String },

    #[error("source revision mismatch: plan expects '{expected}', grid has '{actual}'")]
    SourceRevisionMismatch { expected: String, actual: String },

    #[error(
        "no typed parse data was provided for column '{column_id}', which is required by a decimal comparison"
    )]
    MissingTypedParse { column_id: String },

    #[error("typed parse data for column '{column_id}' does not cover source row {source_row}")]
    TypedParseMismatch {
        column_id: String,
        source_row: usize,
    },

    #[error("column '{column_id}' cannot back a decimal comparison: {reason}")]
    ColumnMixed {
        column_id: String,
        reason: ColumnMixedReason,
    },

    #[error("unresolved deferred literal: {detail}")]
    UnresolvedLiteral { detail: String },

    #[error("invalid plan: {0}")]
    InvalidPlan(#[from] PlanValidationError),
}

impl ExecutionError {
    /// Stable diagnostic code for this failure. A column/literal type
    /// mismatch is the Epic 006 `plan.type_mismatch` and a decision-5
    /// mixed-column refusal is `parse.column_mixed`; every other execution
    /// failure stays `execution.failed`.
    pub fn diagnostic_code(&self) -> &'static str {
        match self {
            Self::TypeMismatch { .. } => "plan.type_mismatch",
            Self::ColumnMixed { .. } => "parse.column_mixed",
            _ => "execution.failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_table_not_found() {
        let err = ExecutionError::TableNotFound {
            table_id: "table-99".to_string(),
        };
        assert_eq!(err.to_string(), "table not found: table-99");
    }

    #[test]
    fn display_column_not_found() {
        let err = ExecutionError::ColumnNotFound {
            column_id: "column-5".to_string(),
        };
        assert_eq!(err.to_string(), "column not found: column-5");
    }

    #[test]
    fn display_type_mismatch() {
        let err = ExecutionError::TypeMismatch {
            column_id: "column-0".to_string(),
            expected: "text".to_string(),
            actual: "number".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "type mismatch for column 'column-0': expected text, got number"
        );
    }

    #[test]
    fn display_limit_exceeded() {
        let err = ExecutionError::LimitExceeded {
            limit: "max_rows".to_string(),
            detail: "10000 rows exceeds limit of 5000".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "limit exceeded for max_rows: 10000 rows exceeds limit of 5000"
        );
    }

    #[test]
    fn display_empty_input() {
        let err = ExecutionError::EmptyInput {
            detail: "no data rows in table".to_string(),
        };
        assert_eq!(err.to_string(), "empty input: no data rows in table");
    }

    #[test]
    fn display_source_revision_mismatch() {
        let err = ExecutionError::SourceRevisionMismatch {
            expected: "aaa".to_string(),
            actual: "bbb".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "source revision mismatch: plan expects 'aaa', grid has 'bbb'"
        );
    }

    #[test]
    fn display_missing_typed_parse() {
        let err = ExecutionError::MissingTypedParse {
            column_id: "column-2".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "no typed parse data was provided for column 'column-2', which is required by a decimal comparison"
        );
    }

    #[test]
    fn display_typed_parse_mismatch() {
        let err = ExecutionError::TypedParseMismatch {
            column_id: "column-2".to_string(),
            source_row: 7,
        };
        assert_eq!(
            err.to_string(),
            "typed parse data for column 'column-2' does not cover source row 7"
        );
    }

    #[test]
    fn display_column_mixed_no_parseable_values() {
        let err = ExecutionError::ColumnMixed {
            column_id: "column-1".to_string(),
            reason: ColumnMixedReason::NoParseableValues,
        };
        assert_eq!(
            err.to_string(),
            "column 'column-1' cannot back a decimal comparison: no nonblank value parses as a strict decimal"
        );
        assert_eq!(err.diagnostic_code(), "parse.column_mixed");
    }

    #[test]
    fn display_column_mixed_malformed_share_exceeded() {
        let err = ExecutionError::ColumnMixed {
            column_id: "column-1".to_string(),
            reason: ColumnMixedReason::MalformedShareExceeded,
        };
        assert_eq!(
            err.to_string(),
            "column 'column-1' cannot back a decimal comparison: the malformed share of nonblank values exceeds the 10% limit"
        );
        assert_eq!(err.diagnostic_code(), "parse.column_mixed");
    }

    #[test]
    fn diagnostic_codes_distinguish_type_mismatch_and_generic_failure() {
        let mismatch = ExecutionError::TypeMismatch {
            column_id: "column-0".to_string(),
            expected: "text".to_string(),
            actual: "numeric".to_string(),
        };
        assert_eq!(mismatch.diagnostic_code(), "plan.type_mismatch");

        let generic = ExecutionError::TableNotFound {
            table_id: "table-0".to_string(),
        };
        assert_eq!(generic.diagnostic_code(), "execution.failed");
    }
}
