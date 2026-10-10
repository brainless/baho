//! Full-pipeline observations on bounded, neutral synthetic CSV fixtures.
//! These observations carry no assertion about what the prompt *should* mean.

use std::fs;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::cli_run::{
    CliBinary, CliRunError, DiagnosticSummary, RunLimits, create_evaluation_dir, parse_run,
    run_case,
};
use crate::contracts::{PlanComparison, compare_plan};
use crate::dataset::{DatasetError, LoadedDataset};
use crate::sql::{SqlError, SqlRunner};

const MAX_CSV_BYTES: usize = 32 * 1024;
const MAX_ROWS: usize = 128;
const MAX_COLUMNS: usize = 32;
const MAX_PROMPT_BYTES: usize = 2 * 1024;
const CANDIDATES_SCHEMA: u64 = 1;

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("invalid synthetic fixture: {0}")]
    Fixture(String),
    #[error("CLI probe failed: {0}")]
    Cli(#[from] CliRunError),
    #[error("probe run timed out or produced no artifact")]
    RunUnavailable,
    #[error("candidate artifact missing or malformed")]
    Candidates,
    #[error("probe I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("synthetic oracle preparation failed: {0}")]
    Dataset(#[from] DatasetError),
    #[error("synthetic oracle query failed: {0}")]
    Sql(#[from] SqlError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SyntheticRow {
    /// Zero-based logical source row. Row 0 is the header.
    pub source_row: u64,
    pub cells: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BodyObservation {
    pub source_row: u64,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProbeObservation {
    pub build_id: String,
    pub binary_sha256: String,
    pub fixture_sha256: String,
    pub prompt_sha256: String,
    pub result_id: String,
    pub run_id: String,
    pub outcome: String,
    pub source_rows: Vec<SyntheticRow>,
    pub body_rows: Vec<BodyObservation>,
    pub result_rows: Option<Vec<u64>>,
    pub diagnostic_codes: Vec<String>,
    pub diagnostics: Vec<DiagnosticSummary>,
    pub plan: Option<crate::contracts::PlanEvidence>,
    pub oracle: Option<OracleObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OracleObservation {
    /// DuckDB's ordered source rows, computed from the restricted Role SQL.
    pub expected_rows: Vec<u64>,
    pub ordered_rows_match: bool,
    pub plan_comparison: PlanComparison,
}

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Validate and enumerate complete CSV records in the same zero-based order used by
/// Baho provenance. Quoted newlines remain one logical record.
pub fn validate_synthetic(csv: &[u8]) -> Result<Vec<SyntheticRow>, ProbeError> {
    if csv.is_empty() || csv.len() > MAX_CSV_BYTES || csv.contains(&0) {
        return Err(ProbeError::Fixture(
            "CSV is empty, too large, or contains NUL".into(),
        ));
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(csv);
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|_| ProbeError::Fixture("invalid CSV record".into()))?;
        if record.is_empty() || record.len() > MAX_COLUMNS {
            return Err(ProbeError::Fixture("invalid column count".into()));
        }
        if rows.len() > MAX_ROWS {
            return Err(ProbeError::Fixture("too many data rows".into()));
        }
        rows.push(SyntheticRow {
            source_row: rows.len() as u64,
            cells: record.iter().map(str::to_owned).collect(),
        });
    }
    if rows.len() < 2 {
        return Err(ProbeError::Fixture("header and data row required".into()));
    }
    if rows[0].cells.iter().any(String::is_empty) {
        return Err(ProbeError::Fixture("empty header cell".into()));
    }
    Ok(rows)
}

/// Execute the supplied CLI build on a synthetic fixture. `work_root` holds only
/// synthetic artifacts; the caller controls its lifecycle and build confinement.
pub fn probe(
    binary: &CliBinary,
    build_id: &str,
    work_root: &Path,
    csv: &[u8],
    prompt: &str,
    limits: RunLimits,
) -> Result<ProbeObservation, ProbeError> {
    probe_with_sql(binary, build_id, work_root, csv, prompt, None, limits)
}

/// Optional SQL uses only the existing restricted Role equality grammar.
pub fn probe_with_sql(
    binary: &CliBinary,
    build_id: &str,
    work_root: &Path,
    csv: &[u8],
    prompt: &str,
    sql: Option<&str>,
    limits: RunLimits,
) -> Result<ProbeObservation, ProbeError> {
    if build_id.is_empty()
        || build_id.len() > 128
        || !build_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(ProbeError::Fixture("invalid build ID".into()));
    }
    if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES || prompt.contains('\0') {
        return Err(ProbeError::Fixture("invalid prompt".into()));
    }
    let source_rows = validate_synthetic(csv)?;
    let expected = match sql {
        Some(sql) => {
            let role_index = source_rows[0]
                .cells
                .iter()
                .position(|cell| cell == "Role")
                .ok_or_else(|| ProbeError::Fixture("SQL requires a Role column".into()))?;
            let role_rows: Vec<_> = source_rows
                .iter()
                .skip(1)
                .map(|row| (row.source_row, row.cells[role_index].clone()))
                .collect();
            let dataset = LoadedDataset::from_synthetic_role_rows(&role_rows)?;
            let literal = crate::sql::parse_role_query(sql, &dataset.profile)?.literal;
            let rows = SqlRunner::new(dataset.database_path(), &dataset.profile, limits.timeout)
                .run(sql)?
                .rows;
            Some((rows, literal))
        }
        None => None,
    };
    let dir = create_evaluation_dir(work_root)?;
    let source_path = dir.join("synthetic.csv");
    fs::write(&source_path, csv)?;
    let run = run_case(binary, &dir, &source_path, prompt, limits)?;
    if run.timed_out {
        return Err(ProbeError::RunUnavailable);
    }
    let run_dir = run.run_dir.ok_or(ProbeError::RunUnavailable)?;
    let parsed = parse_run(&run_dir)?;
    let body_rows = selected_body_rows(&run_dir)?;
    let outcome = format!("{:?}", parsed.outcome);
    let diagnostic_codes = parsed.diagnostics.iter().map(|d| d.code.clone()).collect();
    let oracle = match expected {
        Some((expected_rows, literal)) => Some(OracleObservation {
            ordered_rows_match: parsed.result_rows.as_ref() == Some(&expected_rows),
            plan_comparison: compare_plan("Role", &literal, parsed.plan_evidence.as_ref()),
            expected_rows,
        }),
        None => None,
    };
    let mut observation = ProbeObservation {
        build_id: build_id.to_owned(),
        binary_sha256: binary.sha256.clone(),
        fixture_sha256: digest(csv),
        prompt_sha256: digest(prompt),
        result_id: String::new(),
        run_id: parsed.run_id,
        outcome,
        source_rows,
        body_rows,
        result_rows: parsed.result_rows,
        diagnostic_codes,
        diagnostics: parsed.diagnostics,
        plan: parsed.plan_evidence,
        oracle,
    };
    // The result ID covers observations and identities, excluding the volatile run ID.
    let mut stable = observation.clone();
    stable.run_id.clear();
    observation.result_id = digest(serde_json::to_vec(&stable).expect("serializable probe"));
    Ok(observation)
}

pub(crate) fn selected_body_rows(run_dir: &Path) -> Result<Vec<BodyObservation>, ProbeError> {
    let path = run_dir.join("candidates.json");
    let metadata = fs::metadata(&path).map_err(|_| ProbeError::Candidates)?;
    if metadata.len() > 256 * 1024 {
        return Err(ProbeError::Candidates);
    }
    let artifact: serde_json::Value =
        serde_json::from_slice(&fs::read(path)?).map_err(|_| ProbeError::Candidates)?;
    if artifact["schema_version"].as_u64() != Some(CANDIDATES_SCHEMA) {
        return Err(ProbeError::Candidates);
    }
    let Some(selected) = artifact.get("selected") else {
        return Err(ProbeError::Candidates);
    };
    if selected.is_null() {
        return Ok(Vec::new());
    }
    let classifications = selected["body_row_classifications"]
        .as_array()
        .ok_or(ProbeError::Candidates)?;
    classifications
        .iter()
        .map(|row| {
            Ok(BodyObservation {
                source_row: row["source_row"].as_u64().ok_or(ProbeError::Candidates)?,
                kind: row["kind"]
                    .as_str()
                    .ok_or(ProbeError::Candidates)?
                    .to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_wholly_blank_record_separately_from_empty_role_cell() {
        let rows = validate_synthetic(b"Id,Role\n1,\n,\n2,Analyst\n").unwrap();
        assert_eq!(rows[1].cells, ["1", ""]);
        assert_eq!(rows[2].cells, ["", ""]);
        assert_eq!(
            rows.iter().map(|r| r.source_row).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert!(validate_synthetic(b"Id,Role\n1\n").is_err());
    }

    #[test]
    fn full_pipeline_reports_body_membership_for_blank_records() {
        let Ok(bin) = std::env::var("BAHO_PROBE_BIN") else {
            return;
        };
        let binary = CliBinary::probe(Path::new(&bin)).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let result = probe_with_sql(
            &binary,
            "test-build",
            temp.path(),
            b"Id,Role\n1,Analyst\n2,\n,\n3,Analyst\n",
            "show rows where Role = Analyst",
            Some("SELECT _row FROM data WHERE lower(\"Role\") = lower('Analyst') ORDER BY _row"),
            RunLimits::default(),
        )
        .unwrap();
        assert_eq!(result.source_rows[2].cells, ["2", ""]);
        assert_eq!(result.source_rows[3].cells, ["", ""]);
        assert!(
            result
                .body_rows
                .iter()
                .any(|r| r.source_row == 2 && r.kind == "Data")
        );
        assert!(
            !result
                .body_rows
                .iter()
                .any(|r| r.source_row == 3 && r.kind == "Data")
        );
        assert_eq!(result.result_rows, Some(vec![1, 4]));
        let oracle = result.oracle.unwrap();
        assert_eq!(oracle.expected_rows, [1, 4]);
        assert!(oracle.ordered_rows_match);
        assert_eq!(oracle.plan_comparison, PlanComparison::Equivalent);
        assert!(!result.result_id.is_empty());
    }
}
