//! Versioned, fail-closed repair attempt orchestration. A trusted worker supplies
//! confined builds and probes; agents never choose commands or edit expectations.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::contracts::PlanComparison;

/// Version 2 pins the source-tool budgets in `AttemptSettings` (Epic 019).
/// Version 1 contracts and manifests lack them and are rejected, not defaulted,
/// because no attempt was accepted under version 1.
pub const ATTEMPT_SCHEMA: u32 = 2;
const MAX_PATCH_BYTES: usize = 256 * 1024;
const MAX_TREE_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenCase {
    pub fixture_sha256: String,
    pub prompt_sha256: String,
    pub sql_sha256: String,
    pub expected_source_rows: Vec<u64>,
    pub expected_literal: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairContract {
    pub schema_version: u32,
    pub settings: AttemptSettings,
    pub baseline_revision: String,
    pub evaluation_sha256: String,
    pub generation_sha256: String,
    pub review_sha256: String,
    pub oracle_sha256: String,
    pub target_case_id: String,
    pub target: FrozenCase,
    pub reduced: FrozenCase,
    pub reduction: SyntheticReduction,
    pub protected_cases: Vec<FrozenCase>,
    pub parent_attempt_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptSettings {
    pub investigator_prompt_version: String,
    pub coder_prompt_version: String,
    pub reviewer_prompt_version: String,
    pub model: String,
    pub max_agent_turns: u32,
    pub max_tool_calls: u32,
    pub process_timeout_seconds: u64,
    pub max_output_bytes: usize,
    /// Source text returned by one `read_lines` response.
    pub max_read_bytes: usize,
    /// Outline text returned by one `outline` page.
    pub max_outline_bytes: usize,
    /// Matches returned by one `search_text` response.
    pub max_search_matches: usize,
    /// Total source-derived bytes exposed to one stage.
    pub max_exposed_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticReduction {
    pub csv: String,
    pub prompt: String,
    pub sql: String,
}

impl RepairContract {
    pub fn parse(bytes: &[u8]) -> Result<Self, RepairError> {
        let contract: Self =
            serde_json::from_slice(bytes).map_err(|_| RepairError::InvalidContract)?;
        contract.validate()?;
        Ok(contract)
    }

    pub fn validate(&self) -> Result<(), RepairError> {
        if self.schema_version != ATTEMPT_SCHEMA {
            return Err(RepairError::UnknownSchema(self.schema_version));
        }
        if !is_hash(&self.baseline_revision, 40)
            || self.settings.investigator_prompt_version.is_empty()
            || self.settings.coder_prompt_version.is_empty()
            || self.settings.reviewer_prompt_version.is_empty()
            || self.settings.model.is_empty()
            || self.settings.max_agent_turns == 0
            || self.settings.max_tool_calls == 0
            || self.settings.process_timeout_seconds == 0
            || self.settings.max_output_bytes == 0
            || self.settings.max_output_bytes > 1024 * 1024
            || !(1..=16 * 1024).contains(&self.settings.max_read_bytes)
            || !(1..=32 * 1024).contains(&self.settings.max_outline_bytes)
            || !(1..=200).contains(&self.settings.max_search_matches)
            || !(1..=1024 * 1024).contains(&self.settings.max_exposed_bytes)
            || ![
                &self.evaluation_sha256,
                &self.generation_sha256,
                &self.review_sha256,
                &self.oracle_sha256,
            ]
            .iter()
            .all(|v| is_hash(v, 64))
            || self.target_case_id.is_empty()
            || self.target_case_id.len() > 128
            || self.protected_cases.len() > 64
        {
            return Err(RepairError::InvalidContract);
        }
        for case in std::iter::once(&self.target)
            .chain(std::iter::once(&self.reduced))
            .chain(&self.protected_cases)
        {
            if !is_hash(&case.fixture_sha256, 64)
                || !is_hash(&case.prompt_sha256, 64)
                || !is_hash(&case.sql_sha256, 64)
                || case.expected_literal.is_empty()
                || case.expected_source_rows.len() > 10_000
                || case.expected_source_rows.windows(2).any(|w| w[0] >= w[1])
                || case.expected_source_rows.contains(&0)
            {
                return Err(RepairError::InvalidContract);
            }
        }
        if self.reduction.csv.len() > 32 * 1024
            || self.reduction.prompt.len() > 2 * 1024
            || self.reduction.sql.len() > 512
            || hash(self.reduction.csv.as_bytes()) != self.reduced.fixture_sha256
            || hash(self.reduction.prompt.as_bytes()) != self.reduced.prompt_sha256
            || hash(self.reduction.sql.as_bytes()) != self.reduced.sql_sha256
            || crate::probe::validate_synthetic(self.reduction.csv.as_bytes()).is_err()
        {
            return Err(RepairError::InvalidContract);
        }
        let rows = crate::probe::validate_synthetic(self.reduction.csv.as_bytes())
            .map_err(|_| RepairError::InvalidContract)?;
        let role = rows[0]
            .cells
            .iter()
            .position(|h| h == "Role")
            .ok_or(RepairError::InvalidContract)?;
        let profile = crate::dataset::DatasetProfile {
            column: "Role",
            total_rows: rows.len() - 1,
            roles: rows
                .iter()
                .skip(1)
                .map(|row| crate::dataset::RoleCount {
                    value: row.cells[role].clone(),
                    count: 1,
                })
                .collect(),
        };
        let query = crate::sql::parse_role_query(&self.reduction.sql, &profile)
            .map_err(|_| RepairError::InvalidContract)?;
        if query.literal != self.reduced.expected_literal {
            return Err(RepairError::InvalidContract);
        }
        Ok(())
    }
}

fn is_hash(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn freeze_synthetic(reduction: &SyntheticReduction) -> Result<FrozenCase, Outcome> {
    let rows = crate::probe::validate_synthetic(reduction.csv.as_bytes())
        .map_err(|_| Outcome::NeedsDesign)?;
    let role = rows[0]
        .cells
        .iter()
        .position(|cell| cell == "Role")
        .ok_or(Outcome::NeedsDesign)?;
    let values: Vec<_> = rows
        .iter()
        .skip(1)
        .map(|row| (row.source_row, row.cells[role].clone()))
        .collect();
    let dataset = crate::dataset::LoadedDataset::from_synthetic_role_rows(&values)
        .map_err(|_| Outcome::EnvironmentFailure)?;
    let query = crate::sql::parse_role_query(&reduction.sql, &dataset.profile)
        .map_err(|_| Outcome::NeedsDesign)?;
    let result = crate::sql::SqlRunner::new(
        dataset.database_path(),
        &dataset.profile,
        std::time::Duration::from_secs(30),
    )
    .run(&reduction.sql)
    .map_err(|error| match error {
        crate::sql::SqlError::DuckDbUnavailable(_) | crate::sql::SqlError::Timeout => {
            Outcome::EnvironmentFailure
        }
        _ => Outcome::NeedsDesign,
    })?;
    Ok(FrozenCase {
        fixture_sha256: hash(&reduction.csv),
        prompt_sha256: hash(&reduction.prompt),
        sql_sha256: hash(&reduction.sql),
        expected_source_rows: result.rows,
        expected_literal: query.literal,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Reserved,
    Baseline,
    Investigation,
    Regression,
    Coding,
    Check,
    Review,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted,
    Rejected,
    NoChange,
    NeedsDesign,
    InvalidInput,
    BaselineFailure,
    EnvironmentFailure,
    RegressionFailure,
    BuildFailure,
    CheckFailure,
    UnavailableComparison,
    ModelExhausted,
    ProcessTimeout,
    ReviewIncomplete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptManifest {
    pub schema_version: u32,
    pub attempt_id: String,
    pub stage: Stage,
    pub outcome: Option<Outcome>,
    pub contract_sha256: String,
    pub baseline_build_id: Option<String>,
    pub candidate_build_id: Option<String>,
    pub patch_sha256: Option<String>,
    pub reason: Option<String>,
    pub cleanup_error: Option<String>,
    /// Redacted questions a stage left open when it declared insufficient
    /// evidence. Model output, so unverified.
    #[serde(default)]
    pub open_questions: Vec<String>,
}

impl AttemptManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, RepairError> {
        let manifest: Self =
            serde_json::from_slice(bytes).map_err(|_| RepairError::InvalidContract)?;
        if manifest.schema_version != ATTEMPT_SCHEMA {
            return Err(RepairError::UnknownSchema(manifest.schema_version));
        }
        Ok(manifest)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttemptEvent {
    pub schema_version: u32,
    pub timestamp_ms: u128,
    pub stage: Stage,
    pub name: String,
    pub detail: String,
}

#[derive(Debug, Error)]
pub enum RepairError {
    #[error("unknown attempt schema version {0}")]
    UnknownSchema(u32),
    #[error("invalid attempt contract")]
    InvalidContract,
    #[error("denied candidate edit: {0}")]
    DeniedEdit(String),
    #[error("attempt I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("attempt serialization: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct AttemptRecord {
    pub dir: PathBuf,
    pub manifest: AttemptManifest,
}

impl AttemptRecord {
    pub fn reserve(root: &Path, contract: &RepairContract) -> Result<Self, RepairError> {
        contract.validate()?;
        fs::create_dir_all(root)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for number in 0..100u32 {
            let id = format!("repair-{nanos}-{}-{number}", std::process::id());
            let dir = root.join(&id);
            match fs::create_dir(&dir) {
                Ok(()) => {
                    fs::create_dir(dir.join("protected"))?;
                    fs::create_dir(dir.join("public"))?;
                    let bytes = serde_json::to_vec_pretty(contract)?;
                    fs::write(dir.join("protected/contract.json"), &bytes)?;
                    let mut record = Self {
                        dir,
                        manifest: AttemptManifest {
                            schema_version: ATTEMPT_SCHEMA,
                            attempt_id: id,
                            stage: Stage::Reserved,
                            outcome: None,
                            contract_sha256: hash(bytes),
                            baseline_build_id: None,
                            candidate_build_id: None,
                            patch_sha256: None,
                            reason: None,
                            cleanup_error: None,
                            open_questions: Vec::new(),
                        },
                    };
                    record.persist()?;
                    record.event(Stage::Reserved, "attempt_reserved", "")?;
                    return Ok(record);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(RepairError::Io(std::io::Error::other(
            "attempt ID exhausted",
        )))
    }

    fn persist(&mut self) -> Result<(), RepairError> {
        let temp = self.dir.join("manifest.json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(&self.manifest)?)?;
        fs::rename(temp, self.dir.join("manifest.json"))?;
        Ok(())
    }

    fn event(&self, stage: Stage, name: &str, detail: &str) -> Result<(), RepairError> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.dir.join("events.jsonl"))?;
        serde_json::to_writer(
            &mut file,
            &AttemptEvent {
                schema_version: ATTEMPT_SCHEMA,
                timestamp_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                stage,
                name: name.into(),
                detail: detail.into(),
            },
        )?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn advance(&mut self, stage: Stage, event: &str) -> Result<(), RepairError> {
        self.manifest.stage = stage;
        self.persist()?;
        self.event(stage, event, "")
    }

    fn finish(&mut self, outcome: Outcome, reason: &str) -> Result<(), RepairError> {
        self.manifest.stage = Stage::Terminal;
        self.manifest.outcome = Some(outcome);
        self.manifest.reason = Some(reason.chars().take(300).collect());
        self.persist()?;
        self.event(Stage::Terminal, "attempt_finished", &format!("{outcome:?}"))
    }

    pub fn fail_before_start(&mut self, outcome: Outcome, reason: &str) -> Result<(), RepairError> {
        self.finish(outcome, reason)
    }

    fn freeze_contract(&mut self, contract: &RepairContract) -> Result<(), RepairError> {
        contract.validate()?;
        let bytes = serde_json::to_vec_pretty(contract)?;
        let path = self.dir.join("protected/contract.json");
        fs::rename(&path, self.dir.join("protected/contract-initial.json"))?;
        fs::write(&path, &bytes)?;
        self.manifest.contract_sha256 = hash(bytes);
        self.persist()?;
        self.event(
            Stage::Investigation,
            "reduction_frozen",
            "trusted SQL recomputation",
        )
    }

    pub fn record_cleanup_error(&mut self, reason: &str) -> Result<(), RepairError> {
        self.manifest.cleanup_error = Some(reason.chars().take(300).collect());
        self.persist()?;
        self.event(
            Stage::Terminal,
            "cleanup_failed",
            "isolated checkout cleanup failed",
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildIdentity {
    pub build_id: String,
    pub revision: String,
    pub tree_sha256: String,
    pub toolchain: String,
    pub binary_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub build_id: String,
    pub result_id: String,
    pub fixture_sha256: String,
    pub prompt_sha256: String,
    pub source_rows: Option<Vec<u64>>,
    pub plan: PlanComparison,
}

impl Observation {
    pub fn matches(&self, build: &BuildIdentity, case: &FrozenCase) -> bool {
        self.build_id == build.build_id
            && self.fixture_sha256 == case.fixture_sha256
            && self.prompt_sha256 == case.prompt_sha256
            && self.source_rows.as_deref() == Some(case.expected_source_rows.as_slice())
            && self.plan == PlanComparison::Equivalent
            && !self.result_id.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteId {
    ProtectedCompatibility,
    AdditiveTests,
    OwningCrate,
    WorkspaceFormat,
    WorkspaceCheck,
    WorkspaceTests,
}

impl SuiteId {
    pub const ALL: [Self; 6] = [
        Self::ProtectedCompatibility,
        Self::AdditiveTests,
        Self::OwningCrate,
        Self::WorkspaceFormat,
        Self::WorkspaceCheck,
        Self::WorkspaceTests,
    ];
    pub fn command(self) -> &'static [&'static str] {
        match self {
            Self::ProtectedCompatibility => &[
                "cargo",
                "test",
                "-p",
                "baho-core",
                "--test",
                "repair_regression",
            ],
            Self::AdditiveTests => &["cargo", "test", "-p", "baho-core", "--tests"],
            Self::OwningCrate => &["cargo", "test", "-p", "baho-core"],
            Self::WorkspaceFormat => &["cargo", "fmt", "--check"],
            Self::WorkspaceCheck => &["cargo", "check", "--workspace"],
            Self::WorkspaceTests => &["cargo", "test", "--workspace"],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub suite: SuiteId,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_excerpt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchFile {
    pub path: PathBuf,
    pub baseline_sha256: Option<String>,
    pub content: String,
}

impl CheckResult {
    pub fn passed(&self, expected: SuiteId) -> bool {
        self.suite == expected
            && self.command == expected.command()
            && self.exit_code == Some(0)
            && !self.timed_out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Investigation {
    pub result_ids: Vec<String>,
    pub causal_hypothesis: String,
    pub mechanism: String,
    pub allowed_files: Vec<String>,
    pub compatibility_checks: Vec<String>,
    /// The investigator proposes only CSV, prompt, and supported SQL. The
    /// trusted harness computes expected rows before freezing the contract.
    pub reduction: Option<SyntheticReduction>,
}

pub trait Agents {
    fn failure_reason(&self) -> Option<&str> {
        None
    }
    /// Redacted open questions from a stage that declared insufficient evidence.
    fn open_questions(&self) -> &[String] {
        &[]
    }
    fn investigate(&mut self, target: &Observation) -> Result<Investigation, Outcome>;
    fn code(&mut self, regression: &Path, checkout: &Path) -> Result<(), Outcome>;
    fn review(
        &mut self,
        patch_sha256: &str,
        patch: &[PatchFile],
        evidence: &[Observation],
    ) -> Result<bool, Outcome>;
}

/// Implementations must build/run only in a confined worker with no credentials,
/// network, or access to the user's checkout. No fallback worker is supplied.
pub trait ConfinedWorker {
    fn baseline(&mut self, revision: &str) -> Result<BuildIdentity, Outcome>;
    fn checkout(&self) -> &Path;
    fn probe(&mut self, build: &BuildIdentity, case: &FrozenCase) -> Result<Observation, Outcome>;
    fn baseline_checks(&mut self, build: &BuildIdentity) -> Result<(), Outcome>;
    fn register_reduction(
        &mut self,
        _case: &FrozenCase,
        _reduction: &SyntheticReduction,
    ) -> Result<(), Outcome> {
        Ok(())
    }
    /// Compile and run the protected generated test on the baseline. Only an
    /// assertion failure establishes the intended semantic mismatch.
    fn regression_baseline(
        &mut self,
        build: &BuildIdentity,
        regression: &Path,
    ) -> Result<bool, Outcome>;
    fn candidate(
        &mut self,
        patch_sha256: &str,
        regression: &Path,
    ) -> Result<BuildIdentity, Outcome>;
    fn check(&mut self, build: &BuildIdentity, suite: SuiteId) -> Result<CheckResult, Outcome>;
}

/// The regression is generated and kept in the protected attempt directory. The
/// worker injects it into each fresh build; a coder checkout never receives it.
pub fn emit_regression(
    dir: &Path,
    case: &FrozenCase,
    reduction: &SyntheticReduction,
) -> Result<PathBuf, RepairError> {
    let path = dir.join("repair_regression.rs");
    let expected = case
        .expected_source_rows
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!(
        r#"// Generated from a frozen synthetic Role equality contract.
use std::io::Write;
use baho_core::{{CoreOutcome, run_pipeline}};
use baho_plan::plan::{{ComparisonOperator, Expression, Literal, PlanStep}};

#[test]
fn frozen_role_equality_regression() {{
    let mut source = tempfile::NamedTempFile::new().unwrap();
    source.write_all({:?}.as_bytes()).unwrap();
    let result = run_pipeline(source.path(), {:?});
    assert_eq!(result.outcome, CoreOutcome::Materialized);
    assert_eq!(result.output.as_ref().unwrap().provenance.iter()
        .map(|row| row.source_row).collect::<Vec<_>>(), [{expected}]);
    assert_eq!(result.plan.as_ref().unwrap().steps,
        vec![PlanStep::Filter {{ predicate: Expression::Compare {{
            column: {:?}.into(), operator: ComparisonOperator::Equal,
            literal: Literal::Text({:?}.into()),
        }} }}]);
}}
"#,
        reduction.csv,
        reduction.prompt,
        format!(
            "column-{}",
            crate::probe::validate_synthetic(reduction.csv.as_bytes())
                .map_err(|_| RepairError::InvalidContract)?[0]
                .cells
                .iter()
                .position(|h| h == "Role")
                .ok_or(RepairError::InvalidContract)?
        ),
        case.expected_literal
    );
    fs::write(&path, source)?;
    Ok(path)
}

/// Snapshot the candidate's file tree before and after coding. Symlinks and
/// nonregular files fail closed, including symlinks in otherwise allowed paths.
pub fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, String>, RepairError> {
    fn visit(
        root: &Path,
        relative: &Path,
        out: &mut BTreeMap<PathBuf, String>,
    ) -> Result<(), RepairError> {
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let name = entry.file_name();
            let path = relative.join(name);
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(RepairError::DeniedEdit(path.display().to_string()));
            }
            if kind.is_dir() {
                visit(root, &path, out)?;
            } else if kind.is_file() {
                if entry.metadata()?.len() > MAX_TREE_FILE_BYTES {
                    return Err(RepairError::DeniedEdit(path.display().to_string()));
                }
                let bytes = fs::read(entry.path())?;
                out.insert(path, hash(bytes));
            } else {
                return Err(RepairError::DeniedEdit(path.display().to_string()));
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    visit(root, Path::new(""), &mut result)?;
    Ok(result)
}

fn allowed(path: &Path, was_present: bool) -> bool {
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return false;
    }
    let text = path.to_string_lossy();
    text.starts_with("crates/baho-core/src/")
        || (!was_present && text.starts_with("crates/baho-core/tests/") && text.ends_with(".rs"))
}

pub fn check_edits(
    before: &BTreeMap<PathBuf, String>,
    after: &BTreeMap<PathBuf, String>,
    root: &Path,
) -> Result<(String, Vec<PatchFile>), RepairError> {
    let mut patch = Vec::new();
    for (path, old) in before {
        match after.get(path) {
            None => {
                return Err(RepairError::DeniedEdit(format!(
                    "deleted {}",
                    path.display()
                )));
            }
            Some(new) if new != old => {
                if !allowed(path, true) {
                    return Err(RepairError::DeniedEdit(path.display().to_string()));
                }
                let content = fs::read_to_string(root.join(path))
                    .map_err(|_| RepairError::DeniedEdit(path.display().to_string()))?;
                if hash(content.as_bytes()) != *new {
                    return Err(RepairError::DeniedEdit("changed during snapshot".into()));
                }
                patch.push(PatchFile {
                    path: path.clone(),
                    baseline_sha256: Some(old.clone()),
                    content,
                });
            }
            _ => {}
        }
    }
    for (path, new) in after {
        if !before.contains_key(path) {
            if !allowed(path, false) {
                return Err(RepairError::DeniedEdit(path.display().to_string()));
            }
            let content = fs::read_to_string(root.join(path))
                .map_err(|_| RepairError::DeniedEdit(path.display().to_string()))?;
            if hash(content.as_bytes()) != *new {
                return Err(RepairError::DeniedEdit("changed during snapshot".into()));
            }
            patch.push(PatchFile {
                path: path.clone(),
                baseline_sha256: None,
                content,
            });
        }
    }
    let bytes = serde_json::to_vec(&patch)?;
    if patch.is_empty() || bytes.len() > MAX_PATCH_BYTES {
        return Err(RepairError::DeniedEdit("empty or oversized patch".into()));
    }
    Ok((hash(bytes), patch))
}

pub fn run_attempt(
    record: &mut AttemptRecord,
    contract: &RepairContract,
    worker: &mut impl ConfinedWorker,
    agents: &mut impl Agents,
) -> Result<Outcome, RepairError> {
    let mut frozen = contract.clone();
    let mut run = || -> Result<Outcome, RepairError> {
        record.advance(Stage::Baseline, "baseline_started")?;
        let baseline = match worker.baseline(&contract.baseline_revision) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        if baseline.revision != contract.baseline_revision
            || baseline.build_id.is_empty()
            || baseline.binary_sha256.is_empty()
        {
            return Ok(Outcome::EnvironmentFailure);
        }
        record.manifest.baseline_build_id = Some(baseline.build_id.clone());
        record.persist()?;
        fs::write(
            record.dir.join("protected/baseline-build.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "build_id": baseline.build_id, "revision": baseline.revision, "tree_sha256": baseline.tree_sha256,
                "toolchain": baseline.toolchain, "binary_sha256": baseline.binary_sha256,
            }))?,
        )?;
        if let Err(o) = worker.baseline_checks(&baseline) {
            return Ok(o);
        }
        let target_before = match worker.probe(&baseline, &contract.target) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        if target_before.matches(&baseline, &contract.target) {
            return Ok(Outcome::NoChange);
        }
        if target_before.build_id != baseline.build_id {
            return Ok(Outcome::EnvironmentFailure);
        }
        record.advance(Stage::Investigation, "investigation_started")?;
        let handoff = match agents.investigate(&target_before) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        if !handoff.result_ids.contains(&target_before.result_id)
            || handoff.mechanism.is_empty()
            || handoff.causal_hypothesis.is_empty()
        {
            return Ok(Outcome::NeedsDesign);
        }
        if let Some(reduction) = handoff.reduction {
            let reduced = match freeze_synthetic(&reduction) {
                Ok(value) => value,
                Err(outcome) => return Ok(outcome),
            };
            frozen.reduced = reduced;
            frozen.reduction = reduction;
            if frozen.validate().is_err() {
                return Ok(Outcome::NeedsDesign);
            }
            if let Err(outcome) = worker.register_reduction(&frozen.reduced, &frozen.reduction) {
                return Ok(outcome);
            }
            record.freeze_contract(&frozen)?;
        }
        record.advance(Stage::Regression, "regression_frozen")?;
        let regression = emit_regression(
            &record.dir.join("protected"),
            &frozen.reduced,
            &frozen.reduction,
        )?;
        let reduced_before = match worker.probe(&baseline, &frozen.reduced) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        if reduced_before.matches(&baseline, &contract.reduced)
            || reduced_before.build_id != baseline.build_id
        {
            return Ok(Outcome::RegressionFailure);
        }
        match worker.regression_baseline(&baseline, &regression) {
            Ok(true) => {}
            Ok(false) => return Ok(Outcome::RegressionFailure),
            Err(o) => return Ok(o),
        }
        let mut protected_before = Vec::new();
        for case in &contract.protected_cases {
            let observation = match worker.probe(&baseline, case) {
                Ok(v) => v,
                Err(o) => return Ok(o),
            };
            if observation.build_id != baseline.build_id {
                return Ok(Outcome::EnvironmentFailure);
            }
            protected_before.push(observation);
        }
        record.advance(Stage::Coding, "coding_started")?;
        let before = match snapshot(worker.checkout()) {
            Ok(v) => v,
            Err(_) => return Ok(Outcome::EnvironmentFailure),
        };
        if let Err(o) = agents.code(&regression, worker.checkout()) {
            return Ok(o);
        }
        let after = match snapshot(worker.checkout()) {
            Ok(v) => v,
            Err(_) => return Ok(Outcome::Rejected),
        };
        let (patch_hash, patch) = match check_edits(&before, &after, worker.checkout()) {
            Ok(v) => v,
            Err(_) => return Ok(Outcome::Rejected),
        };
        fs::write(
            record.dir.join("protected/patch.json"),
            serde_json::to_vec_pretty(&patch)?,
        )?;
        record.manifest.patch_sha256 = Some(patch_hash.clone());
        record.persist()?;
        record.advance(Stage::Check, "checks_started")?;
        let candidate = match worker.candidate(&patch_hash, &regression) {
            Ok(v) => v,
            Err(o) => return Ok(o),
        };
        if candidate.build_id == baseline.build_id
            || candidate.binary_sha256 == baseline.binary_sha256
            || candidate.revision != baseline.revision
        {
            return Ok(Outcome::BuildFailure);
        }
        record.manifest.candidate_build_id = Some(candidate.build_id.clone());
        record.persist()?;
        fs::write(
            record.dir.join("protected/candidate-build.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "build_id": candidate.build_id, "revision": candidate.revision, "tree_sha256": candidate.tree_sha256,
                "toolchain": candidate.toolchain, "binary_sha256": candidate.binary_sha256,
                "patch_sha256": patch_hash,
            }))?,
        )?;
        let mut evidence = vec![target_before, reduced_before];
        for case in std::iter::once(&frozen.target).chain(std::iter::once(&frozen.reduced)) {
            let observation = match worker.probe(&candidate, case) {
                Ok(v) => v,
                Err(o) => return Ok(o),
            };
            if observation.plan == PlanComparison::Unavailable {
                return Ok(Outcome::UnavailableComparison);
            }
            if !observation.matches(&candidate, case) {
                return Ok(Outcome::CheckFailure);
            }
            evidence.push(observation);
        }
        for (case, baseline_observation) in
            contract.protected_cases.iter().zip(protected_before.iter())
        {
            let observation = match worker.probe(&candidate, case) {
                Ok(v) => v,
                Err(o) => return Ok(o),
            };
            let preserved = observation.build_id == candidate.build_id
                && observation.source_rows == baseline_observation.source_rows
                && observation.plan == baseline_observation.plan;
            if !observation.matches(&candidate, case) && !preserved {
                return Ok(Outcome::CheckFailure);
            }
            evidence.push(observation);
        }
        for suite in SuiteId::ALL {
            let result = match worker.check(&candidate, suite) {
                Ok(v) => v,
                Err(o) => return Ok(o),
            };
            fs::write(
                record
                    .dir
                    .join("protected")
                    .join(format!("check-{suite:?}.json")),
                serde_json::to_vec_pretty(&result)?,
            )?;
            if !result.passed(suite) {
                return Ok(Outcome::CheckFailure);
            }
        }
        record.advance(Stage::Review, "review_started")?;
        match agents.review(&patch_hash, &patch, &evidence) {
            Ok(true) => Ok(Outcome::Accepted),
            Ok(false) => Ok(Outcome::Rejected),
            Err(o) => Ok(o),
        }
    };
    let outcome = run()?;
    let reason = agents
        .failure_reason()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{:?}: {outcome:?}", record.manifest.stage));
    record.manifest.open_questions = agents.open_questions().to_vec();
    record.finish(outcome, &reason)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn case(csv: &str, prompt: &str, sql: &str, rows: &[u64], literal: &str) -> FrozenCase {
        FrozenCase {
            fixture_sha256: hash(csv),
            prompt_sha256: hash(prompt),
            sql_sha256: hash(sql),
            expected_source_rows: rows.into(),
            expected_literal: literal.into(),
        }
    }

    fn contract() -> RepairContract {
        let csv = "Id,Role\n1,Analyst\n2,Engineer\n";
        let prompt = "Show rows where Role = Analyst";
        let sql = "SELECT _row FROM data WHERE lower(\"Role\") = lower('Analyst') ORDER BY _row";
        let reduced = case(csv, prompt, sql, &[1], "Analyst");
        RepairContract {
            schema_version: ATTEMPT_SCHEMA,
            settings: AttemptSettings {
                investigator_prompt_version: "v1".into(),
                coder_prompt_version: "v1".into(),
                reviewer_prompt_version: "v1".into(),
                model: "stub".into(),
                max_agent_turns: 6,
                max_tool_calls: 20,
                process_timeout_seconds: 30,
                max_output_bytes: 64 * 1024,
                max_read_bytes: 1024,
                max_outline_bytes: 6 * 1024,
                max_search_matches: 40,
                max_exposed_bytes: 48 * 1024,
            },
            baseline_revision: "a".repeat(40),
            evaluation_sha256: "b".repeat(64),
            generation_sha256: "c".repeat(64),
            review_sha256: "d".repeat(64),
            oracle_sha256: "e".repeat(64),
            target_case_id: "case-1".into(),
            target: reduced.clone(),
            reduced,
            reduction: SyntheticReduction {
                csv: csv.into(),
                prompt: prompt.into(),
                sql: sql.into(),
            },
            protected_cases: vec![],
            parent_attempt_id: None,
        }
    }

    struct StubWorker {
        checkout: TempDir,
        log: Vec<String>,
        mode: &'static str,
    }
    impl StubWorker {
        fn new(mode: &'static str) -> Self {
            let checkout = tempfile::tempdir().unwrap();
            fs::create_dir_all(checkout.path().join("crates/baho-core/src")).unwrap();
            fs::write(
                checkout.path().join("crates/baho-core/src/intent.rs"),
                "original",
            )
            .unwrap();
            Self {
                checkout,
                log: vec![],
                mode,
            }
        }
        fn build(id: &str, revision: &str) -> BuildIdentity {
            BuildIdentity {
                build_id: id.into(),
                revision: revision.into(),
                tree_sha256: hash(id),
                toolchain: "stub-rustc".into(),
                binary_sha256: hash(format!("bin-{id}")),
            }
        }
    }
    impl ConfinedWorker for StubWorker {
        fn baseline(&mut self, revision: &str) -> Result<BuildIdentity, Outcome> {
            self.log.push("baseline".into());
            if self.mode == "build_fail" {
                return Err(Outcome::BuildFailure);
            }
            Ok(Self::build("base", revision))
        }
        fn checkout(&self) -> &Path {
            self.checkout.path()
        }
        fn probe(
            &mut self,
            build: &BuildIdentity,
            case: &FrozenCase,
        ) -> Result<Observation, Outcome> {
            self.log.push(format!("probe-{}", build.build_id));
            if self.mode == "timeout" {
                return Err(Outcome::ProcessTimeout);
            }
            let candidate = build.build_id == "candidate";
            let rows = if candidate {
                case.expected_source_rows.clone()
            } else {
                vec![2]
            };
            let rows = if candidate
                && self.mode == "protected_regress"
                && case.expected_source_rows == [2]
            {
                vec![1]
            } else {
                rows
            };
            let rows = if candidate && self.mode == "duplicate_rows" {
                vec![1, 1]
            } else {
                rows
            };
            Ok(Observation {
                build_id: if self.mode == "stale_build" && candidate {
                    "base".into()
                } else {
                    build.build_id.clone()
                },
                result_id: hash(format!("{}{}", build.build_id, case.fixture_sha256)),
                fixture_sha256: case.fixture_sha256.clone(),
                prompt_sha256: case.prompt_sha256.clone(),
                source_rows: Some(rows),
                plan: if candidate && self.mode == "wrong_plan" {
                    PlanComparison::Different
                } else if candidate && self.mode == "unavailable" {
                    PlanComparison::Unavailable
                } else {
                    PlanComparison::Equivalent
                },
            })
        }
        fn baseline_checks(&mut self, _: &BuildIdentity) -> Result<(), Outcome> {
            self.log.push("baseline-checks".into());
            if self.mode == "baseline_fail" {
                Err(Outcome::BaselineFailure)
            } else {
                Ok(())
            }
        }
        fn regression_baseline(
            &mut self,
            _: &BuildIdentity,
            regression: &Path,
        ) -> Result<bool, Outcome> {
            self.log.push("regression-baseline".into());
            assert!(regression.exists());
            if self.mode == "regression_compile_fail" {
                return Err(Outcome::RegressionFailure);
            }
            Ok(self.mode != "regression_passes")
        }
        fn candidate(&mut self, _: &str, regression: &Path) -> Result<BuildIdentity, Outcome> {
            self.log.push("candidate".into());
            assert!(regression.exists());
            if self.mode == "candidate_fail" {
                return Err(Outcome::BuildFailure);
            }
            Ok(Self::build("candidate", &"a".repeat(40)))
        }
        fn check(&mut self, _: &BuildIdentity, suite: SuiteId) -> Result<CheckResult, Outcome> {
            self.log.push(format!("check-{suite:?}"));
            Ok(CheckResult {
                suite,
                command: suite.command().iter().map(|s| s.to_string()).collect(),
                exit_code: Some(if self.mode == "check_fail" { 1 } else { 0 }),
                timed_out: false,
                output_excerpt: String::new(),
            })
        }
    }

    struct StubAgents {
        mode: &'static str,
        log: Vec<String>,
        questions: Vec<String>,
    }
    impl Agents for StubAgents {
        fn failure_reason(&self) -> Option<&str> {
            (self.mode == "exhausted").then_some("investigation: session_timeout (limit=120)")
        }
        fn open_questions(&self) -> &[String] {
            if self.mode == "exhausted" {
                &self.questions
            } else {
                &[]
            }
        }
        fn investigate(&mut self, observation: &Observation) -> Result<Investigation, Outcome> {
            self.log.push("investigate".into());
            if self.mode == "exhausted" {
                return Err(Outcome::ModelExhausted);
            }
            Ok(Investigation {
                result_ids: vec![observation.result_id.clone()],
                causal_hypothesis: "word operator missed".into(),
                mechanism: "recognition".into(),
                allowed_files: vec!["crates/baho-core/src/intent.rs".into()],
                compatibility_checks: vec!["existing text equality".into()],
                reduction: None,
            })
        }
        fn code(&mut self, regression: &Path, checkout: &Path) -> Result<(), Outcome> {
            self.log.push("code".into());
            assert!(regression.exists());
            fs::write(checkout.join("crates/baho-core/src/intent.rs"), "repaired").unwrap();
            Ok(())
        }
        fn review(
            &mut self,
            patch_sha256: &str,
            patch: &[PatchFile],
            _: &[Observation],
        ) -> Result<bool, Outcome> {
            self.log.push("review".into());
            assert_eq!(hash(serde_json::to_vec(patch).unwrap()), patch_sha256);
            assert_eq!(patch.len(), 1);
            assert_eq!(patch[0].path, Path::new("crates/baho-core/src/intent.rs"));
            assert_eq!(patch[0].content, "repaired");
            Ok(self.mode != "reject")
        }
    }

    fn attempt(
        worker_mode: &'static str,
        agent_mode: &'static str,
    ) -> (Outcome, AttemptRecord, StubWorker, StubAgents, TempDir) {
        let root = tempfile::tempdir().unwrap();
        let contract = contract();
        let mut record = AttemptRecord::reserve(root.path(), &contract).unwrap();
        let mut worker = StubWorker::new(worker_mode);
        let mut agents = StubAgents {
            mode: agent_mode,
            log: vec![],
            questions: vec!["which operator path?".into()],
        };
        let result = run_attempt(&mut record, &contract, &mut worker, &mut agents).unwrap();
        assert_eq!(record.manifest.outcome, Some(result));
        (result, record, worker, agents, root)
    }

    #[test]
    fn successful_stub_attempt_runs_ordered_gates() {
        let (outcome, mut record, worker, agents, _root) = attempt("ok", "ok");
        assert_eq!(outcome, Outcome::Accepted);
        assert_eq!(
            worker.log[0..3],
            ["baseline", "baseline-checks", "probe-base"]
        );
        assert_eq!(agents.log, ["investigate", "code", "review"]);
        assert!(
            worker.log.iter().position(|s| s == "candidate").unwrap()
                < worker
                    .log
                    .iter()
                    .position(|s| s == "check-WorkspaceTests")
                    .unwrap()
        );
        assert!(record.manifest.patch_sha256.is_some());
        record.record_cleanup_error("checkout busy").unwrap();
        assert_eq!(record.manifest.outcome, Some(Outcome::Accepted));
        let saved =
            AttemptManifest::parse(&fs::read(record.dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(saved.cleanup_error.as_deref(), Some("checkout busy"));
        assert_eq!(saved.outcome, Some(Outcome::Accepted));
    }

    #[test]
    fn failures_cannot_accept() {
        for (worker_mode, agent_mode, expected) in [
            ("build_fail", "ok", Outcome::BuildFailure),
            ("baseline_fail", "ok", Outcome::BaselineFailure),
            ("regression_compile_fail", "ok", Outcome::RegressionFailure),
            ("regression_passes", "ok", Outcome::RegressionFailure),
            ("timeout", "ok", Outcome::ProcessTimeout),
            ("candidate_fail", "ok", Outcome::BuildFailure),
            ("wrong_plan", "ok", Outcome::CheckFailure),
            ("duplicate_rows", "ok", Outcome::CheckFailure),
            ("stale_build", "ok", Outcome::CheckFailure),
            ("unavailable", "ok", Outcome::UnavailableComparison),
            ("check_fail", "ok", Outcome::CheckFailure),
            ("ok", "reject", Outcome::Rejected),
            ("ok", "exhausted", Outcome::ModelExhausted),
        ] {
            let (outcome, record, _, _, _root) = attempt(worker_mode, agent_mode);
            assert_eq!(outcome, expected, "{worker_mode}/{agent_mode}");
            assert_eq!(record.manifest.stage, Stage::Terminal);
        }
    }

    #[test]
    fn agent_failure_reason_survives_terminal_manifest() {
        let (_, record, _, _, _root) = attempt("ok", "exhausted");
        let manifest =
            AttemptManifest::parse(&fs::read(record.dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest.reason.as_deref(),
            Some("investigation: session_timeout (limit=120)")
        );
        assert_eq!(manifest.open_questions, ["which operator path?"]);
    }

    #[test]
    fn protected_case_regression_rejects_candidate() {
        let root = tempfile::tempdir().unwrap();
        let mut contract = contract();
        contract.protected_cases.push(case(
            "Id,Role\n1,Analyst\n2,Engineer\n",
            "Show rows where Role = Engineer",
            "SELECT _row FROM data WHERE lower(\"Role\") = lower('Engineer') ORDER BY _row",
            &[2],
            "Engineer",
        ));
        let mut record = AttemptRecord::reserve(root.path(), &contract).unwrap();
        let mut worker = StubWorker::new("protected_regress");
        let mut agents = StubAgents {
            mode: "ok",
            log: vec![],
            questions: vec!["which operator path?".into()],
        };
        assert_eq!(
            run_attempt(&mut record, &contract, &mut worker, &mut agents).unwrap(),
            Outcome::CheckFailure
        );
        assert!(!agents.log.contains(&"review".to_string()));
    }

    #[test]
    fn contracts_reject_unknown_versions_and_tampered_reduction() {
        let mut value = contract();
        value.schema_version = 99;
        assert!(matches!(
            RepairContract::parse(&serde_json::to_vec(&value).unwrap()),
            Err(RepairError::UnknownSchema(99))
        ));
        let mut value = contract();
        value.reduction.csv.push('x');
        assert!(value.validate().is_err());
        let mut manifest = AttemptManifest {
            schema_version: 99,
            attempt_id: "x".into(),
            stage: Stage::Reserved,
            outcome: None,
            contract_sha256: String::new(),
            baseline_build_id: None,
            candidate_build_id: None,
            patch_sha256: None,
            reason: None,
            cleanup_error: None,
            open_questions: Vec::new(),
        };
        assert!(matches!(
            AttemptManifest::parse(&serde_json::to_vec(&manifest).unwrap()),
            Err(RepairError::UnknownSchema(99))
        ));
        manifest.schema_version = ATTEMPT_SCHEMA;
        assert!(AttemptManifest::parse(&serde_json::to_vec(&manifest).unwrap()).is_ok());
    }

    #[test]
    fn regression_is_generated_rust_with_ordered_rows_and_predicate() {
        let temp = tempfile::tempdir().unwrap();
        let contract = contract();
        let path = emit_regression(temp.path(), &contract.reduced, &contract.reduction).unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("[1]"));
        assert!(text.contains("PlanStep::Filter"));
        assert!(text.contains("column-1"));
        assert!(text.contains("Analyst"));
    }

    #[test]
    fn edit_guard_rejects_denied_paths_deletion_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("crates/baho-core/src")).unwrap();
        fs::create_dir_all(temp.path().join("crates/baho-core/tests")).unwrap();
        let source = temp.path().join("crates/baho-core/src/intent.rs");
        fs::write(&source, "old").unwrap();
        let before = snapshot(temp.path()).unwrap();
        fs::write(&source, "new").unwrap();
        let after = snapshot(temp.path()).unwrap();
        let (digest, patch) = check_edits(&before, &after, temp.path()).unwrap();
        assert_eq!(digest.len(), 64);
        assert_eq!(patch[0].content, "new");
        fs::write(temp.path().join("Cargo.toml"), "bad").unwrap();
        assert!(check_edits(&before, &snapshot(temp.path()).unwrap(), temp.path()).is_err());
        fs::remove_file(temp.path().join("Cargo.toml")).unwrap();
        fs::remove_file(&source).unwrap();
        assert!(check_edits(&before, &snapshot(temp.path()).unwrap(), temp.path()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", &source).unwrap();
            assert!(snapshot(temp.path()).is_err());
        }
    }
}
