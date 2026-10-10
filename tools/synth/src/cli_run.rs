//! Bounded execution of an externally built `baho-cli` and parsing of its run artifacts
//! (Epic 016, T2). The binary is only ever run as a subprocess; no Baho crate is linked.
//!
//! Unknown artifact schema versions are rejected rather than guessed at, and row sets
//! returned here live in memory only: callers persist counts, never row lists.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::contracts::{PlanEvidence, PlanLiteral, PlanOperator, TextMatchPolicy};

pub const MANIFEST_SCHEMA: u32 = 2;
pub const DIAGNOSTICS_SCHEMA: u32 = 1;
pub const PLAN_SCHEMA: u32 = 5;
pub const RESULT_SCHEMA: u32 = 2;
const MAX_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MESSAGE_CHARS: usize = 300;
const MAX_DIAGNOSTICS: usize = 50;

#[derive(Debug, Error)]
pub enum CliRunError {
    #[error("I/O error on {what}: {source}")]
    Io {
        what: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("baho-cli binary not usable: {0}")]
    Binary(String),
    #[error("derived CSV rejected: {0}")]
    Derive(String),
    #[error("run artifact {file}: {reason}")]
    Artifact { file: &'static str, reason: String },
    #[error("git state unavailable: {0}")]
    Git(String),
}

fn io(what: &'static str) -> impl FnOnce(std::io::Error) -> CliRunError {
    move |source| CliRunError::Io { what, source }
}

// ---------------------------------------------------------------------------
// Role-only derived CSV
// ---------------------------------------------------------------------------

/// A one-column CSV (`Role`) built from the source, so no other column reaches the CLI's
/// run artifacts. Record `i` (data rows, 1-based) of the derived file is source record `i`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedCsv {
    pub bytes: Vec<u8>,
    /// `source_rows[derived_row - 1]` is the source data row (1-based, header excluded).
    pub source_rows: Vec<u64>,
}

impl DerivedCsv {
    pub fn row_count(&self) -> usize {
        self.source_rows.len()
    }

    /// Maps a derived data row to its source data row.
    pub fn source_row(&self, derived_row: u64) -> Option<u64> {
        let index = usize::try_from(derived_row.checked_sub(1)?).ok()?;
        self.source_rows.get(index).copied()
    }
}

/// Projects the `Role` column of `source`, preserving logical-record order, and verifies
/// by re-reading that the projection holds exactly the same values in the same order.
pub fn derive_role_csv(source: &[u8]) -> Result<DerivedCsv, CliRunError> {
    let bad = |e: csv::Error| CliRunError::Derive(e.to_string());
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(source);
    let mut records = reader.records();
    let header = records
        .next()
        .ok_or_else(|| CliRunError::Derive("missing header".into()))?
        .map_err(bad)?;
    let role = header
        .iter()
        .position(|name| name == "Role")
        .ok_or_else(|| CliRunError::Derive("missing Role column".into()))?;

    let mut values = Vec::new();
    for record in records {
        let record = record.map_err(bad)?;
        let value = record
            .get(role)
            .ok_or_else(|| CliRunError::Derive("short record".into()))?;
        values.push(value.to_owned());
    }

    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::new());
    writer.write_record(["Role"]).map_err(bad)?;
    for value in &values {
        writer.write_record([value.as_str()]).map_err(bad)?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|e| CliRunError::Derive(e.to_string()))?;

    // Row mapping check: the derived file parses back to the same ordered values.
    let mut check = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(bytes.as_slice());
    let mut count = 0usize;
    for (index, record) in check.records().enumerate() {
        let record = record.map_err(bad)?;
        if record.len() != 1 || record.get(0) != values.get(index).map(String::as_str) {
            return Err(CliRunError::Derive("row mapping mismatch".into()));
        }
        count += 1;
    }
    if count != values.len() {
        return Err(CliRunError::Derive("row count mismatch".into()));
    }
    let source_rows = (1..=values.len() as u64).collect();
    Ok(DerivedCsv { bytes, source_rows })
}

// ---------------------------------------------------------------------------
// Binary identity and repository state
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliBinary {
    pub path: PathBuf,
    pub sha256: String,
    pub version: String,
}

