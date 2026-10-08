//! `synth plan-epic`: the epic planner agent loop (Epic 016, T7).
//!
//! The agent can read the sanitized evaluation and allowlisted repository text, and it can
//! submit one epic through the harness. This module owns everything around that: the sidecar
//! check before any model call, the bounded stream loop, the terminal outcome, and a
//! sanitized record written beside the evaluation. The record never holds file contents,
//! provider payloads, credentials, or source values.

use crate::{
    contracts::EvaluationReport,
    evaluate::SIDECAR_FILE,
    generator::{MODEL, PROVIDER, served_tier},
    planner_tools::{
        FinishNoEpic, GOALS, ListFiles, MAX_EXCERPT_BYTES, PLANNER_PROMPT, PLANNER_PROMPT_VERSION,
        ReadEvaluation, ReadFile, SearchText, ToolLimits, ToolTranscript, Toolbox, bounded_text,
        goals_hash, looks_like_tool_call_markup, planner_prompt_hash,
    },
    redaction::{RedactionError, Redactor, load_sidecar},
    submit_epic::{EpicSubmitter, RejectionRecord, SubmitEpic},
};
use futures::StreamExt;
use rig::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
    HookContext, ModelTurnAction, ModelTurnFinished, MultiTurnStreamItem, OutcomeAction,
    OutcomeEvent,
};
use rig::{AgentBuilder, DynModel, operation::Completion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const RECORD_SCHEMA_VERSION: u32 = 1;
const MAX_TITLES: usize = 200;
const MAX_TITLE_CHARS: usize = 120;
const MAX_TOOL_EVENTS: usize = 2000;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("cannot read the evaluation report: {0}")]
    Evaluation(String),
    #[error("redaction sidecar unavailable or not for this evaluation: {0}")]
    Sidecar(#[from] RedactionError),
    #[error("planning settings exceed supported bounds")]
    Settings,
    #[error("{0}")]
    Context(#[from] crate::context_pack::PackError),
    #[error("{0}")]
    Setup(String),
    #[error("cannot write the plan record: {0}")]
    Record(String),
}

/// `NEURALWATT_API_KEY` from the environment, else from the repository `.env` contents.
pub fn resolve_credential(
    env: Option<String>,
    root_env_contents: Option<&str>,
) -> Result<String, crate::generator::GenerateError> {
    crate::generator::credential(
        env.or_else(|| crate::generator::env_file_value(root_env_contents?, "NEURALWATT_API_KEY")),
    )
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PlanSettings {
    pub max_turns: usize,
    pub per_call_seconds: u64,
    pub whole_run_seconds: u64,
    pub limits: ToolLimits,
}

impl Default for PlanSettings {
    fn default() -> Self {
        Self {
            max_turns: 40,
            per_call_seconds: 900,
            whole_run_seconds: 3600,
            limits: ToolLimits::default(),
        }
    }
}

impl PlanSettings {
    pub fn validate(&self) -> Result<(), PlanError> {
        if (1..=200).contains(&self.max_turns)
            && (1..=7200).contains(&self.per_call_seconds)
            && (1..=86400).contains(&self.whole_run_seconds)
            && self.limits.max_calls >= 1
        {
            Ok(())
        } else {
            Err(PlanError::Settings)
        }
    }
}

pub struct PlanOptions {
    /// Path to `evaluation.json`; the sidecar and the record live in the same directory.
    pub evaluation: PathBuf,
    /// Repository root: source of the read tools and owner of `epics/`.
    pub repo: PathBuf,
    pub settings: PlanSettings,
    /// Name of a context pack under `tools/synth/context/`; `None` injects nothing.
    pub context: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallRecord {
    pub turn: usize,
    pub elapsed_ms: u128,
    pub requested_tier: &'static str,
    pub served_tier: String,
    pub standard_tier: bool,
    pub interrupted: bool,
    /// Provider-reported token usage for this call; absent when the stream reported none
    /// (and in older records).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolEvent {
    pub turn: usize,
    pub tool: String,
    pub event: &'static str,
    pub succeeded: Option<bool>,
    pub argument_bytes: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanRecord {
    pub schema_version: u32,
    pub id: String,
    pub provider: &'static str,
    pub model: &'static str,
    pub route: &'static str,
    pub rig_version: &'static str,
    pub prompt_version: &'static str,
    pub prompt_hash: String,
    pub goals_hash: String,
    pub evaluation_id: String,
    pub evaluation_hash: String,
    pub repo_revision: String,
    pub repo_dirty: bool,
    pub settings: PlanSettings,
    /// `epic_written`, `no_epic` (deliberate `finish_no_epic` call), `malformed_tool_call`
    /// (the stream ended with tool-call markup written as text), `no_terminal_action` (plain
    /// text ended the stream without `submit_epic` or `finish_no_epic`), `max_turns`,
    /// `timeout`, or `provider_error`.
    pub outcome: String,
    pub epic_path: Option<String>,
    /// Bounded and sanitized; only present for `no_epic`, from `finish_no_epic`.
    pub explanation: Option<String>,
    /// Sanitized final text capped at 300 bytes; only for `malformed_tool_call` and
    /// `no_terminal_action`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_text_excerpt: Option<String>,
    /// Present only when a context pack was injected: name, hash, resolved ranges, bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_pack: Option<crate::context_pack::PackRecord>,
    pub calls: Vec<CallRecord>,
    pub tool_events: Vec<ToolEvent>,
    pub tool_transcript: Vec<ToolTranscript>,
    pub tool_bytes: usize,
    pub rejected_submissions: Vec<RejectionRecord>,
    /// Present only for `provider_error`: a classified, sanitized, bounded cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_error: Option<ProviderErrorSummary>,
}

/// Why a provider call failed, without headers, request payloads, or credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderErrorSummary {
    /// `http_status`, `transport`, `decode`, `request`, `timeout`, `tool`, or `other`.
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// Sanitized and capped at [`MAX_PROVIDER_ERROR_BYTES`].
    pub message: String,
}

pub const MAX_PROVIDER_ERROR_BYTES: usize = 300;

fn summarize_provider_error(
    redactor: &Redactor,
    error: &rig::agent::StreamingError,
) -> ProviderErrorSummary {
    use rig::agent::StreamingError as E;
    use rig::error::{ErrorKind, ErrorReport};
    use rig::prelude::PromptError as P;
    let report: Option<ErrorReport> = match error {
        E::Completion(e) | E::Prompt(P::CompletionError(e)) => Some(e.into()),
        E::Report(r) | E::Prompt(P::Report(r)) => Some(r.clone()),
        E::Prompt(_) => None,
    };
    let (class, http_status, raw) = match report {
        Some(r) => {
            let class = if r.http_status.is_some() {
                "http_status"
            } else {
                match r.kind {
                    ErrorKind::Http => "transport",
                    ErrorKind::Json | ErrorKind::Response => "decode",
                    ErrorKind::Url | ErrorKind::Request => "request",
                    ErrorKind::Timeout => "timeout",
                    ErrorKind::Tool(_) => "tool",
                    _ => "other",
                }
            };
            (class, r.http_status, r.message)
        }
        None => ("other", None, error.to_string()),
    };
    let clean = redactor.sanitize(&raw);
    let clean = if redactor.scan(&clean).is_empty() {
        clean
    } else {
        "[withheld]".into()
    };
    let mut end = clean.len().min(MAX_PROVIDER_ERROR_BYTES);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    ProviderErrorSummary {
        class: class.into(),
        http_status,
        message: clean[..end].to_owned(),
    }
}

pub struct PlanResult {
    pub record: PlanRecord,
    pub record_path: PathBuf,
}

#[derive(Default)]
struct Recorder {
    calls: Vec<CallRecord>,
    events: Vec<ToolEvent>,
    active_call: Option<(usize, Instant)>,
}

#[derive(Clone)]
struct Transcript(Arc<Mutex<Recorder>>);

fn known_tool(name: &str) -> &str {
    match name {
        "read_evaluation" | "list_files" | "read_file" | "search_text" | "submit_epic"
        | "finish_no_epic" => name,
        _ => "unknown",
    }
}

impl AgentHook for Transcript {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        self.0.lock().unwrap().active_call = Some((event.turn, Instant::now()));
        CompletionCallAction::Continue
    }
    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        let mut rec = self.0.lock().unwrap();
        let elapsed_ms = rec
            .active_call
            .take()
            .map_or(0, |(_, start)| start.elapsed().as_millis());
        let tier = served_tier(event.raw);
        rec.calls.push(CallRecord {
            turn: event.turn,
            elapsed_ms,
            requested_tier: "flex",
            standard_tier: matches!(tier.as_str(), "standard" | "default"),
            served_tier: tier,
            interrupted: false,
            input_tokens: event.usage.input_tokens,
            output_tokens: event.usage.output_tokens,
            total_tokens: event.usage.total_tokens,
            cached_input_tokens: event.usage.cached_input_tokens,
        });
        ModelTurnAction::Continue
    }
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if let Some(tool) = event.tool_name() {
            let mut rec = self.0.lock().unwrap();
            if rec.events.len() < MAX_TOOL_EVENTS {
                rec.events.push(ToolEvent {
                    turn: event.turn,
                    tool: known_tool(tool).into(),
                    event: "dispatch",
                    succeeded: None,
                    argument_bytes: event.tool_args().map_or(0, str::len),
                });
            }
        }
        DispatchAction::Proceed
    }
    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(tool) = event.tool_name() {
            let mut rec = self.0.lock().unwrap();
            let succeeded =
                event.outcome.is_ok() && event.tool_result().is_some_and(|r| !r.is_error());
            if rec.events.len() < MAX_TOOL_EVENTS {
                rec.events.push(ToolEvent {
                    turn: event.turn,
                    tool: known_tool(tool).into(),
                    event: "outcome",
                    succeeded: Some(succeeded),
                    argument_bytes: event.tool_args().map_or(0, str::len),
                });
            }
        }
        OutcomeAction::Proceed
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// First heading of each numbered epic, sanitized; a title that cannot be made safe is withheld.
fn epic_titles(repo: &Path, redactor: &Redactor) -> Vec<String> {
    let Ok(rd) = fs::read_dir(repo.join("epics")) else {
        return vec![];
    };
    let mut names: Vec<String> = rd
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".md") && n.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .collect();
    names.sort();
    names
        .into_iter()
        .take(MAX_TITLES)
        .map(|name| {
            let title = fs::read_to_string(repo.join("epics").join(&name))
                .ok()
                .and_then(|t| {
                    t.lines()
                        .find(|l| l.starts_with("# "))
                        .map(|l| l[2..].trim().to_owned())
                })
                .unwrap_or_default();
            let clean = redactor.sanitize(&title);
            let clean = if redactor.scan(&clean).is_empty() {
                truncate_chars(&clean, MAX_TITLE_CHARS)
            } else {
                "[withheld]".into()
            };
            format!("- epics/{}: {clean}", redactor.sanitize(&name))
        })
        .collect()
}

