//! Generation selection and intake for `synth evaluate` (Epic 016, T1).
//!
//! Loads Agent 1's output directory, verifies it against the trusted dataset load,
//! assigns content hashes, and re-runs each stored SQL through the restricted runner.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::dataset::LoadedDataset;
use crate::sql::{SqlError, SqlRunner, parse_role_query};

const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CASES: usize = 100;
const SUPPORTED_METADATA_VERSION: u32 = 2;
const SUPPORTED_GENERATION_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum IntakeError {
    #[error("malformed generation ID {0:?}")]
    MalformedId(String),
    #[error("generation timestamp {0} is shared by more than one directory")]
    Tie(u128),
    #[error("no generation directories found")]
    NoGenerations,
    #[error("invalid generation: {0}")]
    Invalid(String),
    #[error("generation was made from a different dataset")]
    DatasetMismatch,
    #[error("cannot read generation files: {0}")]
    Io(#[from] std::io::Error),
}

fn invalid(reason: impl Into<String>) -> IntakeError {
    IntakeError::Invalid(reason.into())
}

/// Numeric ordering key of `generation-<nanos>-<pid>`. Never derived from file times.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GenerationKey {
    pub nanos: u128,
    pub pid: u64,
}

pub fn parse_generation_id(id: &str) -> Result<GenerationKey, IntakeError> {
    let bad = || IntakeError::MalformedId(id.chars().take(80).collect());
    let rest = id.strip_prefix("generation-").ok_or_else(bad)?;
    let (nanos, pid) = rest.split_once('-').ok_or_else(bad)?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 39 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(nanos) || !digits(pid) {
        return Err(bad());
    }
    Ok(GenerationKey {
        nanos: nanos.parse().map_err(|_| bad())?,
        pid: pid.parse().map_err(|_| bad())?,
    })
}

/// Picks the greatest generation under `root`. Entries not named `generation-*` are
/// ignored; a `generation-*` entry that is malformed or ties on timestamp is an error.
pub fn select_latest_generation(root: &Path) -> Result<PathBuf, IntakeError> {
    let mut found: Vec<(GenerationKey, PathBuf)> = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("generation-") || !entry.file_type()?.is_dir() {
            continue;
        }
        found.push((parse_generation_id(name)?, entry.path()));
    }
    found.sort_by_key(|(key, _)| *key);
    let (best, path) = found.pop().ok_or(IntakeError::NoGenerations)?;
    if found.iter().any(|(key, _)| key.nanos == best.nanos) {
        return Err(IntakeError::Tie(best.nanos));
    }
    Ok(path)
}

#[derive(Clone, Debug)]
pub struct IntakeCase {
    pub case_id: String,
    pub prompt: String,
    pub sql: String,
    /// SHA-256 over case ID, prompt, and SQL; binds human reviews to exact content.
    pub candidate_hash: String,
    pub expected_rows: Vec<u64>,
}

#[derive(Clone, Debug)]
pub struct LoadedGeneration {
    pub id: String,
    pub generation_hash: String,
    pub candidates_hash: String,
    pub metadata_hash: String,
    pub dataset_hash: String,
    pub cases: Vec<IntakeCase>,
}

#[derive(Deserialize)]
struct GenerationFile {
    schema_version: u32,
    id: String,
    dataset_hash: String,
}