impl CliBinary {
    /// Hashes the binary and requires `--version` to report `baho <version>` and
    /// `--help` to name the `run` subcommand.
    ///
    /// The stored path is canonical (absolute): cases run with a different working
    /// directory, where a relative path would no longer resolve.
    pub fn probe(path: &Path) -> Result<Self, CliRunError> {
        let path = &fs::canonicalize(path)
            .map_err(|e| CliRunError::Binary(format!("cannot resolve {}: {e}", path.display())))?;
        let bytes = fs::read(path)
            .map_err(|e| CliRunError::Binary(format!("cannot read {}: {e}", path.display())))?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let version_out = probe_output(path, "--version")?;
        let version = version_out
            .trim()
            .strip_prefix("baho ")
            .filter(|v| !v.is_empty() && v.len() <= 64 && !v.contains('\n'))
            .ok_or_else(|| CliRunError::Binary("unrecognized --version output".into()))?
            .to_owned();
        let help = probe_output(path, "--help")?;
        if !help.lines().any(|l| l.trim_start().starts_with("run ")) {
            return Err(CliRunError::Binary("unrecognized --help output".into()));
        }
        Ok(Self {
            path: path.to_owned(),
            sha256,
            version,
        })
    }
}

fn probe_output(path: &Path, flag: &str) -> Result<String, CliRunError> {
    let outcome = run_bounded(
        Command::new(path).arg(flag),
        Duration::from_secs(10),
        64 * 1024,
    )
    .map_err(|e| CliRunError::Binary(format!("cannot execute: {e}")))?;
    if outcome.timed_out || outcome.exit_code != Some(0) {
        return Err(CliRunError::Binary(format!("{flag} did not succeed")));
    }
    Ok(String::from_utf8_lossy(&outcome.stdout).into_owned())
}

/// Stable, bounded reason code for a failure to start the CLI process; never carries
/// OS error text or paths.
pub fn spawn_failure_code(error: &CliRunError) -> &'static str {
    match error {
        CliRunError::Io { source, .. } => match source.kind() {
            std::io::ErrorKind::NotFound => "spawn_not_found",
            std::io::ErrorKind::PermissionDenied => "spawn_permission_denied",
            _ => "spawn_failed",
        },
        _ => "spawn_failed",
    }
}

/// The revision does not prove the binary was built from it; it is context only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoState {
    pub revision: String,
    pub dirty: bool,
}

impl RepoState {
    pub fn capture(repo: &Path) -> Result<Self, CliRunError> {
        let git = |args: &[&str]| -> Result<String, CliRunError> {
            let out = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .map_err(|e| CliRunError::Git(e.to_string()))?;
            if !out.status.success() {
                return Err(CliRunError::Git(format!("git {} failed", args[0])));
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let revision = git(&["rev-parse", "HEAD"])?.trim().to_owned();
        let dirty = !git(&["status", "--porcelain"])?.trim().is_empty();
        Ok(Self { revision, dirty })
    }
}

// ---------------------------------------------------------------------------
// Bounded subprocess execution
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct RunLimits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_output_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug)]
struct Bounded {
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: Vec<u8>,
    stdout_total: u64,
    stderr_total: u64,
}

fn drain(mut stream: impl Read + Send + 'static, cap: usize) -> thread::JoinHandle<(Vec<u8>, u64)> {
    thread::spawn(move || {
        let (mut kept, mut total) = (Vec::new(), 0u64);
        let mut buf = [0u8; 8192];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n as u64;
            let room = cap.saturating_sub(kept.len());
            kept.extend_from_slice(&buf[..n.min(room)]);
        }
        (kept, total)
    })
}

fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    cap: usize,
) -> Result<Bounded, std::io::Error> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = drain(child.stdout.take().expect("piped"), cap);
    let err = drain(child.stderr.take().expect("piped"), cap);
    let start = Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status), false);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            break (None, true);
        }
        thread::sleep(Duration::from_millis(5));
    };
    let (stdout, stdout_total) = out.join().unwrap_or_default();
    let (_, stderr_total) = err.join().unwrap_or_default();
    Ok(Bounded {
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        stdout,
        stdout_total,
        stderr_total,
    })
}

