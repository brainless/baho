use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use thiserror::Error;

use crate::dataset::DatasetProfile;

const MAX_SQL_BYTES: usize = 512;
const MAX_RESULT_ROWS: usize = 10_000;
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum SqlError {
    #[error("SQL is outside the supported Role equality grammar")]
    Grammar,
    #[error("SQL literal is not a profiled Role value")]
    UnknownRole,
    #[error("DuckDB CLI is unavailable: {0}")]
    DuckDbUnavailable(std::io::Error),
    #[error("DuckDB query failed: {0}")]
    DuckDb(String),
    #[error("DuckDB query timed out")]
    Timeout,
    #[error("DuckDB returned malformed or unsafe results")]
    InvalidResult,
    #[error("temporary query I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleQuery {
    pub literal: String,
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) -> bool {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
        self.at > start
    }

    fn token(&mut self, expected: &str) -> Result<(), SqlError> {
        let end = self.at + expected.len();
        let actual = self.bytes.get(self.at..end).ok_or(SqlError::Grammar)?;
        if !actual.eq_ignore_ascii_case(expected.as_bytes())
            || self
                .bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(SqlError::Grammar);
        }
        self.at = end;
        Ok(())
    }

    fn exact(&mut self, expected: &[u8]) -> Result<(), SqlError> {
        if self.bytes.get(self.at..self.at + expected.len()) != Some(expected) {
            return Err(SqlError::Grammar);
        }
        self.at += expected.len();
        Ok(())
    }

    fn required_space(&mut self) -> Result<(), SqlError> {
        self.space().then_some(()).ok_or(SqlError::Grammar)
    }

    fn literal(&mut self) -> Result<String, SqlError> {
        self.exact(b"'")?;
        let mut value = Vec::new();
        loop {
            let byte = *self.bytes.get(self.at).ok_or(SqlError::Grammar)?;
            self.at += 1;
            if byte == b'\'' {
                if self.bytes.get(self.at) == Some(&b'\'') {
                    value.push(b'\'');
                    self.at += 1;
                } else {
                    break;
                }
            } else if byte.is_ascii_graphic() || byte == b' ' {
                value.push(byte);
            } else {
                return Err(SqlError::Grammar);
            }
        }
        if value.is_empty() {
            return Err(SqlError::Grammar);
        }
        String::from_utf8(value).map_err(|_| SqlError::Grammar)
    }
}

/// Parse the entire accepted query. No comments, semicolon, second statement, or extra clause.
pub fn parse_role_query(sql: &str, profile: &DatasetProfile) -> Result<RoleQuery, SqlError> {
    if sql.len() > MAX_SQL_BYTES || !sql.is_ascii() {
        return Err(SqlError::Grammar);
    }
    let mut parser = Parser {
        bytes: sql.as_bytes(),
        at: 0,
    };
    parser.space();
    parser.token("SELECT")?;
    parser.required_space()?;
    parser.token("_row")?;
    parser.required_space()?;
    parser.token("FROM")?;
    parser.required_space()?;
    parser.token("data")?;
    parser.required_space()?;
    parser.token("WHERE")?;
    parser.required_space()?;
    parser.token("lower")?;
    parser.space();
    parser.exact(b"(")?;
    parser.space();
    parser.exact(b"\"Role\"")?;
    parser.space();
    parser.exact(b")")?;
    parser.space();
    parser.exact(b"=")?;
    parser.space();
    parser.token("lower")?;
    parser.space();
    parser.exact(b"(")?;
    parser.space();
    let literal = parser.literal()?;
    parser.space();
    parser.exact(b")")?;
    parser.required_space()?;
    parser.token("ORDER")?;
    parser.required_space()?;
    parser.token("BY")?;
    parser.required_space()?;
    parser.token("_row")?;
    parser.space();
    if parser.at != parser.bytes.len() {
        return Err(SqlError::Grammar);
    }
    if !profile.contains_role(&literal) {
        return Err(SqlError::UnknownRole);
    }
    Ok(RoleQuery { literal })
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueryResult {
    pub rows: Vec<u64>,
    pub count: usize,
    pub selectivity: f64,
    pub is_empty: bool,
    pub is_all_rows: bool,
}

pub struct SqlRunner<'a> {
    database: &'a Path,
    profile: &'a DatasetProfile,
    timeout: Duration,
}

impl<'a> SqlRunner<'a> {
    pub fn new(database: &'a Path, profile: &'a DatasetProfile, timeout: Duration) -> Self {
        Self {
            database,
            profile,
            timeout,
        }
    }