#[derive(Deserialize)]
struct MetadataLine {
    schema_version: u32,
    case_id: String,
    dataset_hash: String,
    expected_rows: Vec<u64>,
    generation_record_id: String,
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn candidate_hash(case_id: &str, prompt: &str, sql: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [case_id, prompt, sql] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, IntakeError> {
    if fs::metadata(path)?.len() > MAX_FILE_BYTES {
        return Err(invalid("generation file exceeds size bound"));
    }
    Ok(fs::read(path)?)
}

/// Parses `candidates.txt` as written by Agent 1: `case_id:`/`prompt:`/`sql:` blocks whose
/// continuation lines are indented eight spaces.
fn parse_candidates(text: &str) -> Result<Vec<(String, String, String)>, IntakeError> {
    let mut out = Vec::new();
    let mut current: Option<(String, Option<String>, Option<String>)> = None;
    let mut last = 0u8; // 0 none, 1 prompt, 2 sql
    let finish = |c: Option<(String, Option<String>, Option<String>)>,
                  out: &mut Vec<(String, String, String)>|
     -> Result<(), IntakeError> {
        if let Some((id, p, s)) = c {
            out.push((
                id,
                p.ok_or_else(|| invalid("case without prompt"))?,
                s.ok_or_else(|| invalid("case without SQL"))?,
            ));
        }
        Ok(())
    };
    for line in text.lines() {
        if let Some(id) = line.strip_prefix("case_id: ") {
            finish(current.take(), &mut out)?;
            current = Some((id.to_owned(), None, None));
            last = 0;
        } else if let Some(p) = line.strip_prefix("prompt:  ") {
            let c = current
                .as_mut()
                .ok_or_else(|| invalid("prompt before case"))?;
            c.1 = Some(p.to_owned());
            last = 1;
        } else if let Some(s) = line.strip_prefix("sql:     ") {
            let c = current.as_mut().ok_or_else(|| invalid("SQL before case"))?;
            c.2 = Some(s.to_owned());
            last = 2;
        } else if let Some(more) = line.strip_prefix("        ") {
            let c = current
                .as_mut()
                .ok_or_else(|| invalid("stray continuation"))?;
            let slot = match last {
                1 => &mut c.1,
                2 => &mut c.2,
                _ => return Err(invalid("stray continuation")),
            };
            let slot = slot.as_mut().ok_or_else(|| invalid("stray continuation"))?;
            slot.push('\n');
            slot.push_str(more);
        } else if !line.trim().is_empty() {
            return Err(invalid("unrecognized line in candidates.txt"));
        }
    }
    finish(current, &mut out)?;
    Ok(out)
}

/// Loads a generation directory and verifies it against the trusted dataset load.
pub fn load_generation(
    dir: &Path,
    dataset: &LoadedDataset,
) -> Result<LoadedGeneration, IntakeError> {
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("generation path has no name"))?;
    parse_generation_id(name)?;
    let generation_bytes = read_bounded(&dir.join("generation.json"))?;
    let generation: GenerationFile = serde_json::from_slice(&generation_bytes)
        .map_err(|_| invalid("generation.json is not readable"))?;
    if generation.schema_version != SUPPORTED_GENERATION_VERSION {
        return Err(invalid("unsupported generation.json schema version"));
    }
    if generation.id != name {
        return Err(invalid("generation.json ID differs from directory name"));
    }
    if generation.dataset_hash != dataset.hash {
        return Err(IntakeError::DatasetMismatch);
    }
    let metadata_bytes = read_bounded(&dir.join("metadata.jsonl"))?;
    let candidates_bytes = read_bounded(&dir.join("candidates.txt"))?;
    let candidates = parse_candidates(
        std::str::from_utf8(&candidates_bytes)
            .map_err(|_| invalid("candidates.txt is not UTF-8"))?,
    )?;
    let mut metadata = Vec::new();
    for line in metadata_bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
    {
        let item: MetadataLine =
            serde_json::from_slice(line).map_err(|_| invalid("metadata line is not readable"))?;
        if item.schema_version != SUPPORTED_METADATA_VERSION {
            return Err(invalid("unsupported metadata schema version"));
        }
        if item.dataset_hash != dataset.hash {
            return Err(IntakeError::DatasetMismatch);
        }
        if item.generation_record_id != name || !item.case_id.starts_with(name) {
            return Err(invalid("metadata is bound to a different generation"));
        }
        metadata.push(item);
    }
    if candidates.len() != metadata.len() || candidates.len() > MAX_CASES {
        return Err(invalid(
            "candidate and metadata counts differ or exceed bound",
        ));
    }
    let mut by_id: HashMap<String, MetadataLine> = HashMap::new();
    for item in metadata {
        if by_id.contains_key(&item.case_id) {
            return Err(invalid("duplicate case ID in metadata"));
        }
        by_id.insert(item.case_id.clone(), item);
    }
    let mut cases = Vec::new();
    for (case_id, prompt, sql) in candidates {
        let meta = by_id
            .remove(&case_id)
            .ok_or_else(|| invalid("candidate has no metadata or is duplicated"))?;
        cases.push(IntakeCase {
            candidate_hash: candidate_hash(&case_id, &prompt, &sql),
            case_id,
            prompt,
            sql,
            expected_rows: meta.expected_rows,
        });
    }
    Ok(LoadedGeneration {
        id: name.to_owned(),
        generation_hash: sha(&generation_bytes),
        candidates_hash: sha(&candidates_bytes),
        metadata_hash: sha(&metadata_bytes),
        dataset_hash: generation.dataset_hash,
        cases,
    })
}

/// Result of re-running a stored SQL. Carries counts and codes, never row lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OracleCheck {
    Verified { literal: String },
    Mismatch { expected: usize, actual: usize },
    Error { code: &'static str },
}

impl OracleCheck {
    /// Only a verified oracle may be replayed against Baho.
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

pub fn recheck_oracle(
    case: &IntakeCase,
    dataset: &LoadedDataset,
    timeout: Duration,
) -> OracleCheck {
    let literal = match parse_role_query(&case.sql, &dataset.profile) {
        Ok(query) => query.literal,
        Err(error) => {
            return OracleCheck::Error {
                code: sql_code(&error),
            };
        }
    };
    let runner = SqlRunner::new(dataset.database_path(), &dataset.profile, timeout);
    match runner.run(&case.sql) {
        Ok(result) if result.rows == case.expected_rows => OracleCheck::Verified { literal },
        Ok(result) => OracleCheck::Mismatch {
            expected: case.expected_rows.len(),
            actual: result.rows.len(),
        },
        Err(error) => OracleCheck::Error {
            code: sql_code(&error),
        },
    }
}

fn sql_code(error: &SqlError) -> &'static str {
    match error {
        SqlError::Grammar => "sql_grammar",
        SqlError::UnknownRole => "sql_unknown_role",
        SqlError::DuckDbUnavailable(_) => "duckdb_unavailable",
        SqlError::DuckDb(_) => "duckdb_failed",
        SqlError::Timeout => "duckdb_timeout",
        SqlError::InvalidResult => "duckdb_invalid_result",
        SqlError::Io(_) => "io",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SQL_A: &str =
        "SELECT _row FROM data WHERE lower(\"Role\") = lower('Analyst') ORDER BY _row";
    const SQL_L: &str = "SELECT _row FROM data WHERE lower(\"Role\") = lower('Lead') ORDER BY _row";

    fn dataset() -> (tempfile::NamedTempFile, LoadedDataset) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "ID,Role\n1,Analyst\n2,analyst\n3,Lead\n").unwrap();
        let d = LoadedDataset::load(f.path()).unwrap();
        (f, d)
    }