/// Creates a new, empty, uniquely named evaluation directory under `root`. Existing
/// directories are never reused.
pub fn create_evaluation_dir(root: &Path) -> Result<PathBuf, CliRunError> {
    fs::create_dir_all(root).map_err(io("evaluation root"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for attempt in 0..100u32 {
        let dir = root.join(format!(
            "evaluation-{nanos}-{}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io("evaluation directory")(e)),
        }
    }
    Err(CliRunError::Io {
        what: "evaluation directory",
        source: std::io::Error::other("no free directory name"),
    })
}

/// What happened to one CLI process. Output is not kept, only its size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseRun {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// The single new run directory this invocation created, if any.
    pub run_id: Option<String>,
    pub run_dir: Option<PathBuf>,
}

fn run_ids(dir: &Path) -> Vec<(u64, String)> {
    let Ok(entries) = fs::read_dir(dir.join(".baho").join("runs")) else {
        return Vec::new();
    };
    let mut ids: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            Some((name.parse::<u64>().ok()?, name))
        })
        .collect();
    ids.sort();
    ids
}

/// Runs `<binary> run <input> --prompt <prompt>` with `eval_dir` as working directory and
/// reports the run directory it created. "Latest" is by numeric run ID.
pub fn run_case(
    binary: &CliBinary,
    eval_dir: &Path,
    input: &Path,
    prompt: &str,
    limits: RunLimits,
) -> Result<CaseRun, CliRunError> {
    let before = run_ids(eval_dir);
    let outcome = run_bounded(
        Command::new(&binary.path)
            .current_dir(eval_dir)
            .arg("run")
            .arg(input)
            .arg("--prompt")
            .arg(prompt),
        limits.timeout,
        limits.max_output_bytes,
    )
    .map_err(io("baho-cli process"))?;
    let created = run_ids(eval_dir)
        .into_iter()
        .rfind(|id| !before.contains(id));
    Ok(CaseRun {
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        stdout_bytes: outcome.stdout_total,
        stderr_bytes: outcome.stderr_total,
        run_dir: created
            .as_ref()
            .map(|(_, n)| eval_dir.join(".baho").join("runs").join(n)),
        run_id: created.map(|(_, n)| n),
    })
}

// ---------------------------------------------------------------------------
// Run artifact parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Materialized,
    NeedsClarification,
    Error,
    Recorded,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DiagnosticSummary {
    pub code: String,
    pub severity: String,
    /// Bounded; may echo prompt text, so callers sanitize before publishing.
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedRun {
    pub run_id: String,
    pub outcome: RunOutcome,
    pub diagnostics: Vec<DiagnosticSummary>,
    pub plan_evidence: Option<PlanEvidence>,
    /// Stable refusal reason from recognition evidence, when the CLI refused.
    pub refusal_reason: Option<String>,
    /// Canonical operation name from recognition evidence, when recognized.
    pub canonical_operation: Option<String>,
    /// Source data rows (derived-CSV numbering) in the materialized result, in memory only.
    pub result_rows: Option<Vec<u64>>,
}

fn text<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn read_json(
    dir: &Path,
    file: &'static str,
    schema: u32,
) -> Result<Option<serde_json::Value>, CliRunError> {
    let path = dir.join(file);
    let artifact = |reason: String| CliRunError::Artifact { file, reason };
    let handle = match fs::File::open(&path) {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(artifact(e.to_string())),
    };
    let mut bytes = Vec::new();
    handle
        .take(MAX_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| artifact(e.to_string()))?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(artifact("exceeds size bound".into()));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| artifact(e.to_string()))?;
    let found = value.get("schema_version").and_then(|v| v.as_u64());
    if found != Some(u64::from(schema)) {
        return Err(artifact(format!(
            "unsupported schema version {found:?} (expected {schema})"
        )));
    }
    Ok(Some(value))
}

#[derive(Deserialize)]
struct RawDiagnostic {
    code: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    message: String,
}

