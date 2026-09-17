use thiserror::Error;

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
}
