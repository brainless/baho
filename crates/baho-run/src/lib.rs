//! UI-neutral diagnostic run reservation and persistence.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use baho_core::grounding::{
    CLARIFICATION_SCHEMA_VERSION, ClarificationRequest, ClarificationResponse,
    GROUNDING_SCHEMA_VERSION, GroundingOutcome, GroundingResult,
};
use baho_core::{CoreOutcome, CoreResult, ParserConfig};
use baho_model::{candidate::TableCandidate, revision::SourceRevision};
use baho_plan::{evidence::RecognitionEvidence, plan::Plan};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// `diagnostics.json` envelope schema version.
///
/// Adding the optional, defaulted `DiagnosticLocation.cells` sample list is
/// backward compatible: pre-existing diagnostics serialize exactly as before
/// and older artifacts deserialize with an empty list, so the version is
/// intentionally kept at 1. Compatibility is covered in `baho-model`'s
/// serde tests.
const RUN_SCHEMA_VERSION: u32 = 1;
const RUN_ID_WIDTH: usize = 6;
/// `plan.json` envelope schema version.
///
/// Version 4 covers the recognition-evidence envelope with its own
/// `schema_version` and row-filter evidence (Epic 006). Version 5 (Epic 008
/// locked decision 14) covers recognition-evidence version 3 with text-match
/// policy, deferred numeric literals, and the extended `NumericParsePolicy`
/// value space. Historical envelopes are never rewritten or reinterpreted.
const PLAN_ARTIFACT_SCHEMA_VERSION: u32 = 5;
/// Version 2 adds a clarification outcome and optional immutable run links.
const MANIFEST_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Error)]
pub enum RunRecordError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{context}: {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("run ID space is exhausted")]
    RunIdExhausted,
    #[error("input is not a regular file: {0}")]
    NotAFile(PathBuf),
    #[error("invalid pending clarification: {0}")]
    InvalidClarification(String),
}

fn io(context: impl Into<String>, source: std::io::Error) -> RunRecordError {
    RunRecordError::Io {
        context: context.into(),
        source,
    }
}