pub fn parse_run(run_dir: &Path) -> Result<ParsedRun, CliRunError> {
    let manifest = read_json(run_dir, "manifest.json", MANIFEST_SCHEMA)?.ok_or_else(|| {
        CliRunError::Artifact {
            file: "manifest.json",
            reason: "missing".into(),
        }
    })?;
    let run_id = text(&manifest, "run_id").unwrap_or_default().to_owned();
    let outcome = match text(&manifest, "outcome") {
        Some("materialized") => RunOutcome::Materialized,
        Some("needs_clarification") => RunOutcome::NeedsClarification,
        Some("error") => RunOutcome::Error,
        Some("recorded") => RunOutcome::Recorded,
        Some(other) => RunOutcome::Other(other.chars().take(64).collect()),
        None => {
            return Err(CliRunError::Artifact {
                file: "manifest.json",
                reason: "missing outcome".into(),
            });
        }
    };

    let mut diagnostics = Vec::new();
    if let Some(doc) = read_json(run_dir, "diagnostics.json", DIAGNOSTICS_SCHEMA)? {
        let list = doc.get("diagnostics").cloned().unwrap_or_default();
        let raw: Vec<RawDiagnostic> =
            serde_json::from_value(list).map_err(|e| CliRunError::Artifact {
                file: "diagnostics.json",
                reason: e.to_string(),
            })?;
        diagnostics = raw
            .into_iter()
            .take(MAX_DIAGNOSTICS)
            .map(|d| DiagnosticSummary {
                code: d.code,
                severity: d.severity,
                message: d.message.chars().take(MAX_MESSAGE_CHARS).collect(),
            })
            .collect();
    }

    let plan = read_json(run_dir, "plan.json", PLAN_SCHEMA)?;
    let result = read_json(run_dir, "output/result.json", RESULT_SCHEMA)?;
    let (plan_evidence, refusal_reason, canonical_operation) = match &plan {
        Some(plan) => {
            let evidence = plan.get("recognition_evidence");
            (
                plan_evidence(plan, result.as_ref()),
                evidence
                    .and_then(|e| text(e, "refusal_reason"))
                    .map(str::to_owned),
                evidence
                    .and_then(|e| text(e, "canonical_operation"))
                    .map(str::to_owned),
            )
        }
        None => (None, None, None),
    };
    let result_rows = result.as_ref().map(result_source_rows).transpose()?;
    Ok(ParsedRun {
        run_id,
        outcome,
        diagnostics,
        plan_evidence,
        refusal_reason,
        canonical_operation,
        result_rows,
    })
}

/// Extracts a single-compare filter into the comparison contract's terms. Anything
/// outside that shape yields `None` (plan comparison `unavailable`), never a guess.
fn plan_evidence(
    plan: &serde_json::Value,
    result: Option<&serde_json::Value>,
) -> Option<PlanEvidence> {
    let steps = plan.get("plan")?.get("steps")?.as_array()?;
    let [step] = steps.as_slice() else {
        return None;
    };
    if step.get("op")?.as_str()? != "filter" {
        return None;
    }
    let predicate = step.get("predicate")?;
    if predicate.get("op")?.as_str()? != "compare" {
        return None;
    }
    let column_id = predicate.get("column")?.as_str()?;
    let column = display_name(column_id, plan, result)?;
    let operator = match predicate.get("operator")?.as_str()? {
        "=" => PlanOperator::Equal,
        "!=" | "<>" => PlanOperator::NotEqual,
        _ => PlanOperator::Other,
    };
    let literal = match predicate
        .get("literal")?
        .get("text")
        .and_then(|t| t.as_str())
    {
        Some(t) => PlanLiteral::Text(t.to_owned()),
        None => PlanLiteral::Other,
    };
    let policy = match plan
        .get("recognition_evidence")
        .and_then(|e| e.get("row_filter"))
        .and_then(|f| f.get("text_match"))
        .and_then(|t| t.as_str())
    {
        Some("unicode_lowercase") => TextMatchPolicy::UnicodeLowercase,
        _ => TextMatchPolicy::Other,
    };
    Some(PlanEvidence {
        column,
        operator,
        literal,
        text_match_policy: policy,
    })
}

fn display_name(
    id: &str,
    plan: &serde_json::Value,
    result: Option<&serde_json::Value>,
) -> Option<String> {
    let from_filter = plan
        .get("recognition_evidence")
        .and_then(|e| e.get("row_filter"))
        .and_then(|f| f.get("headers"))
        .and_then(|h| h.as_array())
        .and_then(|headers| {
            headers
                .iter()
                .find(|h| h.get("column_id").and_then(|c| c.as_str()) == Some(id))
        });
    let from_result = || {
        result
            .and_then(|r| r.get("result"))
            .and_then(|r| r.get("columns"))
            .and_then(|c| c.as_array())
            .and_then(|cols| {
                cols.iter()
                    .find(|c| c.get("id").and_then(|c| c.as_str()) == Some(id))
            })
    };
    from_filter
        .or_else(from_result)?
        .get("display_name")?
        .as_str()
        .map(str::to_owned)
}

