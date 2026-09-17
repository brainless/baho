use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use anyhow::{Context, Result, anyhow};
use baho_core::CoreOutcome;
use baho_model::candidate::TableCandidate;
use baho_model::document::Value;
use baho_model::materialized::MaterializedView;
use baho_plan::{evidence::RecognitionEvidence, plan::Plan};
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const RUN_SCHEMA_VERSION: u32 = 1;
const INPUT_PROFILE_ARTIFACT_SCHEMA_VERSION: u32 = 1;
const CANDIDATES_ARTIFACT_SCHEMA_VERSION: u32 = 1;
const PLAN_ARTIFACT_SCHEMA_VERSION: u32 = 1;
const MATERIALIZED_RESULT_ARTIFACT_SCHEMA_VERSION: u32 = 2;
const RUN_ID_WIDTH: usize = 6;

#[derive(Debug, Serialize)]
struct InputProfileArtifact<T> {
    schema_version: u32,
    profile: T,
}

impl<T> InputProfileArtifact<T> {
    fn new(profile: T) -> Self {
        Self {
            schema_version: INPUT_PROFILE_ARTIFACT_SCHEMA_VERSION,
            profile,
        }
    }
}

#[derive(Debug, Serialize)]
struct CandidatesArtifact<'a> {
    schema_version: u32,
    candidates: &'a [TableCandidate],
    selected: Option<&'a TableCandidate>,
}

#[derive(Debug, Serialize)]
struct PlanArtifact<'a> {
    schema_version: u32,
    plan: &'a Plan,
    recognition_evidence: Option<&'a RecognitionEvidence>,
}

#[derive(Debug, Serialize)]
struct MaterializedResultArtifact<'a> {
    schema_version: u32,
    result: &'a MaterializedView,
}

#[derive(Debug)]
pub(crate) struct RecordedRun {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) materialized: bool,
}

#[derive(Debug)]
pub(crate) struct RecordFailure {
    pub(crate) run: Option<RecordedRun>,
    pub(crate) error: anyhow::Error,
}

#[derive(Debug, Serialize)]
struct Manifest {
    schema_version: u32,
    run_id: String,
    started_at: String,
    finished_at: Option<String>,
    duration_ms: Option<u128>,
    outcome: Outcome,
    invocation: Invocation,
    build: Build,
    platform: Platform,
    input: Option<InputIdentity>,
    artifacts: Vec<String>,
    error: Option<RunError>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Running,
    Materialized,
    Recorded,
    Error,
}

#[derive(Debug, Serialize)]
struct Invocation {
    command: String,
    subcommand: &'static str,
    arguments: Vec<String>,
    working_directory: String,
    output: Option<String>,
}