    fn write_generation(
        root: &Path,
        id: &str,
        hash: &str,
        cases: &[(&str, &str, &str, &str)],
    ) -> PathBuf {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("generation.json"),
            format!(r#"{{"schema_version":1,"id":"{id}","dataset_hash":"{hash}"}}"#),
        )
        .unwrap();
        let (mut text, mut meta) = (String::new(), String::new());
        for (n, prompt, sql, rows) in cases {
            let case_id = format!("{id}-{n}");
            text.push_str(&format!(
                "case_id: {case_id}\nprompt:  {prompt}\nsql:     {sql}\n\n"
            ));
            meta.push_str(&format!(
                r#"{{"schema_version":2,"case_id":"{case_id}","dataset_hash":"{hash}","expected_rows":{rows},"generation_record_id":"{id}"}}"#
            ));
            meta.push('\n');
        }
        fs::write(dir.join("candidates.txt"), text).unwrap();
        fs::write(dir.join("metadata.jsonl"), meta).unwrap();
        dir
    }

    #[test]
    fn selection_is_numeric_not_lexical_or_mtime() {
        let root = tempfile::tempdir().unwrap();
        // Lexically "9..." > "10..." and the older directory is written last (newer mtime).
        fs::create_dir(root.path().join("generation-10000000000-5")).unwrap();
        fs::create_dir(root.path().join("generation-9000000000-5")).unwrap();
        fs::create_dir(root.path().join("task5-batch")).unwrap();
        let picked = select_latest_generation(root.path()).unwrap();
        assert!(picked.ends_with("generation-10000000000-5"));
    }