fn result_source_rows(result: &serde_json::Value) -> Result<Vec<u64>, CliRunError> {
    let bad = || CliRunError::Artifact {
        file: "output/result.json",
        reason: "malformed provenance".into(),
    };
    let provenance = result
        .get("result")
        .and_then(|r| r.get("provenance"))
        .and_then(|p| p.as_array())
        .ok_or_else(bad)?;
    provenance
        .iter()
        .map(|p| p.get("source_row").and_then(|r| r.as_u64()).ok_or_else(bad))
        .collect()
}

#[cfg(test)]
pub(crate) mod stub {
    //! A stub `baho-cli` (POSIX shell) that writes realistic versioned run artifacts.
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    #[derive(Clone, Copy)]
    pub enum Mode {
        /// Case-insensitive equality on the single `Role` column, with provenance.
        Materialize,
        /// Records an error run with a refusal diagnostic.
        Refuse,
        /// Hangs until killed.
        Hang,
        /// Writes a large amount of output, then materializes nothing.
        Noisy,
        /// Reports an unrecognized version.
        BadVersion,
        /// Writes a plan with an unsupported schema version.
        FutureSchema,
        /// Passes both probes, then deletes itself so no case can start it.
        VanishAfterProbe,
    }

    const COMMON: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "baho 0.1.0"; exit 0; fi
if [ "$1" = "--help" ]; then printf 'Usage: baho <COMMAND>\n\nCommands:\n  run      Record work\n  runs     List\n'; exit 0; fi
INPUT="$2"; PROMPT="$4"
mkdir -p .baho/runs
N=$(ls .baho/runs | wc -l | tr -d ' '); N=$((N+1))
ID=$(printf '%06d' $N)
D=.baho/runs/$ID
mkdir -p $D/output
"#;

    const MATERIALIZE: &str = r#"LIT=${PROMPT#*= }
printf '{"schema_version":2,"run_id":"%s","outcome":"materialized"}' "$ID" > $D/manifest.json
printf '{"schema_version":1,"diagnostics":[{"code":"exec.rows_filtered","location":null,"message":"rows removed","severity":"Info","stage":"exec"}]}' > $D/diagnostics.json
printf '{"schema_version":5,"plan":{"schema_version":3,"steps":[{"op":"filter","predicate":{"op":"compare","column":"column-0","operator":"=","literal":{"text":"%s"}}}]},"recognition_evidence":{"schema_version":3,"refusal_reason":null,"canonical_operation":"row_filter","row_filter":{"headers":[{"column_id":"column-0","display_name":"Role"}],"text_match":"unicode_lowercase"}}}' "$LIT" > $D/plan.json
PROV=$(awk -v lit="$LIT" 'BEGIN{FS="\n"} NR>1 && tolower($0)==tolower(lit) {printf "%s{\"source_row\":%d,\"source_addresses\":[]}", (n++?",":""), NR-1}' "$INPUT")
printf '{"schema_version":2,"result":{"columns":[{"id":"column-0","display_name":"Role"}],"rows":[],"provenance":[%s]}}' "$PROV" > $D/output/result.json
"#;

    const REFUSE: &str = r#"printf '{"schema_version":2,"run_id":"%s","outcome":"error"}' "$ID" > $D/manifest.json
printf '{"schema_version":1,"diagnostics":[{"code":"intent.column_not_found","location":null,"message":"column not found for term","severity":"Error","stage":"intent"}]}' > $D/diagnostics.json
printf '{"schema_version":5,"recognition_evidence":{"schema_version":3,"refusal_reason":"intent.column_not_found","canonical_operation":null,"row_filter":null}}' > $D/plan.json
exit 1
"#;