#[derive(Debug, Clone)]
pub struct Invocation {
    pub command: String,
    pub action: String,
    pub event_target: String,
    pub arguments: Vec<String>,
    pub working_directory: PathBuf,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputIdentity {
    pub path: String,
    pub absolute_path: String,
    pub size_bytes: u64,
    pub modified_at: Option<String>,
    pub sha256: String,
}

impl InputIdentity {
    pub fn inspect(supplied_path: &Path, absolute_path: &Path) -> Result<Self, RunRecordError> {
        let metadata = fs::metadata(absolute_path).map_err(|error| {
            io(
                format!("could not read input {}", supplied_path.display()),
                error,
            )
        })?;
        if !metadata.is_file() {
            return Err(RunRecordError::NotAFile(supplied_path.to_owned()));
        }
        let file = File::open(absolute_path).map_err(|error| {
            io(
                format!("could not open input {}", supplied_path.display()),
                error,
            )
        })?;
        let mut reader = BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer).map_err(|error| {
                io(
                    format!("could not hash input {}", supplied_path.display()),
                    error,
                )
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(Self {
            path: supplied_path.to_string_lossy().into_owned(),
            absolute_path: absolute_path.to_string_lossy().into_owned(),
            size_bytes: metadata.len(),
            modified_at: metadata.modified().ok().map(format_system_time),
            sha256: format!("{:x}", hasher.finalize()),
        })
    }

    /// Builds identity from metadata captured by the opened snapshot, without
    /// touching bytes that may since have changed on disk.
    pub fn from_snapshot(
        supplied_path: &Path,
        absolute_path: &Path,
        revision: &SourceRevision,
    ) -> Self {
        Self {
            path: supplied_path.to_string_lossy().into_owned(),
            absolute_path: absolute_path.to_string_lossy().into_owned(),
            size_bytes: revision.file_size,
            modified_at: revision.modified_time.clone(),
            sha256: revision.content_hash.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordedRun {
    pub id: String,
    pub path: PathBuf,
    pub materialized: bool,
    pub needs_clarification: bool,
}

/// The immutable request and invocation context needed to resume a run.
#[derive(Debug, Clone)]
pub struct PendingClarification {
    pub run_id: String,
    pub input_path: PathBuf,
    pub prompt: String,
    pub request: ClarificationRequest,
}

/// Read only a finalized clarification run, checking its artifact index and
/// request against the recorded input identity. Core revalidates the source.
pub fn load_pending_clarification(
    runs_directory: &Path,
    run_id: &str,
) -> Result<PendingClarification, RunRecordError> {
    if run_id.len() < RUN_ID_WIDTH || !run_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RunRecordError::InvalidClarification(
            "invalid run ID".into(),
        ));
    }
    let directory = runs_directory.join(run_id);
    let manifest: StoredManifest = read_json(&directory.join("manifest.json"))?;
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION
        || manifest.run_id != run_id
        || manifest.outcome != "needs_clarification"
        || !manifest
            .artifacts
            .iter()
            .any(|name| name == "grounding.json")
    {
        return Err(RunRecordError::InvalidClarification(
            "run has no supported pending clarification".into(),
        ));
    }
    let grounding: GroundingResult = read_json(&directory.join("grounding.json"))?;
    let GroundingOutcome::NeedsClarification { request } = grounding.outcome else {
        return Err(RunRecordError::InvalidClarification(
            "grounding artifact does not contain a request".into(),
        ));
    };
    if grounding.schema_version != GROUNDING_SCHEMA_VERSION
        || request.schema_version != CLARIFICATION_SCHEMA_VERSION
    {
        return Err(RunRecordError::InvalidClarification(
            "unsupported clarification schema".into(),
        ));
    }
    let input = manifest
        .input
        .ok_or_else(|| RunRecordError::InvalidClarification("run has no input identity".into()))?;
    if input.sha256 != request.source_revision.content_hash
        || input.size_bytes != request.source_revision.file_size
    {
        return Err(RunRecordError::InvalidClarification(
            "request and manifest input identities disagree".into(),
        ));
    }
    let prompt = fs::read_to_string(directory.join("intent.txt"))
        .map_err(|error| io("could not read intent.txt", error))?;
    if format!("{:x}", Sha256::digest(prompt.as_bytes())) != request.prompt_identity {
        return Err(RunRecordError::InvalidClarification(
            "request and recorded prompt identities disagree".into(),
        ));
    }
    if !manifest
        .artifacts
        .iter()
        .any(|name| name == "parser-config.json")
    {
        return Err(RunRecordError::InvalidClarification(
            "run has no parser configuration artifact".into(),
        ));
    }
    let parser_config: ParserConfig = read_json(&directory.join("parser-config.json"))?;
    let config_bytes =
        serde_json::to_vec(&parser_config).map_err(|source| RunRecordError::Json {
            context: "could not serialize recorded parser configuration".into(),
            source,
        })?;
    if format!("{:x}", Sha256::digest(config_bytes)) != request.parser_config_identity {
        return Err(RunRecordError::InvalidClarification(
            "request and recorded parser configuration identities disagree".into(),
        ));
    }
    Ok(PendingClarification {
        run_id: run_id.into(),
        input_path: PathBuf::from(input.absolute_path),
        prompt,
        request,
    })
}

#[derive(Deserialize)]
struct StoredManifest {
    schema_version: u32,
    run_id: String,
    outcome: String,
    input: Option<InputIdentity>,
    artifacts: Vec<String>,
}

pub struct PendingRun {
    id: String,
    absolute_path: PathBuf,
    reported_path: PathBuf,
    manifest: Manifest,
    events: EventLog,
    timer: Instant,
    clarification_response: Option<ClarificationResponse>,
}

impl PendingRun {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn reserve(
        runs_directory: &Path,
        invocation: Invocation,
        prompt: &str,
    ) -> Result<Self, RunRecordError> {
        let (id, absolute_path) = reserve_run(runs_directory)?;
        let reported_path = invocation
            .working_directory
            .canonicalize()
            .ok()
            .and_then(|cwd| absolute_path.strip_prefix(cwd).ok().map(Path::to_owned))
            .unwrap_or_else(|| absolute_path.clone());
        let manifest_path = absolute_path.join("manifest.json");
        let event_target = invocation.event_target.clone();
        let manifest = Manifest::new(&id, invocation);
        write_json(&manifest_path, &manifest)?;
        fs::write(absolute_path.join("intent.txt"), prompt)
            .map_err(|error| io("could not write intent.txt", error))?;
        let mut events = EventLog::create(&absolute_path.join("events.jsonl"), event_target)?;
        events.write(
            "INFO",
            "run",
            "run_started",
            BTreeMap::from([("run_id".into(), json!(id))]),
        )?;
        Ok(Self {
            id,
            absolute_path,
            reported_path,
            manifest,
            events,
            timer: Instant::now(),
            clarification_response: None,
        })
    }

    /// Link a new reserved run to its immutable clarification source.
    pub fn link_resolution(
        mut self,
        original: &PendingClarification,
        response: ClarificationResponse,
    ) -> Result<Self, RunRecordError> {
        self.manifest.resumes_run_id = Some(original.run_id.clone());
        self.manifest.clarification_request_id = Some(original.request.request_id.clone());
        self.clarification_response = Some(response);
        write_json(&self.absolute_path.join("manifest.json"), &self.manifest)?;
        Ok(self)
    }

    pub fn record_result(
        mut self,
        input: InputIdentity,
        result: &CoreResult,
    ) -> Result<RecordedRun, RunRecordError> {
        self.events.write(
            "INFO",
            "input",
            "input_identified",
            BTreeMap::from([
                ("size_bytes".into(), json!(input.size_bytes)),
                ("sha256".into(), json!(input.sha256)),
            ]),
        )?;
        self.manifest.input = Some(input);
        self.record_clarification_response()?;
        for event in &result.events {
            self.events.write(
                "INFO",
                &event.stage,
                &event.name,
                BTreeMap::from([("fields".into(), event.fields.clone())]),
            )?;
        }
        if let Some(profile) = &result.input_profile {
            write_json(
                &self.absolute_path.join("input-profile.json"),
                &Versioned {
                    schema_version: 1,
                    profile,
                },
            )?;
            self.manifest.artifacts.push("input-profile.json".into());
        }
        if let Some(config) = &result.parser_config {
            write_json(&self.absolute_path.join("parser-config.json"), config)?;
            self.manifest.artifacts.push("parser-config.json".into());
        }
        write_json(
            &self.absolute_path.join("candidates.json"),
            &CandidatesArtifact {
                schema_version: 1,
                candidates: &result.candidates,
                selected: result.selected_candidate.as_ref(),
            },
        )?;
        self.manifest.artifacts.push("candidates.json".into());
        if let Some(evidence) = result.intent_evidence.as_ref() {
            write_json(
                &self.absolute_path.join("plan.json"),
                &PlanArtifact {
                    schema_version: PLAN_ARTIFACT_SCHEMA_VERSION,
                    plan: result.plan.as_ref(),
                    recognition_evidence: Some(evidence),
                },
            )?;
            self.manifest.artifacts.push("plan.json".into());
        }
        if let Some(grounding) = result.grounding.as_ref() {
            write_json(&self.absolute_path.join("grounding.json"), grounding)?;
            self.manifest.artifacts.push("grounding.json".into());
            if let GroundingOutcome::NeedsClarification { request } = &grounding.outcome {
                self.manifest.clarification_request_id = Some(request.request_id.clone());
            }
        }
        if let Some(output) = &result.output {
            fs::create_dir_all(self.absolute_path.join("output"))
                .map_err(|error| io("could not create output directory", error))?;
            write_json(
                &self.absolute_path.join("output/result.json"),
                &ResultArtifact {
                    schema_version: 2,
                    result: output,
                },
            )?;
            self.manifest.artifacts.push("output/result.json".into());
        }
        write_json(
            &self.absolute_path.join("diagnostics.json"),
            &json!({
                "schema_version": RUN_SCHEMA_VERSION, "diagnostics": result.diagnostics,
            }),
        )?;
        let needs_clarification = matches!(
            result
                .grounding
                .as_ref()
                .map(|grounding| &grounding.outcome),
            Some(GroundingOutcome::NeedsClarification { .. })
        );
        let (outcome, label, materialized) = match (result.outcome.clone(), needs_clarification) {
            (_, true) => (Outcome::NeedsClarification, "needs_clarification", false),
            (CoreOutcome::Materialized, _) => (Outcome::Materialized, "materialized", true),
            (CoreOutcome::Recorded, _) => (Outcome::Recorded, "recorded", false),
            (CoreOutcome::Failed, _) => (Outcome::Error, "error", false),
        };
        self.events.write(
            "INFO",
            "run",
            "run_finished",
            BTreeMap::from([("outcome".into(), json!(label))]),
        )?;
        self.manifest.outcome = outcome;
        self.finish_manifest()?;
        Ok(self.recorded(materialized, needs_clarification))
    }

    pub fn record_input_error(
        mut self,
        message: impl Into<String>,
    ) -> Result<RecordedRun, RunRecordError> {
        let message = message.into();
        self.record_clarification_response()?;
        write_json(
            &self.absolute_path.join("diagnostics.json"),
            &Diagnostics {
                schema_version: 1,
                diagnostics: vec![RunDiagnostic {
                    code: "input.unreadable",
                    severity: "error",
                    stage: "input",
                    message: message.clone(),
                }],
            },
        )?;
        self.events.write(
            "ERROR",
            "input",
            "input_unreadable",
            BTreeMap::from([("message".into(), json!(message))]),
        )?;
        self.events.write(
            "INFO",
            "run",
            "run_finished",
            BTreeMap::from([("outcome".into(), json!("error"))]),
        )?;
        self.manifest.outcome = Outcome::Error;
        self.manifest.error = Some(RunError {
            code: "input.unreadable",
            message,
        });
        self.finish_manifest()?;
        Ok(self.recorded(false, false))
    }

    fn record_clarification_response(&mut self) -> Result<(), RunRecordError> {
        if let Some(response) = &self.clarification_response {
            write_json(
                &self.absolute_path.join("clarification-response.json"),
                response,
            )?;
            self.manifest
                .artifacts
                .push("clarification-response.json".into());
        }
        Ok(())
    }

    fn finish_manifest(&mut self) -> Result<(), RunRecordError> {
        self.manifest.finished_at = Some(now());
        self.manifest.duration_ms = Some(self.timer.elapsed().as_millis());
        write_json(&self.absolute_path.join("manifest.json"), &self.manifest)
    }

    fn recorded(&self, materialized: bool, needs_clarification: bool) -> RecordedRun {
        RecordedRun {
            id: self.id.clone(),
            path: self.reported_path.clone(),
            materialized,
            needs_clarification,
        }
    }
}

pub fn list(
    runs_directory: &Path,
    reported_root: &Path,
    latest: bool,
) -> Result<Vec<PathBuf>, RunRecordError> {
    let mut ids = run_ids(runs_directory)?;
    if latest {
        ids = ids.into_iter().rev().take(1).collect();
    }
    Ok(ids
        .into_iter()
        .map(|id| reported_root.join(format!("{id:0RUN_ID_WIDTH$}")))
        .collect())
}

#[derive(Serialize)]
struct Manifest {
    schema_version: u32,
    run_id: String,
    started_at: String,
    finished_at: Option<String>,
    duration_ms: Option<u128>,
    outcome: Outcome,
    invocation: PersistedInvocation,
    build: Build,
    platform: Platform,
    input: Option<InputIdentity>,
    artifacts: Vec<String>,
    error: Option<RunError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resumes_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clarification_request_id: Option<String>,
}

impl Manifest {
    fn new(id: &str, invocation: Invocation) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            run_id: id.into(),
            started_at: now(),
            finished_at: None,
            duration_ms: None,
            outcome: Outcome::Running,
            invocation: PersistedInvocation {
                command: invocation.command,
                subcommand: invocation.action,
                arguments: invocation.arguments,
                working_directory: invocation.working_directory.to_string_lossy().into_owned(),
                output: invocation
                    .output
                    .map(|path| path.to_string_lossy().into_owned()),
            },
            build: Build {
                version: env!("CARGO_PKG_VERSION"),
                git_revision: option_env!("BAHO_GIT_REVISION"),
            },
            platform: Platform {
                os: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
            },
            input: None,
            artifacts: vec![
                "manifest.json".into(),
                "intent.txt".into(),
                "events.jsonl".into(),
                "diagnostics.json".into(),
            ],
            error: None,
            resumes_run_id: None,
            clarification_request_id: None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Running,
    Materialized,
    Recorded,
    NeedsClarification,
    Error,
}
#[derive(Serialize)]
struct PersistedInvocation {
    command: String,
    subcommand: String,
    arguments: Vec<String>,
    working_directory: String,
    output: Option<String>,
}
#[derive(Serialize)]
struct Build {
    version: &'static str,
    git_revision: Option<&'static str>,
}
#[derive(Serialize)]
struct Platform {
    os: &'static str,
    architecture: &'static str,
}
#[derive(Serialize)]
struct RunError {
    code: &'static str,
    message: String,
}
#[derive(Serialize)]
struct Diagnostics {
    schema_version: u32,
    diagnostics: Vec<RunDiagnostic>,
}
#[derive(Serialize)]
struct RunDiagnostic {
    code: &'static str,
    severity: &'static str,
    stage: &'static str,
    message: String,
}
#[derive(Serialize)]
struct Versioned<'a, T> {
    schema_version: u32,
    profile: &'a T,
}
#[derive(Serialize)]
struct CandidatesArtifact<'a> {
    schema_version: u32,
    candidates: &'a [TableCandidate],
    selected: Option<&'a TableCandidate>,
}
#[derive(Serialize)]
struct PlanArtifact<'a> {
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<&'a Plan>,
    recognition_evidence: Option<&'a RecognitionEvidence>,
}
#[derive(Serialize)]
struct ResultArtifact<'a> {
    schema_version: u32,
    result: &'a baho_model::materialized::MaterializedView,
}

