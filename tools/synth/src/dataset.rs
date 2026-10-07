use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use thiserror::Error;

const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ROWS: usize = 10_000;
const MAX_ROLES: usize = 32;

#[derive(Debug, Error)]
pub enum DatasetError {
    #[error("cannot read CSV: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV exceeds the supported 8 MiB size limit")]
    TooLarge,
    #[error("unsupported CSV layout: {0}")]
    Layout(String),
    #[error("CSV parsing failed: {0}")]
    Csv(#[from] csv::Error),
    #[error("DuckDB CLI is unavailable: {0}")]
    DuckDbUnavailable(std::io::Error),
    #[error("trusted DuckDB loading failed: {0}")]
    DuckDbLoad(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RoleCount {
    pub value: String,
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DatasetProfile {
    pub column: &'static str,
    pub total_rows: usize,
    pub roles: Vec<RoleCount>,
}

impl DatasetProfile {
    pub fn contains_role(&self, value: &str) -> bool {
        self.roles
            .iter()
            .any(|entry| entry.value.eq_ignore_ascii_case(value))
    }
}

/// Owns the private temporary database. Keep this value alive while SQL is run.
pub struct LoadedDataset {
    _directory: TempDir,
    database: PathBuf,
    pub profile: DatasetProfile,
    pub hash: String,
}

impl LoadedDataset {
    pub fn load(path: &Path) -> Result<Self, DatasetError> {
        let metadata = fs::metadata(path)?;
        if metadata.len() > MAX_SOURCE_BYTES {
            return Err(DatasetError::TooLarge);
        }
        let bytes = fs::read(path)?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(DatasetError::TooLarge);
        }
        let (rows, profile) = parse_csv(&bytes)?;
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let directory = tempfile::Builder::new().prefix("baho-synth-").tempdir()?;
        let database = directory.path().join("roles.duckdb");
        prepare_database(&database, &rows)?;
        Ok(Self {
            _directory: directory,
            database,
            profile,
            hash,
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.database
    }
}

fn parse_csv(bytes: &[u8]) -> Result<(Vec<(i64, String)>, DatasetProfile), DatasetError> {
    if bytes.is_empty() || bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(DatasetError::Layout("empty or BOM-prefixed input".into()));
    }
    if bytes.iter().any(|byte| *byte == 0) {
        return Err(DatasetError::Layout("NUL byte".into()));
    }
    let physical_lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let line_count = physical_lines.len() - usize::from(bytes.last() == Some(&b'\n'));
    if physical_lines[..line_count]
        .iter()
        .any(|line| line.strip_suffix(b"\r").unwrap_or(line).is_empty())
    {
        return Err(DatasetError::Layout("blank physical line".into()));
    }

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(bytes);
    let mut records = reader.records();
    let header = records
        .next()
        .ok_or_else(|| DatasetError::Layout("missing header".into()))??;
    if header
        .position()
        .is_none_or(|position| position.line() != 1)
    {
        return Err(DatasetError::Layout("header must start on line 1".into()));
    }
    if header.is_empty() || header.iter().any(str::is_empty) {
        return Err(DatasetError::Layout("empty header cell".into()));
    }
    let mut names = HashSet::new();
    if header.iter().any(|name| !names.insert(name)) {
        return Err(DatasetError::Layout("duplicate header".into()));
    }
    let role_index = header
        .iter()
        .position(|name| name == "Role")
        .ok_or_else(|| DatasetError::Layout("missing Role column".into()))?;
    let mut rows = Vec::new();
    let mut counts = BTreeMap::<String, usize>::new();
    for (index, record) in records.enumerate() {
        let record = record?;
        let source_row = index + 1;
        if source_row > MAX_ROWS {
            return Err(DatasetError::Layout("too many rows".into()));
        }
        if record
            .position()
            .is_none_or(|position| position.line() != (source_row + 1) as u64)
        {
            return Err(DatasetError::Layout("multiline or skipped record".into()));
        }
        let role = record.get(role_index).expect("rectangular CSV record");
        if role.is_empty() || !role.is_ascii() || role.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(DatasetError::Layout(
                "Role must be nonempty ASCII text".into(),
            ));
        }
        *counts.entry(role.to_owned()).or_default() += 1;
        if counts.len() > MAX_ROLES {
            return Err(DatasetError::Layout("too many distinct Role values".into()));
        }
        rows.push((source_row as i64, role.to_owned()));
    }
    if rows.is_empty() || rows.len() + 1 != line_count {
        return Err(DatasetError::Layout(
            "missing rows or non-rectangular physical layout".into(),
        ));
    }
    let roles = counts
        .into_iter()
        .map(|(value, count)| RoleCount { value, count })
        .collect();
    let profile = DatasetProfile {
        column: "Role",
        total_rows: rows.len(),
        roles,
    };
    Ok((rows, profile))
}

fn prepare_database(path: &Path, rows: &[(i64, String)]) -> Result<(), DatasetError> {
    let mut child = Command::new("duckdb")
        .arg("-no-init")
        .arg("-bail")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(DatasetError::DuckDbUnavailable)?;
    {
        let mut input = child.stdin.take().expect("piped stdin");
        writeln!(
            input,
            "CREATE TABLE data (_row BIGINT PRIMARY KEY, Role VARCHAR NOT NULL);"
        )?;
        writeln!(input, "BEGIN TRANSACTION;")?;
        for (row, role) in rows {
            writeln!(
                input,
                "INSERT INTO data VALUES ({row}, '{}');",
                role.replace('\'', "''")
            )?;
        }
        writeln!(input, "COMMIT;")?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(DatasetError::DuckDbLoad(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_profile_and_source_rows() {
        let (rows, profile) = parse_csv(b"ID,Role\n1,Analyst\n2,analyst\n3,Lead\n").unwrap();
        assert_eq!(
            rows,
            [
                (1, "Analyst".into()),
                (2, "analyst".into()),
                (3, "Lead".into())
            ]
        );
        assert_eq!(profile.total_rows, 3);
        assert!(profile.contains_role("ANALYST"));
        assert_eq!(profile.roles.len(), 3);
    }

    #[test]
    fn rejects_unsupported_layouts() {
        for csv in [
            "Role,X\na,1\n\nb,2\n",
            "Role,X\na,1\nb\n",
            "Role,X\n\"a\nb\",1\n",
            "Role,Role\na,b\n",
            "X,Y\na,b\n",
            "Role,X\n,1\n",
        ] {
            assert!(parse_csv(csv.as_bytes()).is_err(), "{csv:?}");
        }
    }
}