    pub fn write(dir: &Path, mode: Mode) -> PathBuf {
        let body = match mode {
            Mode::Materialize => format!("{COMMON}{MATERIALIZE}"),
            Mode::Refuse => format!("{COMMON}{REFUSE}"),
            Mode::Hang => format!("{COMMON}exec sleep 30\n"),
            Mode::Noisy => format!(
                "{COMMON}yes 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' | head -c 2000000\n{REFUSE}"
            ),
            Mode::VanishAfterProbe => r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "baho 0.1.0"; exit 0; fi
if [ "$1" = "--help" ]; then printf 'Usage: baho <COMMAND>\n\nCommands:\n  run      Record work\n'; rm -f "$0"; exit 0; fi
exit 1
"#
            .to_owned(),
            Mode::BadVersion => "#!/bin/sh\necho 'something 9'\n".to_owned(),
            Mode::FutureSchema => {
                format!("{COMMON}{MATERIALIZE}printf '{{\"schema_version\":99}}' > $D/plan.json\n")
            }
        };
        let path = dir.join("stub-baho");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::stub::{Mode, write};
    use super::*;

    fn setup(mode: Mode) -> (tempfile::TempDir, CliBinary, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write(tmp.path(), mode);
        let binary = CliBinary::probe(&bin).unwrap();
        let eval = create_evaluation_dir(&tmp.path().join("out")).unwrap();
        (tmp, binary, eval)
    }

    fn source() -> Vec<u8> {
        b"Id,Name,Role,Pay\n1,A,Analyst,10\n2,\"B, Jr\",Barista,20\n3,C,\"Multi\nLine\",30\n4,D,analyst,40\n5,E,,50\n".to_vec()
    }

    #[test]
    fn derived_csv_keeps_only_role_with_row_mapping() {
        let derived = derive_role_csv(&source()).unwrap();
        let text = String::from_utf8(derived.bytes.clone()).unwrap();
        assert!(text.starts_with("Role\nAnalyst\nBarista\n"));
        assert!(!text.contains("Pay") && !text.contains("B, Jr"));
        assert_eq!(derived.row_count(), 5);
        assert_eq!(derived.source_row(4), Some(4));
        assert_eq!(derived.source_row(0), None);
        assert_eq!(derived.source_row(6), None);
        assert!(derive_role_csv(b"Id,Name\n1,A\n").is_err());
        assert!(derive_role_csv(b"Role,X\n1\n").is_err());
    }

    #[test]
    fn relative_binary_path_still_runs_cases_from_another_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = write(tmp.path(), Mode::Refuse);
        // Relative to the process cwd via `..` components, so global cwd is untouched.
        let cwd = std::env::current_dir().unwrap();
        let mut rel = PathBuf::new();
        for _ in cwd.components().skip(1) {
            rel.push("..");
        }
        rel.push(bin.strip_prefix("/").unwrap());
        assert!(rel.is_relative());
        let binary = CliBinary::probe(&rel).unwrap();
        assert!(binary.path.is_absolute());
        let eval = create_evaluation_dir(&tmp.path().join("out")).unwrap();
        let input = eval.join("input.csv");
        std::fs::write(&input, "Role\nA\n").unwrap();
        let run = run_case(&binary, &eval, &input, "p", RunLimits::default()).unwrap();
        assert!(run.run_id.is_some());
    }

    #[test]
    fn spawn_failures_have_stable_codes() {
        let tmp = tempfile::tempdir().unwrap();
        let ghost = CliBinary {
            path: tmp.path().join("ghost"),
            sha256: String::new(),
            version: String::new(),
        };
        let err = run_case(&ghost, tmp.path(), tmp.path(), "p", RunLimits::default()).unwrap_err();
        assert_eq!(spawn_failure_code(&err), "spawn_not_found");
    }