struct EventLog {
    writer: BufWriter<File>,
    target: String,
}
impl EventLog {
    fn create(path: &Path, target: String) -> Result<Self, RunRecordError> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|error| io(format!("could not create {}", path.display()), error))?;
        Ok(Self {
            writer: BufWriter::new(file),
            target,
        })
    }
    fn write(
        &mut self,
        level: &str,
        stage: &str,
        event: &str,
        fields: BTreeMap<String, JsonValue>,
    ) -> Result<(), RunRecordError> {
        #[derive(Serialize)]
        struct Event<'a> {
            timestamp: String,
            level: &'a str,
            target: &'a str,
            stage: &'a str,
            event: &'a str,
            fields: BTreeMap<String, JsonValue>,
        }
        serde_json::to_writer(
            &mut self.writer,
            &Event {
                timestamp: now(),
                level,
                target: &self.target,
                stage,
                event,
                fields,
            },
        )
        .map_err(|source| RunRecordError::Json {
            context: "could not serialize run event".into(),
            source,
        })?;
        self.writer
            .write_all(b"\n")
            .map_err(|error| io("could not write run event", error))?;
        self.writer
            .flush()
            .map_err(|error| io("could not flush run event", error))
    }
}

fn reserve_run(runs_directory: &Path) -> Result<(String, PathBuf), RunRecordError> {
    fs::create_dir_all(runs_directory).map_err(|error| {
        io(
            format!("could not create {}", runs_directory.display()),
            error,
        )
    })?;
    let mut candidate = run_ids(runs_directory)?
        .into_iter()
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(RunRecordError::RunIdExhausted)?;
    loop {
        let id = format!("{candidate:0RUN_ID_WIDTH$}");
        let path = runs_directory.join(&id);
        match fs::create_dir(&path) {
            Ok(()) => return Ok((id, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                candidate = candidate
                    .checked_add(1)
                    .ok_or(RunRecordError::RunIdExhausted)?
            }
            Err(error) => {
                return Err(io(
                    format!("could not reserve run directory {}", path.display()),
                    error,
                ));
            }
        }
    }
}

fn run_ids(runs_directory: &Path) -> Result<Vec<u64>, RunRecordError> {
    let entries = match fs::read_dir(runs_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(io(
                format!("could not read {}", runs_directory.display()),
                error,
            ));
        }
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io("could not read run directory entry", error))?;
        if !entry
            .file_type()
            .map_err(|error| io("could not inspect run directory entry", error))?
            .is_dir()
        {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.len() >= RUN_ID_WIDTH && name.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Ok(id) = name.parse() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), RunRecordError> {
    let file = File::create(path)
        .map_err(|error| io(format!("could not create {}", path.display()), error))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value).map_err(|source| RunRecordError::Json {
        context: format!("could not serialize {}", path.display()),
        source,
    })?;
    writer
        .write_all(b"\n")
        .map_err(|error| io(format!("could not write {}", path.display()), error))?;
    writer
        .flush()
        .map_err(|error| io(format!("could not flush {}", path.display()), error))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, RunRecordError> {
    let file = File::open(path)
        .map_err(|error| io(format!("could not open {}", path.display()), error))?;
    serde_json::from_reader(BufReader::new(file)).map_err(|source| RunRecordError::Json {
        context: format!("could not deserialize {}", path.display()),
        source,
    })
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC 3339 formatting has no variable components")
}
fn format_system_time(value: SystemTime) -> String {
    OffsetDateTime::from(value)
        .format(&Rfc3339)
        .expect("RFC 3339 formatting has no variable components")
}