    pub fn run(&self, sql: &str) -> Result<QueryResult, SqlError> {
        parse_role_query(sql, self.profile)?;
        let output_dir = tempfile::tempdir()?;
        let stdout_path = output_dir.path().join("stdout");
        let stderr_path = output_dir.path().join("stderr");
        let mut child = Command::new("duckdb")
            .args(["-safe", "-readonly", "-no-init", "-json", "-bail"])
            .arg(self.database)
            .arg("-c")
            .arg(sql)
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(&stdout_path)?))
            .stderr(Stdio::from(File::create(&stderr_path)?))
            .spawn()
            .map_err(SqlError::DuckDbUnavailable)?;
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                let stderr = fs::read_to_string(&stderr_path)?;
                if !status.success() || !stderr.is_empty() {
                    return Err(SqlError::DuckDb(stderr.trim().chars().take(512).collect()));
                }
                break;
            }
            if Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                return Err(SqlError::Timeout);
            }
            thread::sleep(Duration::from_millis(10));
        }
        if fs::metadata(&stdout_path)?.len() > MAX_OUTPUT_BYTES {
            return Err(SqlError::InvalidResult);
        }
        let output = fs::read(&stdout_path)?;
        let values: Vec<serde_json::Value> =
            serde_json::from_slice(&output).map_err(|_| SqlError::InvalidResult)?;
        if values.len() > MAX_RESULT_ROWS || values.len() > self.profile.total_rows {
            return Err(SqlError::InvalidResult);
        }
        let mut rows = Vec::with_capacity(values.len());
        for item in values {
            let object = item.as_object().ok_or(SqlError::InvalidResult)?;
            if object.len() != 1 {
                return Err(SqlError::InvalidResult);
            }
            let row = object
                .get("_row")
                .and_then(serde_json::Value::as_u64)
                .ok_or(SqlError::InvalidResult)?;
            if row == 0
                || row > self.profile.total_rows as u64
                || rows.last().is_some_and(|previous| *previous >= row)
            {
                return Err(SqlError::InvalidResult);
            }
            rows.push(row);
        }
        let count = rows.len();
        Ok(QueryResult {
            rows,
            count,
            selectivity: count as f64 / self.profile.total_rows as f64,
            is_empty: count == 0,
            is_all_rows: count == self.profile.total_rows,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{LoadedDataset, RoleCount};
    use std::io::Write;

    fn profile() -> DatasetProfile {
        DatasetProfile {
            column: "Role",
            total_rows: 3,
            roles: vec![RoleCount {
                value: "Lead's aide".into(),
                count: 2,
            }],
        }
    }

    #[test]
    fn grammar_accepts_whitespace_and_escaped_quote() {
        let query = parse_role_query(
            "\n SELECT _row FROM data WHERE lower ( \"Role\" ) = lower('Lead''s aide') ORDER BY _row \t",
            &profile(),
        ).unwrap();
        assert_eq!(query.literal, "Lead's aide");
    }

    #[test]
    fn grammar_rejects_escapes_and_other_sql() {
        for sql in [
            "SELECT * FROM data WHERE lower(\"Role\") = lower('Lead''s aide') ORDER BY _row",
            "SELECT _row FROM data WHERE lower(\"Other\") = lower('Lead''s aide') ORDER BY _row",
            "SELECT _row FROM data WHERE lower(\"Role\") = lower('Lead''s aide') ORDER BY _row; DROP TABLE data",
            "SELECT _row FROM read_csv('/etc/passwd') WHERE lower(\"Role\") = lower('Lead''s aide') ORDER BY _row",
            "INSERT INTO data VALUES (1,'x')",
            "SELECT _row FROM data WHERE lower(\"Role\") = lower('Lead''s aide') OR TRUE ORDER BY _row",
            "SELECT _row FROM data WHERE lower(\"Role\") = lower('Lead''s aide') ORDER BY _row -- hi",
            "SELECT _row FROM data WHERE lower(\"Role\") = lower('unknown') ORDER BY _row",
        ] {
            assert!(parse_role_query(sql, &profile()).is_err(), "{sql}");
        }
    }

    #[test]
    fn safe_readonly_process_returns_ordered_source_rows() {
        let mut source = tempfile::NamedTempFile::new().unwrap();
        write!(source, "ID,Role\n1,Analyst\n2,analyst\n3,Lead\n").unwrap();
        let dataset = LoadedDataset::load(source.path()).unwrap();
        let runner = SqlRunner::new(
            dataset.database_path(),
            &dataset.profile,
            Duration::from_secs(5),
        );
        let result = runner
            .run("SELECT _row FROM data WHERE lower(\"Role\") = lower('ANALYST') ORDER BY _row")
            .unwrap();
        assert_eq!(result.rows, [1, 2]);
        assert_eq!(result.count, 2);
        assert_eq!(result.selectivity, 2.0 / 3.0);
        assert!(!result.is_empty);
        assert!(!result.is_all_rows);
    }
}