    #[test]
    fn probe_rejects_unrecognized_binary_and_captures_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = write(tmp.path(), Mode::BadVersion);
        assert!(matches!(
            CliBinary::probe(&bad),
            Err(CliRunError::Binary(_))
        ));
        assert!(CliBinary::probe(&tmp.path().join("missing")).is_err());
        let good = write(tmp.path(), Mode::Materialize);
        let binary = CliBinary::probe(&good).unwrap();
        assert_eq!(binary.version, "0.1.0");
        assert_eq!(binary.sha256.len(), 64);
    }

    #[test]
    fn materialized_run_parses_outcome_plan_and_rows() {
        let (tmp, binary, eval) = setup(Mode::Materialize);
        // The stub numbers physical lines, so avoid multi-line records here.
        let derived = derive_role_csv(b"Id,Role\n1,Analyst\n2,Barista\n3,x\n4,analyst\n").unwrap();
        let input = eval.join("input.csv");
        fs::write(&input, &derived.bytes).unwrap();
        let run = run_case(
            &binary,
            &eval,
            &input,
            "Show rows where Role = ANALYST",
            RunLimits::default(),
        )
        .unwrap();
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.run_id.as_deref(), Some("000001"));
        // Fresh evaluation directory holds the run, not the caller's cwd.
        assert!(eval.join(".baho/runs/000001").is_dir());
        let parsed = parse_run(run.run_dir.as_ref().unwrap()).unwrap();
        assert_eq!(parsed.outcome, RunOutcome::Materialized);
        assert_eq!(parsed.diagnostics[0].code, "exec.rows_filtered");
        assert_eq!(parsed.result_rows, Some(vec![1, 4]));
        let evidence = parsed.plan_evidence.unwrap();
        assert_eq!(evidence.column, "Role");
        assert_eq!(evidence.operator, PlanOperator::Equal);
        assert_eq!(evidence.literal, PlanLiteral::Text("ANALYST".into()));
        assert_eq!(
            evidence.text_match_policy,
            TextMatchPolicy::UnicodeLowercase
        );
        // A second case in the same directory is attributed to its own run.
        let second = run_case(&binary, &eval, &input, "x = Barista", RunLimits::default()).unwrap();
        assert_eq!(second.run_id.as_deref(), Some("000002"));
        drop(tmp);
    }

    #[test]
    fn refusal_run_keeps_diagnostics_without_plan_or_rows() {
        let (_tmp, binary, eval) = setup(Mode::Refuse);
        let input = eval.join("input.csv");
        fs::write(&input, b"Role\nA\n").unwrap();
        let run = run_case(&binary, &eval, &input, "p", RunLimits::default()).unwrap();
        assert_eq!(run.exit_code, Some(1));
        let parsed = parse_run(run.run_dir.as_ref().unwrap()).unwrap();
        assert_eq!(parsed.outcome, RunOutcome::Error);
        assert_eq!(parsed.diagnostics[0].code, "intent.column_not_found");
        assert_eq!(
            parsed.refusal_reason.as_deref(),
            Some("intent.column_not_found")
        );
        assert!(parsed.plan_evidence.is_none());
        assert!(parsed.result_rows.is_none());
    }

    #[test]
    fn timeout_kills_the_process() {
        let (_tmp, binary, eval) = setup(Mode::Hang);
        let input = eval.join("input.csv");
        fs::write(&input, b"Role\nA\n").unwrap();
        let limits = RunLimits {
            timeout: Duration::from_millis(200),
            ..RunLimits::default()
        };
        let started = Instant::now();
        let run = run_case(&binary, &eval, &input, "p", limits).unwrap();
        assert!(run.timed_out);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_is_counted_but_bounded() {
        let (_tmp, binary, eval) = setup(Mode::Noisy);
        let input = eval.join("input.csv");
        fs::write(&input, b"Role\nA\n").unwrap();
        let limits = RunLimits {
            max_output_bytes: 1024,
            ..RunLimits::default()
        };
        let run = run_case(&binary, &eval, &input, "p", limits).unwrap();
        assert!(run.stdout_bytes >= 2_000_000);
        assert!(run.run_dir.is_some());
    }

    #[test]
    fn unsupported_artifact_schema_is_an_error() {
        let (_tmp, binary, eval) = setup(Mode::FutureSchema);
        let input = eval.join("input.csv");
        fs::write(&input, b"Role\nA\n").unwrap();
        let run = run_case(&binary, &eval, &input, "x = A", RunLimits::default()).unwrap();
        let err = parse_run(run.run_dir.as_ref().unwrap()).unwrap_err();
        assert!(matches!(
            err,
            CliRunError::Artifact {
                file: "plan.json",
                ..
            }
        ));
    }

    #[test]
    fn evaluation_dirs_are_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let a = create_evaluation_dir(tmp.path()).unwrap();
        let b = create_evaluation_dir(tmp.path()).unwrap();
        assert_ne!(a, b);
        assert!(!a.join(".baho").exists());
    }

    #[test]
    fn repo_state_reports_revision_and_dirty_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(tmp.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            )
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        let clean = RepoState::capture(tmp.path()).unwrap();
        assert_eq!(clean.revision.len(), 40);
        assert!(!clean.dirty);
        fs::write(tmp.path().join("f"), "x").unwrap();
        assert!(RepoState::capture(tmp.path()).unwrap().dirty);
    }
}