fn task_prompt(report: &EvaluationReport, titles: &[String], pack: Option<&str>) -> String {
    // The pack comes first and is identical across runs of one goal: a stable prefix.
    let pack = pack.map_or(String::new(), |p| format!("{p}\n---\n\n"));
    format!(
        "{pack}Plan one epic from evaluation {} (generation {}), {} cases.\n\n\
         High-level goals:\n{GOALS}\n\nExisting epics (read them with read_file):\n{}\n\n\
         Start with read_evaluation. Finish only by calling submit_epic (one epic) or \
         finish_no_epic (brief reason the evidence is insufficient). Never write tool calls \
         as text.",
        report.evaluation_id,
        report.generation_id,
        report.cases.len(),
        titles.join("\n")
    )
}

pub async fn plan_epic(
    model: impl Into<DynModel<Completion>>,
    options: &PlanOptions,
) -> Result<PlanResult, PlanError> {
    options.settings.validate()?;
    let bytes =
        fs::read(&options.evaluation).map_err(|e| PlanError::Evaluation(e.kind().to_string()))?;
    let evaluation_hash = format!("{:x}", Sha256::digest(&bytes));
    let report: EvaluationReport = serde_json::from_slice(&bytes)
        .map_err(|_| PlanError::Evaluation("not a valid evaluation report".into()))?;
    report
        .check_version()
        .map_err(|e| PlanError::Evaluation(e.to_string()))?;
    let dir = options
        .evaluation
        .parent()
        .ok_or_else(|| PlanError::Evaluation("evaluation path has no directory".into()))?;
    // Fails before any model call when the sidecar is missing or for another evaluation.
    let redactor = load_sidecar(&dir.join(SIDECAR_FILE), &evaluation_hash)?;
    let toolbox = Toolbox::new(
        &options.repo,
        &report,
        redactor.clone(),
        options.settings.limits,
    )
    .map_err(|e| PlanError::Setup(e.to_string()))?;
    let submitter = EpicSubmitter::new(&options.repo, &report, redactor.clone())
        .ok_or_else(|| PlanError::Setup("repository root is not usable".into()))?;
    let repo_state = crate::cli_run::RepoState::capture(&options.repo).ok();

    let id = format!(
        "plan-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id()
    );
    let finisher = FinishNoEpic::new(redactor.clone());
    let pack = options
        .context
        .as_deref()
        .map(|name| {
            crate::context_pack::load(&options.repo.join("tools/synth/context"), name, &toolbox)
        })
        .transpose()?;
    let prompt = task_prompt(
        &report,
        &epic_titles(&options.repo, &redactor),
        pack.as_ref().map(|p| p.text.as_str()),
    );
    let recorder = Arc::new(Mutex::new(Recorder::default()));
    let agent = AgentBuilder::new(model)
        .preamble(PLANNER_PROMPT)
        .temperature(0.3)
        .max_tokens(16384)
        .add_hook(Transcript(recorder.clone()))
        .tool(ReadEvaluation(toolbox.clone()))
        .tool(ListFiles(toolbox.clone()))
        .tool(ReadFile(toolbox.clone()))
        .tool(SearchText(toolbox.clone()))
        .tool(SubmitEpic(submitter.clone()))
        .tool(finisher.clone())
        .build();

    let settings = options.settings;
    let mut stream = agent.prompt(prompt).max_turns(settings.max_turns).stream();
    let whole_deadline = Instant::now() + Duration::from_secs(settings.whole_run_seconds);
    let mut final_text = String::new();
    let mut provider_error = None;
    let outcome = loop {
        // Terminal as soon as a submission is accepted; nothing later may change that.
        if submitter.accepted().is_some() {
            break "epic_written";
        }
        if finisher.explanation().is_some() {
            break "no_epic";
        }
        let call_deadline = recorder
            .lock()
            .unwrap()
            .active_call
            .map(|(_, t)| t + Duration::from_secs(settings.per_call_seconds))
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(settings.per_call_seconds));
        let deadline = whole_deadline.min(call_deadline);
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), stream.next()).await
        {
            Err(_) => break "timeout",
            Ok(None) => {
                // The stream ended on its own. Only a recorded `finish_no_epic` is deliberate;
                // anything else is classified from the final text. No re-prompt: rig's stream
                // owns the history and exposes no cheap way to continue it after completion.
                break if finisher.explanation().is_some() {
                    "no_epic"
                } else if looks_like_tool_call_markup(&final_text) {
                    "malformed_tool_call"
                } else {
                    "no_terminal_action"
                };
            }
            Ok(Some(Err(rig::agent::StreamingError::Prompt(
                rig::prelude::PromptError::MaxTurnsError { .. },
            )))) => break "max_turns",
            Ok(Some(Err(error))) => {
                provider_error = Some(summarize_provider_error(&redactor, &error));
                break "provider_error";
            }
            Ok(Some(Ok(MultiTurnStreamItem::FinalResponse(r)))) => {
                final_text = r.output;
            }
            Ok(Some(Ok(_))) => {}
        }
    };
    drop(stream);
    // A committed epic outranks any later failure.
    let outcome = if submitter.accepted().is_some() {
        "epic_written"
    } else {
        outcome
    };
    let accepted = submitter.accepted();
    let mut rec = recorder.lock().unwrap();
    if let Some((turn, start)) = rec.active_call.take() {
        rec.calls.push(CallRecord {
            turn,
            elapsed_ms: start.elapsed().as_millis(),
            requested_tier: "flex",
            served_tier: "unknown".into(),
            standard_tier: false,
            interrupted: true,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cached_input_tokens: None,
        });
    }
    let record = PlanRecord {
        schema_version: RECORD_SCHEMA_VERSION,
        id: id.clone(),
        provider: PROVIDER,
        model: MODEL,
        route: "https://api.neuralwatt.com/v1/chat/completions",
        rig_version: "0.43.0",
        prompt_version: PLANNER_PROMPT_VERSION,
        prompt_hash: planner_prompt_hash(),
        goals_hash: goals_hash(),
        evaluation_id: report.evaluation_id.clone(),
        evaluation_hash,
        repo_revision: repo_state
            .as_ref()
            .map_or("unknown".into(), |s| s.revision.clone()),
        repo_dirty: repo_state.as_ref().is_none_or(|s| s.dirty),
        settings,
        outcome: outcome.into(),
        epic_path: accepted.map(|a| a.relative_path),
        explanation: finisher.explanation().filter(|_| outcome == "no_epic"),
        final_text_excerpt: matches!(outcome, "malformed_tool_call" | "no_terminal_action")
            .then(|| bounded_text(&redactor, &final_text, MAX_EXCERPT_BYTES)),
        context_pack: pack.map(|p| p.record),
        calls: rec.calls.clone(),
        tool_events: rec.events.clone(),
        tool_transcript: toolbox.transcript(),
        tool_bytes: toolbox.used_bytes(),
        rejected_submissions: submitter.rejections(),
        provider_error: provider_error.filter(|_| outcome == "provider_error"),
    };
    drop(rec);
    let record_path = write_record(dir, &record)?;
    Ok(PlanResult {
        record,
        record_path,
    })
}