#[derive(Debug, Serialize)]
struct Build {
    version: &'static str,
    git_revision: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct Platform {
    os: &'static str,
    architecture: &'static str,
}

#[derive(Debug, Serialize)]
struct InputIdentity {
    path: String,
    absolute_path: String,
    size_bytes: u64,
    modified_at: Option<String>,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct RunError {
    code: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
struct Diagnostics {
    schema_version: u32,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
struct Diagnostic {
    code: &'static str,
    severity: &'static str,
    stage: &'static str,
    message: String,
}

struct EventLog {
    writer: BufWriter<File>,
}

impl EventLog {
    fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .with_context(|| format!("could not create {}", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    fn write(
        &mut self,
        level: &str,
        stage: &str,
        event: &str,
        fields: BTreeMap<String, JsonValue>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Event<'a> {
            timestamp: String,
            level: &'a str,
            target: &'static str,
            stage: &'a str,
            event: &'a str,
            fields: BTreeMap<String, JsonValue>,
        }

        serde_json::to_writer(
            &mut self.writer,
            &Event {
                timestamp: now(),
                level,
                target: "baho_cli",
                stage,
                event,
                fields,
            },
        )
        .context("could not serialize run event")?;
        self.writer.write_all(b"\n")?;
        self.writer.flush().context("could not flush run event")
    }
}

pub(crate) fn record(
    input: PathBuf,
    prompt: String,
    output: Option<PathBuf>,
    arguments: Vec<String>,
) -> std::result::Result<RecordedRun, RecordFailure> {
    let working_directory =
        match std::env::current_dir().context("could not read working directory") {
            Ok(path) => path,
            Err(error) => return Err(RecordFailure { run: None, error }),
        };
    let runs_directory = working_directory.join(".baho/runs");
    let (id, absolute_run_path) = match reserve_run(&runs_directory) {
        Ok(reservation) => reservation,
        Err(error) => return Err(RecordFailure { run: None, error }),
    };
    let relative_run_path = PathBuf::from(format!(".baho/runs/{id}"));

    let result = record_reserved(
        &absolute_run_path,
        &id,
        &working_directory,
        &input,
        &prompt,
        output.as_deref(),
        arguments,
    );

    match result {
        Ok(materialized) => Ok(RecordedRun {
            id: id.clone(),
            path: relative_run_path,
            materialized,
        }),
        Err(error) => Err(RecordFailure {
            run: Some(RecordedRun {
                id: id.clone(),
                path: relative_run_path,
                materialized: false,
            }),
            error,
        }),
    }
}

fn record_reserved(
    run_path: &Path,
    id: &str,
    working_directory: &Path,
    input: &Path,
    prompt: &str,
    output: Option<&Path>,
    arguments: Vec<String>,
) -> Result<bool> {
    let started_at = now();
    let timer = Instant::now();
    let manifest_path = run_path.join("manifest.json");
    let mut manifest = Manifest {
        schema_version: RUN_SCHEMA_VERSION,
        run_id: id.to_owned(),
        started_at,
        finished_at: None,
        duration_ms: None,
        outcome: Outcome::Running,
        invocation: Invocation {
            command: arguments
                .first()
                .cloned()
                .unwrap_or_else(|| "baho".to_owned()),
            subcommand: "run",
            arguments,
            working_directory: working_directory.to_string_lossy().into_owned(),
            output: output.map(|path| path.to_string_lossy().into_owned()),
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
            "manifest.json".to_owned(),
            "intent.txt".to_owned(),
            "events.jsonl".to_owned(),
            "diagnostics.json".to_owned(),
        ],
        error: None,
    };
    write_json(&manifest_path, &manifest)?;

    fs::write(run_path.join("intent.txt"), prompt).context("could not write intent.txt")?;
    let mut events = EventLog::create(&run_path.join("events.jsonl"))?;
    events.write(
        "INFO",
        "run",
        "run_started",
        BTreeMap::from([("run_id".to_owned(), json!(id))]),
    )?;

    let absolute_input = if input.is_absolute() {
        input.to_owned()
    } else {
        working_directory.join(input)
    };

    match identify_input(input, &absolute_input) {
        Ok(identity) => {
            events.write(
                "INFO",
                "input",
                "input_identified",
                BTreeMap::from([
                    ("size_bytes".to_owned(), json!(identity.size_bytes)),
                    ("sha256".to_owned(), json!(identity.sha256)),
                ]),
            )?;
            manifest.input = Some(identity);

            let core_result = baho_core::run_pipeline(&absolute_input, prompt);

            for core_event in &core_result.events {
                events.write(
                    "INFO",
                    &core_event.stage,
                    &core_event.name,
                    BTreeMap::from([("fields".to_owned(), core_event.fields.clone())]),
                )?;
            }

            if let Some(ref profile) = core_result.input_profile {
                write_json(
                    &run_path.join("input-profile.json"),
                    &InputProfileArtifact::new(profile),
                )?;
                manifest.artifacts.push("input-profile.json".to_owned());
            }

            if let Some(ref config) = core_result.parser_config {
                write_json(&run_path.join("parser-config.json"), config)?;
                manifest.artifacts.push("parser-config.json".to_owned());
            }

            write_json(
                &run_path.join("candidates.json"),
                &CandidatesArtifact {
                    schema_version: CANDIDATES_ARTIFACT_SCHEMA_VERSION,
                    candidates: &core_result.candidates,
                    selected: core_result.selected_candidate.as_ref(),
                },
            )?;
            manifest.artifacts.push("candidates.json".to_owned());

            if let Some(ref plan) = core_result.plan {
                let plan_artifact = PlanArtifact {
                    schema_version: PLAN_ARTIFACT_SCHEMA_VERSION,
                    plan,
                    recognition_evidence: core_result.intent.as_ref().map(|i| &i.evidence),
                };
                write_json(&run_path.join("plan.json"), &plan_artifact)?;
                manifest.artifacts.push("plan.json".to_owned());
            }

            if let Some(ref view) = core_result.output {
                fs::create_dir_all(run_path.join("output"))
                    .context("could not create output directory")?;
                write_json(
                    &run_path.join("output/result.json"),
                    &MaterializedResultArtifact {
                        schema_version: MATERIALIZED_RESULT_ARTIFACT_SCHEMA_VERSION,
                        result: view,
                    },
                )?;
                manifest.artifacts.push("output/result.json".to_owned());
            }

            write_json(
                &run_path.join("diagnostics.json"),
                &serde_json::json!({
                    "schema_version": RUN_SCHEMA_VERSION,
                    "diagnostics": core_result.diagnostics,
                }),
            )?;

            let outcome = match core_result.outcome {
                CoreOutcome::Materialized => Outcome::Materialized,
                CoreOutcome::Recorded => Outcome::Recorded,
                CoreOutcome::Failed => Outcome::Error,
            };

            events.write(
                "INFO",
                "run",
                "run_finished",
                BTreeMap::from([(
                    "outcome".to_owned(),
                    json!(match core_result.outcome {
                        CoreOutcome::Materialized => "materialized",
                        CoreOutcome::Recorded => "recorded",
                        CoreOutcome::Failed => "error",
                    }),
                )]),
            )?;

            if matches!(core_result.outcome, CoreOutcome::Materialized) {
                if let Some(ref view) = core_result.output {
                    for row in &view.rows {
                        for value in &row.values {
                            match value {
                                Some(Value::Text(s)) => println!("{s}"),
                                Some(Value::Number(n)) => println!("{n}"),
                                Some(Value::Boolean(b)) => println!("{b}"),
                                Some(Value::Blank) | None => println!(""),
                            }
                        }
                    }
                }
            }

            manifest.outcome = outcome;
            finish_manifest(&mut manifest, timer);
            write_json(&manifest_path, &manifest)?;

            if matches!(core_result.outcome, CoreOutcome::Materialized) {
                Ok(true)
            } else {
                let error_msg = core_result
                    .diagnostics
                    .iter()
                    .filter(|d| matches!(d.severity, baho_model::diagnostic::Severity::Error))
                    .map(|d| d.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                Err(anyhow!("{}", error_msg))
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            let diagnostics = Diagnostics {
                schema_version: RUN_SCHEMA_VERSION,
                diagnostics: vec![Diagnostic {
                    code: "input.unreadable",
                    severity: "error",
                    stage: "input",
                    message: message.clone(),
                }],
            };
            write_json(&run_path.join("diagnostics.json"), &diagnostics)?;
            events.write(
                "ERROR",
                "input",
                "input_unreadable",
                BTreeMap::from([("message".to_owned(), json!(message))]),
            )?;
            events.write(
                "INFO",
                "run",
                "run_finished",
                BTreeMap::from([("outcome".to_owned(), json!("error"))]),
            )?;

            manifest.outcome = Outcome::Error;
            manifest.error = Some(RunError {
                code: "input.unreadable",
                message,
            });
            finish_manifest(&mut manifest, timer);
            write_json(&manifest_path, &manifest)?;
            Err(error)
        }
    }
}

fn identify_input(supplied_path: &Path, absolute_path: &Path) -> Result<InputIdentity> {
    let metadata = fs::metadata(absolute_path)
        .with_context(|| format!("could not read input {}", supplied_path.display()))?;
    if !metadata.is_file() {
        return Err(anyhow!(
            "input is not a regular file: {}",
            supplied_path.display()
        ));
    }

    let file = File::open(absolute_path)
        .with_context(|| format!("could not open input {}", supplied_path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("could not hash input {}", supplied_path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(InputIdentity {
        path: supplied_path.to_string_lossy().into_owned(),
        absolute_path: absolute_path.to_string_lossy().into_owned(),
        size_bytes: metadata.len(),
        modified_at: metadata.modified().ok().map(format_system_time),
        sha256: format!("{:x}", hasher.finalize()),
    })
}

fn reserve_run(runs_directory: &Path) -> Result<(String, PathBuf)> {
    fs::create_dir_all(runs_directory)
        .with_context(|| format!("could not create {}", runs_directory.display()))?;

    let mut candidate = greatest_run_id(runs_directory)?
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| anyhow!("run ID space is exhausted"))?;
    loop {
        let id = format!("{candidate:0RUN_ID_WIDTH$}");
        let path = runs_directory.join(&id);
        match fs::create_dir(&path) {
            Ok(()) => return Ok((id, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                candidate = candidate
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("run ID space is exhausted"))?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not reserve run directory {}", path.display())
                });
            }
        }
    }
}

pub(crate) fn list(latest: bool) -> Result<Vec<PathBuf>> {
    let working_directory = std::env::current_dir().context("could not read working directory")?;
    let runs_directory = working_directory.join(".baho/runs");
    let mut ids = run_ids(&runs_directory)?;
    if latest {
        ids = ids.into_iter().rev().take(1).collect();
    }

    Ok(ids
        .into_iter()
        .map(|id| PathBuf::from(format!(".baho/runs/{id:0RUN_ID_WIDTH$}")))
        .collect())
}

fn greatest_run_id(runs_directory: &Path) -> Result<Option<u64>> {
    Ok(run_ids(runs_directory)?.into_iter().max())
}

fn run_ids(runs_directory: &Path) -> Result<Vec<u64>> {
    let entries = match fs::read_dir(runs_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not read {}", runs_directory.display()));
        }
    };

    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.context("could not read run directory entry")?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.len() >= RUN_ID_WIDTH && name.bytes().all(|byte| byte.is_ascii_digit()) {
            if let Ok(id) = name.parse() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn finish_manifest(manifest: &mut Manifest, timer: Instant) {
    manifest.finished_at = Some(now());
    manifest.duration_ms = Some(timer.elapsed().as_millis());
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let file =
        File::create(path).with_context(|| format!("could not create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)
        .with_context(|| format!("could not serialize {}", path.display()))?;
    writer.write_all(b"\n")?;
    writer
        .flush()
        .with_context(|| format!("could not flush {}", path.display()))
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