    #[test]
    fn selection_rejects_ties_and_malformed_ids() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("generation-5-1")).unwrap();
        fs::create_dir(root.path().join("generation-5-2")).unwrap();
        assert!(matches!(
            select_latest_generation(root.path()),
            Err(IntakeError::Tie(5))
        ));
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("generation-abc-1")).unwrap();
        assert!(matches!(
            select_latest_generation(root.path()),
            Err(IntakeError::MalformedId(_))
        ));
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            select_latest_generation(root.path()),
            Err(IntakeError::NoGenerations)
        ));
        for bad in [
            "generation-1",
            "generation--1",
            "generation-1-",
            "generation-1-2-3",
            "x-1-2",
            "generation-+1-2",
        ] {
            assert!(parse_generation_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn loads_generation_with_stable_hashes_and_multiline_fields() {
        let (_f, d) = dataset();
        let root = tempfile::tempdir().unwrap();
        let dir = write_generation(
            root.path(),
            "generation-7-1",
            &d.hash,
            &[
                (
                    "0001",
                    "Show rows\n        where Role is Analyst",
                    SQL_A,
                    "[1,2]",
                ),
                ("0002", "Lead only", SQL_L, "[3]"),
            ],
        );
        let g = load_generation(&dir, &d).unwrap();
        assert_eq!(g.cases.len(), 2);
        assert_eq!(g.cases[0].case_id, "generation-7-1-0001");
        assert_eq!(g.cases[0].prompt, "Show rows\nwhere Role is Analyst");
        assert_eq!(g.cases[1].expected_rows, [3]);
        let again = load_generation(&dir, &d).unwrap();
        assert_eq!(g.cases[0].candidate_hash, again.cases[0].candidate_hash);
        assert_ne!(g.cases[0].candidate_hash, g.cases[1].candidate_hash);
        assert_eq!(g.metadata_hash, again.metadata_hash);
        assert_eq!(g.dataset_hash, d.hash);
        assert_ne!(
            candidate_hash("a", "bc", "d"),
            candidate_hash("ab", "c", "d")
        );
    }

    #[test]
    fn rejects_wrong_dataset_and_inconsistent_files() {
        let (_f, d) = dataset();
        let root = tempfile::tempdir().unwrap();
        let dir = write_generation(
            root.path(),
            "generation-7-1",
            "deadbeef",
            &[("0001", "p", SQL_A, "[1,2]")],
        );
        assert!(matches!(
            load_generation(&dir, &d),
            Err(IntakeError::DatasetMismatch)
        ));
        let dir = write_generation(
            root.path(),
            "generation-8-1",
            &d.hash,
            &[("0001", "p", SQL_A, "[1,2]")],
        );
        fs::write(dir.join("metadata.jsonl"), "").unwrap();
        assert!(matches!(
            load_generation(&dir, &d),
            Err(IntakeError::Invalid(_))
        ));
        let dir = write_generation(
            root.path(),
            "generation-9-1",
            &d.hash,
            &[("0001", "p", SQL_A, "[1,2]")],
        );
        fs::write(
            dir.join("generation.json"),
            format!(
                r#"{{"schema_version":1,"id":"generation-10-1","dataset_hash":"{}"}}"#,
                d.hash
            ),
        )
        .unwrap();
        assert!(matches!(
            load_generation(&dir, &d),
            Err(IntakeError::Invalid(_))
        ));
    }

    #[test]
    fn oracle_recheck_verifies_mismatches_and_errors() {
        let (_f, d) = dataset();
        let timeout = Duration::from_secs(5);
        let case = |sql: &str, rows: Vec<u64>| IntakeCase {
            case_id: "c".into(),
            prompt: "p".into(),
            sql: sql.into(),
            candidate_hash: "h".into(),
            expected_rows: rows,
        };
        assert_eq!(
            recheck_oracle(&case(SQL_A, vec![1, 2]), &d, timeout),
            OracleCheck::Verified {
                literal: "Analyst".into()
            }
        );
        let wrong = recheck_oracle(&case(SQL_A, vec![1]), &d, timeout);
        assert_eq!(
            wrong,
            OracleCheck::Mismatch {
                expected: 1,
                actual: 2
            }
        );
        assert!(!wrong.is_verified());
        assert_eq!(
            recheck_oracle(&case("DROP TABLE data", vec![]), &d, timeout),
            OracleCheck::Error {
                code: "sql_grammar"
            }
        );
    }
}