fn write_record(dir: &Path, record: &PlanRecord) -> Result<PathBuf, PlanError> {
    let path = dir.join(format!("{}.json", record.id));
    let bytes = serde_json::to_vec_pretty(record).map_err(|e| PlanError::Record(e.to_string()))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| PlanError::Record(e.kind().to_string()))?;
    file.write_all(&bytes)
        .map_err(|e| PlanError::Record(e.kind().to_string()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{contracts::*, redaction::write_sidecar};
    use rig::completion::Usage;
    use rig::providers::openai::OpenAIConfig;
    use rig::test_utils::{MockCompletionModel, MockStreamEvent, mock_final};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    const GEN: &str = "generation-1791425654630970000-3920";
    const EVAL: &str = "evaluation-1791425654630970000-3920-0";
    const SECRET: &str = "Senior Analyst";

    fn report() -> EvaluationReport {
        let case = |i: usize| CaseReport {
            case_id: format!("{GEN}-{i:04}"),
            candidate_hash: "ab".repeat(32),
            prompt: "Show rows where Role = Zz".into(),
            review: ReviewStatus::Unreviewed,
            review_rationale: None,
            observed: ObservedResult::Refused,
            plan_comparison: PlanComparison::Unavailable,
            diagnostic_codes: vec![],
            row_counts: None,
            evidence: CaseEvidence::default(),
        };
        EvaluationReport {
            schema_version: REPORT_SCHEMA_VERSION,
            evaluation_id: EVAL.into(),
            generation_id: GEN.into(),
            generation_hash: "g".into(),
            candidates_hash: "c".into(),
            metadata_hash: "m".into(),
            dataset_hash: "d".into(),
            provenance: ToolProvenance::default(),
            aggregates: Aggregates::default(),
            limitations: vec![],
            notes: vec![],
            cases: (0..3).map(case).collect(),
        }
    }

    struct Fixture {
        _dir: TempDir,
        repo: PathBuf,
        options: PlanOptions,
        eval_dir: PathBuf,
    }

    fn fixture(with_sidecar: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let w = |rel: &str, body: &str| {
            let p = repo.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        };
        w("epics/001-a.md", "# Epic 001: About Senior Analyst\n");
        w("crates/a/src/lib.rs", "fn x() {}\nfn y() {}\nfn z() {}\n");
        w(".env", "NEURALWATT_API_KEY=leak\n");
        let eval_dir = dir.path().join("synthetic/out/evaluation-x");
        fs::create_dir_all(&eval_dir).unwrap();
        let bytes = serde_json::to_vec_pretty(&report()).unwrap();
        let evaluation = eval_dir.join("evaluation.json");
        fs::write(&evaluation, &bytes).unwrap();
        if with_sidecar {
            let hash = format!("{:x}", Sha256::digest(&bytes));
            let redactor = Redactor::new([SECRET]).unwrap();
            write_sidecar(&eval_dir.join(SIDECAR_FILE), &redactor.to_sidecar(&hash)).unwrap();
        }
        Fixture {
            options: PlanOptions {
                evaluation,
                repo: repo.clone(),
                settings: PlanSettings::default(),
                context: None,
            },
            repo,
            eval_dir,
            _dir: dir,
        }
    }

    fn turn(name: &str, args: Value) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::tool_call("call", name, args),
            MockStreamEvent::FinalResponse(mock_final(Usage::default())),
        ]
    }
    fn text_turn(text: &str) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::text(text),
            MockStreamEvent::FinalResponse(mock_final(Usage::default())),
        ]
    }

    fn doc() -> String {
        format!(
            "# Parse refusals for polite lead-ins\n\nStatus: Proposed\n\n\
## Motivating evidence and user intent\nRun `{EVAL}` from `{GEN}`; case {GEN}-0001 is unreviewed.\n\n\
## Observed behavior\nRefused (`crates/a/src/lib.rs:1`, `crates/a/src/lib.rs:2`, `crates/a/src/lib.rs:3`).\n\n## Problem and desired behavior\nx\n\n## Out of scope\nx\n\n\
## Architecture and crate ownership\nx\n\n## Design and alternatives\nChosen increment: normalize lead-ins.\n\n\
## Goal coverage\n\n| Goal | Decision | Reason |\n|---|---|---|\n{}\n\
## Acceptance criteria\nx\n\n## Fixture and test strategy\nx\n\n## Risks and regressions\nx\n\n## Implementation outline\nx\n\n\
## Path inventory\n\nExisting evidence files:\n- `crates/a/src/lib.rs`\n\nProposed new files:\n- `crates/a/tests/lead_in.rs`\n\n\
## Unresolved questions\nNone.\n",
            (1..=8)
                .map(|g| format!("| {g} | not affected | reason {g} |\n"))
                .collect::<String>()
        )
    }

    fn submit(markdown: &str) -> Vec<MockStreamEvent> {
        turn(
            "submit_epic",
            json!({"slug":"polite-lead-ins","markdown":markdown}),
        )
    }

    fn epics(f: &Fixture) -> usize {
        fs::read_dir(f.repo.join("epics")).unwrap().count()
    }

    #[tokio::test]
    async fn reads_then_submits_and_stops_with_epic_written() {
        let f = fixture(true);
        let model = MockCompletionModel::from_stream_turns([
            turn("read_evaluation", json!({})),
            turn("read_file", json!({"path":"crates/a/src/lib.rs"})),
            submit(&doc()),
            // Must never be requested: the loop stops once the submission is accepted.
            text_turn("unreachable"),
        ]);
        let capture = model.clone();
        let result = plan_epic(model, &f.options).await.unwrap();
        let r = &result.record;
        assert_eq!(r.outcome, "epic_written");
        assert_eq!(r.epic_path.as_deref(), Some("epics/002-polite-lead-ins.md"));
        assert!(f.repo.join("epics/002-polite-lead-ins.md").is_file());
        assert_eq!(epics(&f), 2);
        assert_eq!(r.calls.len(), 3);
        assert_eq!(r.model, "deepseek-v4.1-flash-flex");
        assert!(r.calls.iter().all(|c| c.requested_tier == "flex"));
        let names: Vec<_> = r.tool_transcript.iter().map(|t| t.tool.as_str()).collect();
        assert_eq!(names, ["read_evaluation", "read_file"]);
        assert!(r.tool_events.iter().any(|e| e.tool == "submit_epic"));
        assert!(result.record_path.starts_with(&f.eval_dir));
        // Titles reach the model only in sanitized form; the secret never does.
        let requests = format!("{:?}", capture.requests());
        assert!(!requests.contains(SECRET));
        assert!(requests.contains("epics/001-a.md"));
        let saved = fs::read_to_string(&result.record_path).unwrap();
        for private in [SECRET, "leak", "NEURALWATT_API_KEY=", "fn x()"] {
            assert!(!saved.contains(private), "{private}");
        }
    }

    #[tokio::test]
    async fn per_call_token_usage_is_recorded_when_reported() {
        let f = fixture(true);
        let usage = Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            total_tokens: Some(120),
            cached_input_tokens: Some(40),
            ..Usage::default()
        };
        let reported = vec![
            MockStreamEvent::tool_call("call", "read_evaluation", json!({})),
            MockStreamEvent::FinalResponse(mock_final(usage)),
        ];
        let model = MockCompletionModel::from_stream_turns([reported, submit(&doc())]);
        let result = plan_epic(model, &f.options).await.unwrap();
        let calls = &result.record.calls;
        assert_eq!(calls[0].input_tokens, Some(100));
        assert_eq!(calls[0].output_tokens, Some(20));
        assert_eq!(calls[0].total_tokens, Some(120));
        assert_eq!(calls[0].cached_input_tokens, Some(40));
        // A call without reported usage records nothing rather than zeros.
        assert_eq!(calls[1].input_tokens, None);
        let saved: Value = serde_json::from_slice(&fs::read(&result.record_path).unwrap()).unwrap();
        assert_eq!(saved["calls"][0]["input_tokens"], 100);
        assert!(saved["calls"][1].get("input_tokens").is_none());
    }

    mod pack {
        use super::*;
        use crate::context_pack::{self, MAX_PACK_BYTES, PackError};

        fn toolbox(f: &Fixture) -> Toolbox {
            let redactor = Redactor::new([SECRET]).unwrap();
            Toolbox::new(&f.repo, &report(), redactor, ToolLimits::default()).unwrap()
        }

        fn manifest(f: &Fixture, name: &str, entries: Value) {
            let dir = f.repo.join("tools/synth/context");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("{name}.json")),
                json!({ "entries": entries }).to_string(),
            )
            .unwrap();
        }

        fn load(f: &Fixture, name: &str) -> Result<context_pack::ContextPack, PackError> {
            context_pack::load(&f.repo.join("tools/synth/context"), name, &toolbox(f))
        }

        #[test]
        fn anchors_and_ranges_resolve_to_numbered_lines() {
            let f = fixture(true);
            manifest(
                &f,
                "p",
                json!([
                    {"path":"crates/a/src/lib.rs","anchor":"fn y","lines_before":1,"lines_after":5},
                    {"path":"crates/a/src/lib.rs","start_line":3,"end_line":3}
                ]),
            );
            let pack = load(&f, "p").unwrap();
            let r = &pack.record;
            let ranges: Vec<_> = r
                .entries
                .iter()
                .map(|e| (e.path.as_str(), e.start_line, e.end_line))
                .collect();
            // The anchor range is clamped to the file's three lines.
            assert_eq!(
                ranges,
                [("crates/a/src/lib.rs", 1, 3), ("crates/a/src/lib.rs", 3, 3)]
            );
            assert!(
                pack.text
                    .contains("### crates/a/src/lib.rs:1-3\n1: fn x() {}\n2: fn y() {}")
            );
            assert_eq!(r.bytes, pack.text.len());
            assert_eq!(r.hash.len(), 64);
            // Same tree, same pack: a stable prefix.
            assert_eq!(load(&f, "p").unwrap().record.hash, r.hash);
        }

        #[test]
        fn unresolved_ambiguous_and_malformed_entries_fail_loudly() {
            let f = fixture(true);
            for (entry, needle) in [
                (
                    json!({"path":"crates/a/src/lib.rs","anchor":"fn nope"}),
                    "does not resolve",
                ),
                (
                    json!({"path":"crates/a/src/lib.rs","anchor":"fn "}),
                    "ambiguous",
                ),
                (json!({"path":"crates/a/src/lib.rs"}), "either an anchor"),
                (
                    json!({"path":"crates/a/src/lib.rs","start_line":9,"end_line":9}),
                    "beyond",
                ),
            ] {
                manifest(&f, "p", json!([entry]));
                let error = load(&f, "p").err().unwrap().to_string();
                assert!(error.contains(needle), "{error}");
            }
            assert!(matches!(load(&f, "absent"), Err(PackError::Missing(_))));
            assert_eq!(load(&f, "../p").err(), Some(PackError::Name));
        }

        #[test]
        fn allowlist_and_denylist_apply_to_pack_paths() {
            let f = fixture(true);
            fs::create_dir_all(f.repo.join("synthetic/out")).unwrap();
            fs::write(f.repo.join("synthetic/out/x.txt"), "x\n").unwrap();
            for path in [
                ".env",
                "synthetic/out/x.txt",
                "../outside",
                "/etc/passwd",
                "Cargo.lock",
            ] {
                manifest(&f, "p", json!([{"path":path,"start_line":1,"end_line":1}]));
                let error = load(&f, "p").err().unwrap().to_string();
                assert!(
                    error.contains("path not allowed") || error.contains("not found"),
                    "{path}: {error}"
                );
                assert!(!error.contains("leak"));
            }
        }

        #[test]
        fn oversized_pack_is_rejected() {
            let f = fixture(true);
            let body: String = (0..1500).map(|i| format!("// {i:0>60}\n")).collect();
            fs::write(f.repo.join("crates/a/src/big.rs"), body).unwrap();
            manifest(
                &f,
                "p",
                json!([{"path":"crates/a/src/big.rs","start_line":1,"end_line":1500}]),
            );
            match load(&f, "p") {
                Err(PackError::TooLarge { bytes }) => assert!(bytes > MAX_PACK_BYTES),
                other => panic!("{:?}", other.err()),
            }
        }

        #[test]
        fn pack_content_is_redacted() {
            let f = fixture(true);
            fs::write(
                f.repo.join("crates/a/src/s.rs"),
                format!("// the {SECRET} role\nfn s() {{}}\n"),
            )
            .unwrap();
            manifest(
                &f,
                "p",
                json!([{"path":"crates/a/src/s.rs","start_line":1,"end_line":2}]),
            );
            let pack = load(&f, "p").unwrap();
            assert!(!pack.text.contains(SECRET));
            assert!(pack.text.contains("2: fn s() {}"));
        }

        #[tokio::test]
        async fn pack_leads_the_first_message_and_is_recorded() {
            let f = fixture(true);
            manifest(
                &f,
                "p",
                json!([{"path":"crates/a/src/lib.rs","anchor":"fn y","lines_before":1,"lines_after":1}]),
            );
            let mut options = PlanOptions {
                evaluation: f.options.evaluation.clone(),
                repo: f.repo.clone(),
                settings: PlanSettings::default(),
                context: Some("p".into()),
            };
            let model = MockCompletionModel::from_stream_turns([submit(&doc())]);
            let capture = model.clone();
            let result = plan_epic(model, &options).await.unwrap();
            let requests = format!("{:?}", capture.requests());
            let pack_at = requests.find("Context pack").expect("pack present");
            let task_at = requests.find("Plan one epic").expect("task present");
            assert!(pack_at < task_at);
            assert!(requests.contains("2: fn y() {}"));
            let recorded = result.record.context_pack.as_ref().unwrap();
            assert_eq!(recorded.name, "p");
            assert_eq!(recorded.entries[0].start_line, 1);
            let saved = fs::read_to_string(&result.record_path).unwrap();
            assert!(saved.contains("\"context_pack\""));
            assert!(!saved.contains("fn y()"));

            // A bad pack fails before the model is called at all.
            options.context = Some("absent".into());
            let model = MockCompletionModel::from_stream_turns([submit(&doc())]);
            let capture = model.clone();
            assert!(matches!(
                plan_epic(model, &options).await,
                Err(PlanError::Context(_))
            ));
            assert!(capture.requests().is_empty());
        }
    }

    #[tokio::test]
    async fn rejected_submission_is_corrected_and_resubmitted() {
        let f = fixture(true);
        let model =
            MockCompletionModel::from_stream_turns([submit("# Not an epic\n"), submit(&doc())]);
        let result = plan_epic(model, &f.options).await.unwrap();
        assert_eq!(result.record.outcome, "epic_written");
        assert_eq!(result.record.rejected_submissions.len(), 1);
        assert!(!result.record.rejected_submissions[0].codes.is_empty());
        assert_eq!(epics(&f), 2);
    }

    #[tokio::test]
    async fn finish_no_epic_ends_as_bounded_sanitized_no_epic() {
        let f = fixture(true);
        let long = format!(
            "Evidence is thin about Senior Analyst. {}",
            "x".repeat(5000)
        );
        let model = MockCompletionModel::from_stream_turns([
            turn("read_evaluation", json!({})),
            turn("finish_no_epic", json!({"explanation": long})),
            text_turn("unreachable"),
        ]);
        let result = plan_epic(model, &f.options).await.unwrap();
        let r = &result.record;
        assert_eq!(r.outcome, "no_epic");
        assert!(r.epic_path.is_none());
        assert!(r.final_text_excerpt.is_none());
        let why = r.explanation.as_deref().unwrap();
        assert!(why.starts_with("Evidence is thin"));
        assert!(why.len() <= 1000);
        assert!(!why.contains(SECRET));
        assert_eq!(epics(&f), 1);
    }

    #[tokio::test]
    async fn leaked_tool_call_markup_is_malformed_not_no_epic() {
        let f = fixture(true);
        let leaked = format!(
            "I will inspect {SECRET}.\n<\u{ff5c}DSML\u{ff5c} invoke name=\"read_file\">\n\
             <\u{ff5c}DSML\u{ff5c} parameter name=\"path\" string=\"true\">crates/a{}",
            "y".repeat(2000)
        );
        let model = MockCompletionModel::from_stream_turns([
            turn("read_evaluation", json!({})),
            text_turn(&leaked),
        ]);
        let result = plan_epic(model, &f.options).await.unwrap();
        let r = &result.record;
        assert_eq!(r.outcome, "malformed_tool_call");
        assert!(r.explanation.is_none());
        let excerpt = r.final_text_excerpt.as_deref().unwrap();
        assert!(excerpt.len() <= 300);
        assert!(excerpt.contains("DSML"));
        assert!(!excerpt.contains(SECRET));
        let saved = fs::read_to_string(&result.record_path).unwrap();
        assert!(!saved.contains(SECRET));
        assert_eq!(epics(&f), 1);
    }

    #[tokio::test]
    async fn plain_prose_without_terminal_tool_is_no_terminal_action() {
        let f = fixture(true);
        let model = MockCompletionModel::from_stream_turns([
            turn("read_evaluation", json!({})),
            text_turn(&format!("Evidence is thin about {SECRET}.")),
        ]);
        let result = plan_epic(model, &f.options).await.unwrap();
        let r = &result.record;
        assert_eq!(r.outcome, "no_terminal_action");
        assert!(r.explanation.is_none());
        let excerpt = r.final_text_excerpt.as_deref().unwrap();
        assert!(excerpt.starts_with("Evidence is thin"));
        assert!(!excerpt.contains(SECRET));
    }

    #[tokio::test]
    async fn accepted_submission_wins_over_leaked_markup_text() {
        let f = fixture(true);
        let model = MockCompletionModel::from_stream_turns([
            submit(&doc()),
            text_turn("<tool_call> never reached"),
        ]);
        let result = plan_epic(model, &f.options).await.unwrap();
        assert_eq!(result.record.outcome, "epic_written");
        assert!(result.record.final_text_excerpt.is_none());
        assert!(result.record.explanation.is_none());
    }

    #[tokio::test]
    async fn forbidden_reads_are_refused_and_recorded_without_content() {
        let f = fixture(true);
        let model = MockCompletionModel::from_stream_turns([
            turn("read_file", json!({"path":".env"})),
            turn("read_file", json!({"path":"../outside.rs"})),
            turn(
                "read_file",
                json!({"path":"synthetic/out/evaluation-x/evaluation.json"}),
            ),
            text_turn("cannot proceed"),
        ]);
        let result = plan_epic(model, &f.options).await.unwrap();
        let r = &result.record;
        assert_eq!(r.outcome, "no_terminal_action");
        assert_eq!(r.tool_transcript.len(), 3);
        assert!(r.tool_transcript.iter().all(|t| t.outcome == "rejected"));
        assert!(r.tool_events.iter().any(|e| e.succeeded == Some(false)));
        assert!(
            !fs::read_to_string(&result.record_path)
                .unwrap()
                .contains("leak")
        );
    }

    #[tokio::test]
    async fn turn_exhaustion_is_a_recorded_outcome_without_an_epic() {
        let mut f = fixture(true);
        f.options.settings.max_turns = 2;
        let model = MockCompletionModel::from_stream_turns([
            turn("read_evaluation", json!({})),
            turn("read_evaluation", json!({"section":"cases"})),
            turn("read_evaluation", json!({})),
        ]);
        let result = plan_epic(model, &f.options).await.unwrap();
        assert_eq!(result.record.outcome, "max_turns");
        assert!(result.record.epic_path.is_none());
        assert_eq!(epics(&f), 1);
        assert!(result.record_path.is_file());
    }

    #[tokio::test]
    async fn missing_or_mismatched_sidecar_fails_before_any_model_call() {
        let f = fixture(false);
        let model = MockCompletionModel::from_stream_turns([text_turn("never")]);
        let capture = model.clone();
        let err = plan_epic(model, &f.options).await.err().unwrap();
        assert!(matches!(err, PlanError::Sidecar(_)));
        assert!(capture.requests().is_empty());

        let f = fixture(true);
        fs::write(&f.options.evaluation, b"{\"changed\":true}").unwrap();
        let model = MockCompletionModel::from_stream_turns([text_turn("never")]);
        let capture = model.clone();
        assert!(plan_epic(model, &f.options).await.is_err());
        assert!(capture.requests().is_empty());
        // No record is written for a run that never started.
        assert!(fs::read_dir(&f.eval_dir).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("plan-")
        }));
    }

    #[test]
    fn credential_comes_from_environment_then_env_file() {
        assert!(
            resolve_credential(None, None)
                .unwrap_err()
                .to_string()
                .contains("NEURALWATT_API_KEY")
        );
        assert_eq!(
            resolve_credential(None, Some("NEURALWATT_API_KEY=file")).unwrap(),
            "file"
        );
        assert_eq!(
            resolve_credential(Some("env".into()), Some("NEURALWATT_API_KEY=file")).unwrap(),
            "env"
        );
        assert!(resolve_credential(Some(" ".into()), None).is_err());
    }

    /// A one-shot local server: reads a request, then either hangs or answers with an error.
    fn server(respond: Option<&'static str>) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for connection in listener.incoming().take(4) {
                let Ok(mut c) = connection else { continue };
                let mut buf = [0u8; 8192];
                let _ = c.set_read_timeout(Some(Duration::from_millis(300)));
                let _ = c.read(&mut buf);
                match respond {
                    Some(r) => {
                        let _ = c.write_all(r.as_bytes());
                    }
                    None => std::thread::sleep(Duration::from_secs(4)),
                }
            }
        });
        address
    }

    fn http_model(address: std::net::SocketAddr) -> impl Into<DynModel<Completion>> {
        OpenAIConfig::new("offline-fixture-secret")
            .with_base_url(format!("http://{address}/v1"))
            .client()
            .chat(MODEL)
    }

    #[tokio::test]
    async fn unresponsive_provider_times_out_with_interrupted_unknown_tier() {
        let mut f = fixture(true);
        f.options.settings.per_call_seconds = 1;
        let result = plan_epic(http_model(server(None)), &f.options)
            .await
            .unwrap();
        assert_eq!(result.record.outcome, "timeout");
        let last = result.record.calls.last().unwrap();
        assert!(last.interrupted);
        assert_eq!(last.served_tier, "unknown");
        assert!(
            !fs::read_to_string(&result.record_path)
                .unwrap()
                .contains("offline-fixture-secret")
        );
    }

    #[tokio::test]
    async fn provider_failure_is_recorded_as_provider_error() {
        let f = fixture(true);
        let address = server(Some(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ));
        let result = plan_epic(http_model(address), &f.options).await.unwrap();
        assert_eq!(result.record.outcome, "provider_error");
        assert!(result.record.epic_path.is_none());
        let summary = result.record.provider_error.as_ref().expect("summary");
        assert_eq!(summary.class, "http_status");
        assert_eq!(summary.http_status, Some(500));
        assert!(summary.message.len() <= MAX_PROVIDER_ERROR_BYTES);
        let text = fs::read_to_string(&result.record_path).unwrap();
        assert!(!text.contains("offline-fixture-secret"));
    }

    #[tokio::test]
    async fn provider_error_summary_is_scrubbed_and_bounded() {
        let f = fixture(true);
        let body = format!(
            "{{\"error\":\"bad key sk-abcdefghijklmnopqrstuvwxyz0123456789 at /Users/someone/proj {}\"}}",
            "x".repeat(900)
        );
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let result = plan_epic(http_model(server(Some(response))), &f.options)
            .await
            .unwrap();
        let summary = result.record.provider_error.as_ref().expect("summary");
        assert_eq!(summary.http_status, Some(400));
        assert!(summary.message.len() <= MAX_PROVIDER_ERROR_BYTES);
        let text = fs::read_to_string(&result.record_path).unwrap();
        for leaked in ["sk-abc", "/Users/someone", "offline-fixture-secret"] {
            assert!(!text.contains(leaked), "{leaked}");
        }
    }

    #[tokio::test]
    async fn non_provider_outcomes_carry_no_provider_error() {
        let f = fixture(true);
        let model = MockCompletionModel::from_stream_turns([text_turn("nothing to propose")]);
        let result = plan_epic(model, &f.options).await.unwrap();
        assert!(result.record.provider_error.is_none());
        let text = fs::read_to_string(&result.record_path).unwrap();
        assert!(!text.contains("provider_error\":"));
    }

    #[tokio::test]
    async fn unreported_tier_is_visible_as_unknown_without_retry() {
        let f = fixture(true);
        let model = MockCompletionModel::from_stream_turns([text_turn("nothing to propose")]);
        let result = plan_epic(model, &f.options).await.unwrap();
        // The mock reports no tier: it must show as unknown rather than being assumed flex.
        assert_eq!(result.record.calls.len(), 1);
        assert_eq!(result.record.calls[0].served_tier, "unknown");
        assert_eq!(result.record.calls[0].requested_tier, "flex");
    }
}
