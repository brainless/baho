//! `synth plan-epic`: the epic planner agent loop (Epic 016, T7).
//!
//! The agent can read the sanitized evaluation and allowlisted repository text, and it can
//! submit one epic through the harness. This module owns everything around that: the sidecar
//! check before any model call, the bounded stream loop, the terminal outcome, and a
//! sanitized record written beside the evaluation. The record never holds file contents,
//! provider payloads, credentials, or source values.

use crate::{
    contracts::EvaluationReport,
    draft_review::{DraftProposal, Review, ReviewVerdict, SubmitDraft, SubmitReview, draft_hash},
    evaluate::SIDECAR_FILE,
    generator::{PROVIDER, served_tier},
    investigation::{Investigation, SubmitInvestigation},
    planner_tools::{
        FinishNoEpic, GOALS, ListFiles, MAX_EXCERPT_BYTES, PLANNER_PROMPT, PLANNER_PROMPT_VERSION,
        ReadEvaluation, ReadFile, SearchText, ToolLimits, ToolTranscript, Toolbox, bounded_text,
        goals_hash, looks_like_tool_call_markup, planner_prompt_hash,
    },
    redaction::{RedactionError, Redactor, load_sidecar},
    submit_epic::{EpicSubmitter, ProposalEvidence, RejectionRecord},
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

pub const MODEL: &str = "glm-5.3-flex";
pub const RECORD_SCHEMA_VERSION: u32 = 3;
const INVESTIGATION_PROMPT: &str = include_str!("../prompts/planner-investigation.md");
const REVIEW_PROMPT: &str = include_str!("../prompts/planner-review.md");
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

/// Requested provider reasoning level; recorded so fixed-evaluation comparisons are reproducible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    #[default]
    High,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => Err("reasoning effort must be low, medium, or high"),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct StageBudget {
    pub max_turns: usize,
    pub max_tool_calls: usize,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct StageBudgets {
    pub investigation: StageBudget,
    pub draft: StageBudget,
    pub review: StageBudget,
}

impl Default for StageBudgets {
    fn default() -> Self {
        Self {
            investigation: StageBudget {
                max_turns: 40,
                max_tool_calls: 60,
            },
            draft: StageBudget {
                max_turns: 25,
                max_tool_calls: 40,
            },
            review: StageBudget {
                max_turns: 20,
                max_tool_calls: 30,
            },
        }
    }
}

impl StageBudgets {
    fn for_stage(self, stage: &str) -> StageBudget {
        match stage {
            "investigate" => self.investigation,
            "draft" | "revise" => self.draft,
            "review" | "final_review" => self.review,
            _ => unreachable!("stage runner only executes known stages"),
        }
    }

    fn valid(self) -> bool {
        [self.investigation, self.draft, self.review]
            .into_iter()
            .all(|budget| {
                (1..=200).contains(&budget.max_turns) && (1..=1000).contains(&budget.max_tool_calls)
            })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PlanSettings {
    pub stage_budgets: StageBudgets,
    pub reasoning_effort: ReasoningEffort,
    pub max_turns: usize,
    pub per_call_seconds: u64,
    pub whole_run_seconds: u64,
    pub limits: ToolLimits,
}

impl Default for PlanSettings {
    fn default() -> Self {
        Self {
            stage_budgets: StageBudgets::default(),
            reasoning_effort: ReasoningEffort::High,
            max_turns: 120,
            per_call_seconds: 900,
            whole_run_seconds: 3600,
            limits: ToolLimits {
                max_calls: 180,
                ..ToolLimits::default()
            },
        }
    }
}

impl PlanSettings {
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.stage_budgets.valid()
            && (1..=200).contains(&self.max_turns)
            && (1..=7200).contains(&self.per_call_seconds)
            && (1..=86400).contains(&self.whole_run_seconds)
            && (1..=1000).contains(&self.limits.max_calls)
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
    /// Resume writing from a previously saved, validated investigative proposal.
    pub proposal: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CallRecord {
    pub stage: String,
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
    pub stage: String,
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
    pub stages: Vec<StageRecord>,
    pub investigation: Option<Investigation>,
    pub proposal_path: Option<String>,
    pub resumed_from_proposal: bool,
    pub reviews: Vec<Review>,
    /// `epic_written`, `no_epic` (deliberate `finish_no_epic` call), `malformed_tool_call`
    /// (the stream ended with tool-call markup written as text), `no_terminal_action` (plain
    /// text ended the stream without `submit_epic` or `finish_no_epic`), `max_turns`,
    /// `timeout`, `provider_error`, `review_failed`, `draft_scope_mismatch`, or `publication_failed`.
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

/// One bounded phase, with its independent history and prompt provenance.
#[derive(Clone, Debug, Serialize)]
pub struct StageRecord {
    pub stage: String,
    pub outcome: String,
    pub prompt_hash: String,
    pub first_turn: usize,
    pub last_turn: usize,
    pub draft_hash: Option<String>,
    pub terminal_rejections: Vec<String>,
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
    stage: String,
    tool_calls: usize,
    max_tool_calls: usize,
    tool_budget_exhausted: bool,
    stage_tool_calls: usize,
    stage_max_tool_calls: usize,
    stage_tool_budget_exhausted: bool,
}

#[derive(Clone)]
struct Transcript(Arc<Mutex<Recorder>>);

fn known_tool(name: &str) -> &str {
    match name {
        "read_evaluation"
        | "list_files"
        | "read_file"
        | "search_text"
        | "submit_epic"
        | "finish_no_epic"
        | "submit_investigation"
        | "submit_draft"
        | "submit_review" => name,
        _ => "unknown",
    }
}

impl AgentHook for Transcript {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        _: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        let mut rec = self.0.lock().unwrap();
        let turn = rec.calls.len() + 1;
        rec.active_call = Some((turn, Instant::now()));
        CompletionCallAction::Continue
    }
    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        let mut rec = self.0.lock().unwrap();
        let (turn, elapsed_ms) = rec
            .active_call
            .take()
            .map_or((rec.calls.len() + 1, 0), |(turn, start)| {
                (turn, start.elapsed().as_millis())
            });
        let stage = rec.stage.clone();
        let tier = served_tier(event.raw);
        rec.calls.push(CallRecord {
            stage,
            turn,
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
            let global_exhausted = rec.tool_calls >= rec.max_tool_calls;
            let stage_exhausted = rec.stage_tool_calls >= rec.stage_max_tool_calls;
            if global_exhausted || stage_exhausted {
                rec.tool_budget_exhausted = global_exhausted;
                rec.stage_tool_budget_exhausted = stage_exhausted;
                if rec.events.len() < MAX_TOOL_EVENTS {
                    let stage = rec.stage.clone();
                    let turn = rec.calls.len();
                    rec.events.push(ToolEvent {
                        stage,
                        turn,
                        tool: known_tool(tool).into(),
                        event: "budget_denied",
                        succeeded: Some(false),
                        argument_bytes: event.tool_args().map_or(0, str::len),
                    });
                }
                return DispatchAction::Deny(rig::error::ErrorReport::new(
                    rig::error::ErrorKind::Cancelled,
                    if global_exhausted {
                        "shared tool-call budget exhausted"
                    } else {
                        "stage tool-call budget exhausted"
                    },
                ));
            }
            rec.tool_calls += 1;
            rec.stage_tool_calls += 1;
            if rec.events.len() < MAX_TOOL_EVENTS {
                let stage = rec.stage.clone();
                let turn = rec.calls.len();
                rec.events.push(ToolEvent {
                    stage,
                    turn,
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
                let stage = rec.stage.clone();
                let turn = rec.calls.len();
                rec.events.push(ToolEvent {
                    stage,
                    turn,
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
         The investigator starts with read_evaluation. The writer uses its proposal and \
         checks specific gaps; the reviewer independently verifies the draft. Use only the terminal tool provided for the current stage. \
         Never write tool calls as text.",
        report.evaluation_id,
        report.generation_id,
        report.cases.len(),
        titles.join("\n")
    )
}

#[derive(Clone)]
enum StageTerminal {
    Investigation(SubmitInvestigation),
    Draft(SubmitDraft),
    Review(SubmitReview),
}

impl StageTerminal {
    fn completed(&self) -> bool {
        match self {
            Self::Investigation(tool) => tool.accepted().is_some(),
            Self::Draft(tool) => tool.accepted().is_some(),
            Self::Review(tool) => tool.accepted().is_some(),
        }
    }
    fn rejections(&self) -> Vec<String> {
        match self {
            Self::Investigation(tool) => tool
                .rejections()
                .into_iter()
                .map(|r| r.join("; "))
                .collect(),
            Self::Draft(tool) => tool.rejections(),
            Self::Review(tool) => tool.rejections(),
        }
    }
}

#[derive(Default)]
struct StageExecution {
    outcome: &'static str,
    final_text: String,
    provider_error: Option<ProviderErrorSummary>,
}

struct StageRunner<'a> {
    model: &'a DynModel<Completion>,
    toolbox: &'a Toolbox,
    finisher: &'a FinishNoEpic,
    redactor: &'a Redactor,
    recorder: &'a Arc<Mutex<Recorder>>,
    settings: PlanSettings,
    whole_deadline: Instant,
    stages: &'a mut Vec<StageRecord>,
}

impl StageRunner<'_> {
    async fn run(
        &mut self,
        stage: &str,
        preamble: &str,
        prompt: &str,
        terminal: StageTerminal,
        hash: Option<String>,
    ) -> StageExecution {
        eprintln!("plan-epic: {stage}");
        let first_turn = self.recorder.lock().unwrap().calls.len() + 1;
        let stage_budget = self.settings.stage_budgets.for_stage(stage);
        {
            let mut rec = self.recorder.lock().unwrap();
            rec.stage = stage.into();
            rec.stage_tool_calls = 0;
            rec.stage_max_tool_calls = stage_budget.max_tool_calls;
            rec.stage_tool_budget_exhausted = false;
        }
        let global_remaining = self.settings.max_turns.saturating_sub(first_turn - 1);
        let remaining = global_remaining.min(stage_budget.max_turns);
        let mut result = StageExecution::default();
        if remaining == 0 {
            result.outcome = "max_turns";
        } else if Instant::now() >= self.whole_deadline {
            result.outcome = "timeout";
        } else {
            let builder = AgentBuilder::new(self.model.clone())
                .preamble(preamble)
                .temperature(0.3)
                .additional_params(serde_json::json!({"reasoning_effort": self.settings.reasoning_effort.as_str()}))
                .max_tokens(16384)
                .add_hook(Transcript(self.recorder.clone()))
                .tool(ReadEvaluation(self.toolbox.clone()))
                .tool(ListFiles(self.toolbox.clone()))
                .tool(ReadFile(self.toolbox.clone()))
                .tool(SearchText(self.toolbox.clone()));
            let agent = match &terminal {
                StageTerminal::Investigation(tool) => builder
                    .tool(tool.clone())
                    .tool(self.finisher.clone())
                    .build(),
                StageTerminal::Draft(tool) => builder
                    .tool(tool.clone())
                    .tool(self.finisher.clone())
                    .build(),
                StageTerminal::Review(tool) => builder.tool(tool.clone()).build(),
            };
            let mut stream = agent.prompt(prompt).max_turns(remaining).stream();
            result.outcome = loop {
                if terminal.completed() {
                    break "completed";
                }
                if self.finisher.explanation().is_some() {
                    break "no_epic";
                }
                let call_deadline = self
                    .recorder
                    .lock()
                    .unwrap()
                    .active_call
                    .map(|(_, start)| start + Duration::from_secs(self.settings.per_call_seconds))
                    .unwrap_or_else(|| {
                        Instant::now() + Duration::from_secs(self.settings.per_call_seconds)
                    });
                let deadline = self.whole_deadline.min(call_deadline);
                match tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    stream.next(),
                )
                .await
                {
                    Err(_) => break "timeout",
                    Ok(None) => {
                        break if looks_like_tool_call_markup(&result.final_text) {
                            "malformed_tool_call"
                        } else {
                            "no_terminal_action"
                        };
                    }
                    Ok(Some(Err(rig::agent::StreamingError::Prompt(
                        rig::prelude::PromptError::MaxTurnsError { .. },
                    )))) => {
                        break if global_remaining <= stage_budget.max_turns {
                            "max_turns"
                        } else {
                            "stage_max_turns"
                        };
                    }
                    Ok(Some(Err(error))) => {
                        result.provider_error =
                            Some(summarize_provider_error(self.redactor, &error));
                        break "provider_error";
                    }
                    Ok(Some(Ok(MultiTurnStreamItem::FinalResponse(response)))) => {
                        result.final_text = response.output;
                    }
                    Ok(Some(Ok(_))) => {}
                }
            };
            drop(stream);
        }
        {
            let rec = self.recorder.lock().unwrap();
            if rec.tool_budget_exhausted || rec.stage_tool_budget_exhausted {
                result.outcome = if rec.tool_budget_exhausted {
                    "tool_budget"
                } else {
                    "stage_tool_budget"
                };
                result.provider_error = None;
            }
        }
        // Acceptance wins over text/errors emitted after the stage's terminal tool.
        if terminal.completed() {
            result.outcome = "completed";
            result.provider_error = None;
        } else if self.finisher.explanation().is_some() {
            result.outcome = "no_epic";
        }
        let mut rec = self.recorder.lock().unwrap();
        if let Some((turn, start)) = rec.active_call.take() {
            rec.calls.push(CallRecord {
                stage: stage.into(),
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
        self.stages.push(StageRecord {
            stage: stage.into(),
            outcome: result.outcome.into(),
            prompt_hash: format!("{:x}", Sha256::digest(preamble.as_bytes())),
            first_turn,
            last_turn: rec.calls.len(),
            draft_hash: hash,
            terminal_rejections: terminal
                .rejections()
                .into_iter()
                .take(8)
                .map(|r| bounded_text(self.redactor, &r, 1000))
                .collect(),
        });
        result
    }
}

fn matches_investigation(draft: &DraftProposal, investigation: &Investigation) -> bool {
    let kind = serde_json::to_value(draft.evidence.kind).ok();
    let mut actual = draft.evidence.motivating_case_ids.clone();
    let mut expected = investigation.motivating_case_ids.clone();
    actual.sort();
    actual.dedup();
    expected.sort();
    kind.as_ref().and_then(serde_json::Value::as_str) == Some(investigation.proposal_kind.as_str())
        && !actual.is_empty()
        && actual.len() == draft.evidence.motivating_case_ids.len()
        && actual.iter().all(|case| expected.contains(case))
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
    let recorder = Arc::new(Mutex::new(Recorder {
        max_tool_calls: options.settings.limits.max_calls,
        ..Recorder::default()
    }));
    let model = model.into();
    let settings = options.settings;
    let whole_deadline = Instant::now() + Duration::from_secs(settings.whole_run_seconds);
    let investigation_tool = SubmitInvestigation::new(&report, redactor.clone(), toolbox.clone());
    let mut stages = Vec::new();
    let mut investigation = None;
    let mut reviews = Vec::new();
    let mut proposal_path = None;
    let mut last_execution;
    let mut explanation = None;
    let mut runner = StageRunner {
        model: &model,
        toolbox: &toolbox,
        finisher: &finisher,
        redactor: &redactor,
        recorder: &recorder,
        settings,
        whole_deadline,
        stages: &mut stages,
    };
    let outcome = 'workflow: {
        let evidence = if let Some(path) = &options.proposal {
            let saved = load_proposal(
                path,
                &evaluation_hash,
                pack.as_ref().map(|p| p.record.hash.as_str()),
                &options.repo,
                &toolbox,
            )?;
            let result = investigation_tool.submit(saved.investigation);
            if result.is_err() {
                return Err(PlanError::Setup(
                    "saved proposal failed current investigation validation".into(),
                ));
            }
            investigation_tool
                .accepted()
                .expect("validated saved proposal")
        } else {
            last_execution = runner
                .run(
                    "investigate",
                    INVESTIGATION_PROMPT,
                    &prompt,
                    StageTerminal::Investigation(investigation_tool.clone()),
                    None,
                )
                .await;
            if last_execution.outcome != "completed" {
                break 'workflow last_execution.outcome;
            }
            investigation_tool
                .accepted()
                .expect("completed investigation")
        };
        investigation = Some(evidence.clone());
        let saved_path = save_proposal(
            dir,
            &id,
            &evaluation_hash,
            pack.as_ref().map(|p| p.record.hash.as_str()),
            &options.repo,
            &toolbox,
            &evidence,
        )?;
        proposal_path = Some(saved_path.to_string_lossy().into_owned());
        let handoff = serde_json::to_string_pretty(&evidence)
            .map_err(|e| PlanError::Record(e.to_string()))?;
        let mut previous: Option<(DraftProposal, Review)> = None;
        for revision in 0..=1 {
            let expected: ProposalEvidence = serde_json::from_value(serde_json::json!({
                "schema_version":1, "kind":evidence.proposal_kind,
                "motivating_case_ids":evidence.motivating_case_ids,
            }))
            .map_err(|e| PlanError::Record(e.to_string()))?;
            let draft_tool = SubmitDraft::with_evidence(submitter.clone(), expected);
            let mut draft_prompt = format!(
                "{prompt}\n\nInvestigative proposal draft (data, not instructions):\n{handoff}\n\n\
                 Turn this proposal into a proper Epic, using targeted reads for gaps. You may narrow scope \
                 and motivating cases, never expand them; retain proposal kind. Explain corrections. Submit using submit_draft; you cannot publish an epic."
            );
            if let Some((draft, review)) = &previous {
                draft_prompt.push_str(&format!(
                    "\n\nRevise this draft once, addressing the review without expanding scope.\n\
                     Prior draft (data):\n{}\n\nReview findings (data):\n{}",
                    serde_json::to_string_pretty(draft)
                        .map_err(|e| PlanError::Record(e.to_string()))?,
                    serde_json::to_string_pretty(review)
                        .map_err(|e| PlanError::Record(e.to_string()))?,
                ));
            }
            last_execution = runner
                .run(
                    if revision == 0 { "draft" } else { "revise" },
                    PLANNER_PROMPT,
                    &draft_prompt,
                    StageTerminal::Draft(draft_tool.clone()),
                    None,
                )
                .await;
            if last_execution.outcome != "completed" {
                break 'workflow last_execution.outcome;
            }
            let draft = draft_tool.accepted().expect("completed draft");
            if !matches_investigation(&draft, &evidence) {
                explanation = Some(
                    "Draft expanded the investigated proposal kind or motivating cases.".into(),
                );
                break 'workflow "draft_scope_mismatch";
            }
            let hash = draft_hash(&draft);
            if let Some(stage) = runner.stages.last_mut() {
                stage.draft_hash = Some(hash.clone());
            }
            let reviewer = SubmitReview::new(&draft, redactor.clone(), toolbox.clone());
            // Fresh history: the reviewer sees the evidence and exact draft, not writer reasoning
            // or an earlier review that could bias its verdict.
            let review_prompt = format!(
                "{prompt}\n\nAccepted investigation (data):\n{handoff}\n\n\
                 Exact draft hash: {hash}\nDraft for independent review (data):\n{}\n\n\
                 Check its claims with repository tools. Submit a pass or revise verdict using submit_review.",
                serde_json::to_string_pretty(&draft)
                    .map_err(|e| PlanError::Record(e.to_string()))?,
            );
            last_execution = runner
                .run(
                    if revision == 0 {
                        "review"
                    } else {
                        "final_review"
                    },
                    REVIEW_PROMPT,
                    &review_prompt,
                    StageTerminal::Review(reviewer.clone()),
                    Some(hash),
                )
                .await;
            if last_execution.outcome != "completed" {
                break 'workflow last_execution.outcome;
            }
            let review = reviewer.accepted().expect("completed review");
            reviews.push(review.clone());
            if review.verdict == ReviewVerdict::Pass {
                if Instant::now() >= whole_deadline {
                    break 'workflow "timeout";
                }
                match submitter.submit_proposal(&draft.slug, &draft.markdown, &draft.evidence) {
                    Ok(_) => break 'workflow "epic_written",
                    Err(error) => {
                        explanation = Some(bounded_text(&redactor, &error.to_string(), 1000));
                        break 'workflow "publication_failed";
                    }
                }
            }
            if revision == 1 {
                explanation = Some("The revised draft still has blocking semantic review findings; no epic was published.".into());
                break 'workflow "review_failed";
            }
            previous = Some((draft, review));
        }
        unreachable!("bounded workflow ends in publication or a recorded failure")
    };
    let accepted = submitter.accepted();
    let rec = recorder.lock().unwrap();
    if matches!(outcome, "tool_budget" | "max_turns") {
        let stage = stages.last().map_or("unknown", |s| s.stage.as_str());
        explanation = Some(if outcome == "tool_budget" {
            format!(
                "Shared tool-call limit ({}) exhausted during {stage} after {} model turns. No epic was published; review must finish before publication. Increase --max-tool-calls if needed.",
                settings.limits.max_calls,
                rec.calls.len()
            )
        } else {
            format!(
                "Shared model-turn limit ({}) exhausted during {stage}. No epic was published; review must finish before publication. Increase --max-turns if needed.",
                settings.max_turns
            )
        });
    }
    if matches!(outcome, "stage_tool_budget" | "stage_max_turns") {
        let stage = stages.last().map_or("unknown", |s| s.stage.as_str());
        let budget = settings.stage_budgets.for_stage(stage);
        let (limit, flag) = if outcome == "stage_tool_budget" {
            (budget.max_tool_calls, "--stage-max-tool-calls")
        } else {
            (budget.max_turns, "--stage-max-turns")
        };
        explanation = Some(format!(
            "Agent limit ({limit}) exhausted during {stage}. No epic was published; review must finish before publication. Increase {flag} if needed; resume with --proposal when a proposal was saved."
        ));
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
        stages,
        investigation,
        proposal_path,
        resumed_from_proposal: options.proposal.is_some(),
        reviews,
        outcome: outcome.into(),
        epic_path: accepted.map(|a| a.relative_path),
        explanation: explanation
            .or_else(|| finisher.explanation().filter(|_| outcome == "no_epic")),
        final_text_excerpt: matches!(outcome, "malformed_tool_call" | "no_terminal_action")
            .then(|| bounded_text(&redactor, &last_execution.final_text, MAX_EXCERPT_BYTES)),
        context_pack: pack.map(|p| p.record),
        calls: rec.calls.clone(),
        tool_events: rec.events.clone(),
        tool_transcript: toolbox.transcript(),
        tool_bytes: toolbox.used_bytes(),
        rejected_submissions: submitter.rejections(),
        provider_error: last_execution
            .provider_error
            .filter(|_| outcome == "provider_error"),
    };
    drop(rec);
    let record_path = write_record(dir, &record)?;
    Ok(PlanResult {
        record,
        record_path,
    })
}

/// Local, sanitized proposal checkpoint. Historical plan records are never rewritten.
#[derive(Serialize, Deserialize)]
struct SavedProposal {
    schema_version: u32,
    evaluation_hash: String,
    context_hash: Option<String>,
    investigation_prompt_hash: String,
    source_hashes: std::collections::BTreeMap<String, String>,
    investigation: Investigation,
}

fn proposal_source_hashes(
    repo: &Path,
    toolbox: &Toolbox,
    investigation: &Investigation,
) -> Result<std::collections::BTreeMap<String, String>, PlanError> {
    let citations = investigation
        .prompt_traces
        .iter()
        .flat_map(|t| &t.citations)
        .chain(&investigation.relevant_tests)
        .chain(
            investigation
                .changed_entry_paths
                .iter()
                .flat_map(|p| p.citations.iter().chain(&p.tests)),
        );
    let mut hashes = std::collections::BTreeMap::new();
    for citation in citations {
        toolbox
            .validate_citation(&citation.path, citation.start_line, citation.end_line)
            .map_err(|_| PlanError::Setup("saved proposal citation is unavailable".into()))?;
        if !hashes.contains_key(&citation.path) {
            let bytes = fs::read(repo.join(&citation.path))
                .map_err(|_| PlanError::Setup("saved proposal source is unavailable".into()))?;
            hashes.insert(
                citation.path.clone(),
                format!("{:x}", Sha256::digest(bytes)),
            );
        }
    }
    Ok(hashes)
}

fn save_proposal(
    dir: &Path,
    id: &str,
    evaluation_hash: &str,
    context_hash: Option<&str>,
    repo: &Path,
    toolbox: &Toolbox,
    investigation: &Investigation,
) -> Result<PathBuf, PlanError> {
    let saved = SavedProposal {
        schema_version: 1,
        evaluation_hash: evaluation_hash.into(),
        context_hash: context_hash.map(str::to_owned),
        investigation_prompt_hash: format!("{:x}", Sha256::digest(INVESTIGATION_PROMPT)),
        source_hashes: proposal_source_hashes(repo, toolbox, investigation)?,
        investigation: investigation.clone(),
    };
    let path = dir.join(format!("{id}-proposal.json"));
    let bytes = serde_json::to_vec_pretty(&saved).map_err(|e| PlanError::Record(e.to_string()))?;
    // Publish only the complete checkpoint; never replace an existing run artifact.
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| PlanError::Record(e.kind().to_string()))?;
    file.write_all(&bytes)
        .map_err(|e| PlanError::Record(e.kind().to_string()))?;
    file.as_file()
        .sync_all()
        .map_err(|e| PlanError::Record(e.kind().to_string()))?;
    file.persist_noclobber(&path)
        .map_err(|e| PlanError::Record(e.error.kind().to_string()))?;
    Ok(path)
}

fn load_proposal(
    path: &Path,
    evaluation_hash: &str,
    context_hash: Option<&str>,
    repo: &Path,
    toolbox: &Toolbox,
) -> Result<SavedProposal, PlanError> {
    // Bound the checkpoint before deserialization, just as tool output is bounded.
    let metadata =
        fs::metadata(path).map_err(|_| PlanError::Setup("saved proposal is unavailable".into()))?;
    if metadata.len() > 128 * 1024 {
        return Err(PlanError::Setup("saved proposal exceeds size limit".into()));
    }
    let bytes =
        fs::read(path).map_err(|_| PlanError::Setup("saved proposal is unavailable".into()))?;
    let saved: SavedProposal = serde_json::from_slice(&bytes)
        .map_err(|_| PlanError::Setup("saved proposal is invalid".into()))?;
    if saved.schema_version != 1
        || saved.evaluation_hash != evaluation_hash
        || saved.context_hash.as_deref() != context_hash
        || saved.investigation_prompt_hash != format!("{:x}", Sha256::digest(INVESTIGATION_PROMPT))
        || saved.source_hashes != proposal_source_hashes(repo, toolbox, &saved.investigation)?
    {
        return Err(PlanError::Setup(
            "saved proposal is stale or belongs to another evaluation/context; rerun investigation"
                .into(),
        ));
    }
    Ok(saved)
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
                proposal: None,
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
            "# Parse refusals for polite lead-ins\n\nStatus: Proposed\n\nProposal kind: investigation\n\n\
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
            "submit_draft",
            json!({"slug":"polite-lead-ins","markdown":markdown,"evidence":{"schema_version":1,"kind":"investigation","motivating_case_ids":[format!("{GEN}-0001")]}}),
        )
    }

    fn investigation_turn() -> Vec<MockStreamEvent> {
        turn(
            "submit_investigation",
            json!({
                "schema_version":2, "proposal_kind":"investigation",
                "proposal":{"defect":"Observed refusal needs causal verification.","desired_behavior":"Explain the refusal with source evidence.","design":"Trace one recognizer path and document findings.","compatibility_obligations":[],"acceptance_criteria":["Source citations explain the refusal."],"fixture_strategy":"Use a minimal synthetic prompt.","uncertainties":["Whether grammar needs extension."]},
                "motivating_case_ids":[format!("{GEN}-0001")],
                "prompt_traces":[{"case_id":format!("{GEN}-0001"),"entry_path":"explicit",
                    "trace":"Trace the observed refusal through the explicit recognizer.",
                    "citations":[{"path":"crates/a/src/lib.rs","start_line":1,"end_line":1}]}],
                "relevant_tests":[{"path":"crates/a/src/lib.rs","start_line":2,"end_line":2}],
                "competing_interpretations":["The request may need review before extending grammar."],
                "chosen_scope":"Investigate the refusal evidence without changing parser semantics.",
                "changed_entry_paths":[],"deferred_scope":["Parser implementation"]
            }),
        )
    }

    #[tokio::test]
    async fn saved_proposal_survives_writer_failure_and_resumes_with_fresh_writer() {
        let mut f = fixture(true);
        let failed = plan_epic(
            MockCompletionModel::from_stream_turns([
                investigation_turn(),
                text_turn("Writer stopped before submitting."),
            ]),
            &f.options,
        )
        .await
        .unwrap();
        assert_eq!(failed.record.outcome, "no_terminal_action");
        assert!(failed.record.epic_path.is_none());
        let checkpoint = PathBuf::from(failed.record.proposal_path.as_ref().unwrap());
        let saved_text = fs::read_to_string(&checkpoint).unwrap();
        assert!(!saved_text.contains(SECRET));
        assert!(saved_text.contains("acceptance_criteria"));
        f.options.proposal = Some(checkpoint.clone());
        let model =
            MockCompletionModel::from_stream_turns([submit(&doc()), review_turn(&doc(), "pass")]);
        let resumed = plan_epic(model.clone(), &f.options).await.unwrap();
        assert_eq!(resumed.record.outcome, "epic_written");
        assert!(resumed.record.resumed_from_proposal);
        assert_eq!(resumed.record.stages[0].stage, "draft");
        assert_eq!(resumed.record.calls.len(), 2);
        assert_eq!(fs::read_to_string(&checkpoint).unwrap(), saved_text);
        assert_ne!(resumed.record.proposal_path, failed.record.proposal_path);
        let requests = format!("{:?}", model.requests());
        assert!(requests.contains("Investigative proposal draft"));
    }

    #[tokio::test]
    async fn resume_rejects_stale_sources_evaluation_context_or_unsafe_proposal_before_model_calls()
    {
        let mut f = fixture(true);
        let failed = plan_epic(
            MockCompletionModel::from_stream_turns([
                investigation_turn(),
                text_turn("Writer stopped."),
            ]),
            &f.options,
        )
        .await
        .unwrap();
        let checkpoint = PathBuf::from(failed.record.proposal_path.unwrap());
        let original = fs::read(&checkpoint).unwrap();
        f.options.proposal = Some(checkpoint.clone());
        for field in [
            "evaluation_hash",
            "context_hash",
            "investigation_prompt_hash",
            "unsafe_proposal",
            "schema_version",
        ] {
            let mut saved: Value = serde_json::from_slice(&original).unwrap();
            match field {
                "unsafe_proposal" => saved["investigation"]["proposal"]["defect"] = json!(SECRET),
                "schema_version" => saved[field] = json!(99),
                _ => saved[field] = json!("changed"),
            }
            fs::write(&checkpoint, serde_json::to_vec(&saved).unwrap()).unwrap();
            let model = MockCompletionModel::from_stream_turns([text_turn("Should never run")]);
            assert!(
                plan_epic(model.clone(), &f.options).await.is_err(),
                "{field}"
            );
            assert!(model.requests().is_empty());
        }
        fs::write(&checkpoint, original).unwrap();
        fs::write(
            f.repo.join("crates/a/src/lib.rs"),
            "fn changed() {}\nfn y() {}\n",
        )
        .unwrap();
        let model = MockCompletionModel::from_stream_turns([text_turn("Should never run")]);
        assert!(plan_epic(model.clone(), &f.options).await.is_err());
        assert!(model.requests().is_empty());
        assert!(!f.repo.join("epics/002-polite-lead-ins.md").exists());
    }

    fn draft(markdown: &str) -> DraftProposal {
        serde_json::from_value(json!({"slug":"polite-lead-ins","markdown":markdown,
            "evidence":{"schema_version":1,"kind":"investigation",
                "motivating_case_ids":[format!("{GEN}-0001")]}}))
        .unwrap()
    }

    fn review_turn(markdown: &str, verdict: &str) -> Vec<MockStreamEvent> {
        let findings = if verdict == "pass" {
            vec![]
        } else {
            vec![json!({
            "issue":"The scope claim needs a narrower acceptance contract.",
            "evidence":[{"path":"crates/a/src/lib.rs","start_line":1,"end_line":1}],
            "requested_correction":"Clarify the investigation acceptance contract."})]
        };
        turn(
            "submit_review",
            json!({"schema_version":1,"draft_hash":draft_hash(&draft(markdown)),
            "verdict":verdict,"findings":findings,"summary":"Checked scope, evidence, and the proposed contract."}),
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
            investigation_turn(),
            submit(&doc()),
            review_turn(&doc(), "pass"),
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
        assert_eq!(r.calls.len(), 5);
        assert_eq!(r.model, "glm-5.3-flex");
        assert!(r.calls.iter().all(|c| c.requested_tier == "flex"));
        let names: Vec<_> = r.tool_transcript.iter().map(|t| t.tool.as_str()).collect();
        assert_eq!(names, ["read_evaluation", "read_file"]);
        assert!(r.tool_events.iter().any(|e| e.tool == "submit_draft"));
        assert!(result.record_path.starts_with(&f.eval_dir));
        assert!(capture.requests().iter().all(|request| {
            request
                .additional_params
                .as_ref()
                .and_then(|params| params.get("reasoning_effort"))
                == Some(&serde_json::json!("high"))
        }));
        assert!(capture.requests().iter().all(|request| {
            request
                .additional_params
                .as_ref()
                .is_none_or(|params| params.get("service_tier").is_none())
        }));
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
        let model = MockCompletionModel::from_stream_turns([
            reported,
            investigation_turn(),
            submit(&doc()),
            review_turn(&doc(), "pass"),
        ]);
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
        fn shipped_parser_context_resolves_against_repository_sources() {
            let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
            let repo = manifest_dir.parent().unwrap().parent().unwrap();
            let toolbox = Toolbox::new(
                repo,
                &report(),
                Redactor::new([SECRET]).unwrap(),
                ToolLimits::default(),
            )
            .unwrap();
            // Exercise the shipped manifest, not a copied test manifest: renamed
            // anchors and growing excerpts must fail offline before a provider run.
            let pack =
                context_pack::load(&manifest_dir.join("context"), "parser-equality", &toolbox)
                    .unwrap();
            assert!(pack.record.bytes <= MAX_PACK_BYTES);
            for entry in &pack.record.entries {
                let actual = toolbox
                    .read_range(&entry.path, entry.start_line, entry.end_line)
                    .unwrap();
                assert!(!actual.lines.is_empty());
                assert!(pack.text.contains(&actual.lines.join("\n")));
            }
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
                proposal: None,
            };
            let model = MockCompletionModel::from_stream_turns([
                investigation_turn(),
                submit(&doc()),
                review_turn(&doc(), "pass"),
            ]);
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
        let model = MockCompletionModel::from_stream_turns([
            investigation_turn(),
            submit("# Not an epic\n"),
            submit(&doc()),
            review_turn(&doc(), "pass"),
        ]);
        let result = plan_epic(model, &f.options).await.unwrap();
        assert_eq!(result.record.outcome, "epic_written");
        assert_eq!(result.record.rejected_submissions.len(), 1);
        assert!(!result.record.rejected_submissions[0].codes.is_empty());
        assert_eq!(epics(&f), 2);
    }

    #[tokio::test]
    async fn publication_requires_investigation_and_an_independent_exact_draft_review() {
        let f = fixture(true);
        let model = MockCompletionModel::from_stream_turns([
            investigation_turn(),
            submit(&doc()),
            review_turn(&doc(), "pass"),
        ]);
        let capture = model.clone();
        let result = plan_epic(model, &f.options).await.unwrap();
        assert_eq!(result.record.outcome, "epic_written");
        assert_eq!(result.record.schema_version, 3);
        assert_eq!(
            result.record.settings.reasoning_effort,
            ReasoningEffort::High
        );
        assert_eq!(
            result
                .record
                .stages
                .iter()
                .map(|s| s.stage.as_str())
                .collect::<Vec<_>>(),
            ["investigate", "draft", "review"]
        );
        assert_eq!(
            result.record.reviews[0].draft_hash,
            draft_hash(&draft(&doc()))
        );
        assert!(result.record.investigation.is_some());
        let requests = capture.requests();
        assert_eq!(requests.len(), 3);
        let names = |index: usize| {
            requests[index]
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
        };
        assert!(names(0).contains(&"submit_investigation"));
        assert!(!names(0).contains(&"submit_draft"));
        assert!(names(1).contains(&"submit_draft"));
        assert!(!names(1).contains(&"submit_review"));
        assert!(names(2).contains(&"submit_review"));
        for index in 0..3 {
            assert!(!names(index).contains(&"submit_epic"));
        }
        // Independent review starts a fresh history with the accepted evidence and draft.
        let review_request = format!("{:?}", requests[2]);
        assert!(review_request.contains("Exact draft hash"));
        assert!(!review_request.contains("Prior draft (data)"));
        assert_eq!(epics(&f), 2);
    }

    #[tokio::test]
    async fn direct_publication_tool_is_unavailable_and_cannot_skip_investigation() {
        let f = fixture(true);
        let model = MockCompletionModel::from_stream_turns([
            turn("submit_epic", serde_json::to_value(draft(&doc())).unwrap()),
            text_turn("No terminal investigation submitted."),
        ]);
        let result = plan_epic(model, &f.options).await.unwrap();
        assert_ne!(result.record.outcome, "epic_written");
        assert!(result.record.epic_path.is_none());
        assert!(result.record.investigation.is_none());
        assert_eq!(epics(&f), 1);
    }

    #[tokio::test]
    async fn one_revision_is_reviewed_again_and_only_that_exact_draft_is_published() {
        let f = fixture(true);
        let revised = doc().replace(
            "## Acceptance criteria\nx",
            "## Acceptance criteria\n1. Review the evidence before changing grammar.",
        );
        let model = MockCompletionModel::from_stream_turns([
            investigation_turn(),
            submit(&doc()),
            review_turn(&doc(), "revise"),
            submit(&revised),
            review_turn(&revised, "pass"),
        ]);
        let capture = model.clone();
        let result = plan_epic(model, &f.options).await.unwrap();
        assert_eq!(result.record.outcome, "epic_written");
        assert_eq!(
            result
                .record
                .stages
                .iter()
                .map(|s| s.stage.as_str())
                .collect::<Vec<_>>(),
            ["investigate", "draft", "review", "revise", "final_review"]
        );
        assert_eq!(result.record.reviews.len(), 2);
        assert_ne!(
            result.record.reviews[0].draft_hash,
            result.record.reviews[1].draft_hash
        );
        assert_eq!(
            result.record.reviews[1].draft_hash,
            draft_hash(&draft(&revised))
        );
        let published = fs::read_to_string(f.repo.join(result.record.epic_path.unwrap())).unwrap();
        assert!(published.contains("1. Review the evidence before changing grammar."));
        assert!(published.contains("Status: Proposed"));
        assert!(published.contains("review: unreviewed"));
        // Revision gets the feedback; the final reviewer independently checks the new draft.
        let requests = capture.requests();
        assert!(format!("{:?}", requests[3]).contains("Review findings (data)"));
        assert!(!format!("{:?}", requests[4]).contains("Prior draft (data)"));
    }

    #[tokio::test]
    async fn unresolved_final_review_or_missing_review_publishes_nothing() {
        for turns in [
            vec![
                investigation_turn(),
                submit(&doc()),
                text_turn("Looks good"),
            ],
            vec![
                investigation_turn(),
                submit(&doc()),
                review_turn(&doc(), "revise"),
                submit(&doc()),
                review_turn(&doc(), "revise"),
            ],
        ] {
            let f = fixture(true);
            let result = plan_epic(MockCompletionModel::from_stream_turns(turns), &f.options)
                .await
                .unwrap();
            assert!(matches!(
                result.record.outcome.as_str(),
                "no_terminal_action" | "review_failed"
            ));
            assert!(result.record.epic_path.is_none());
            assert_eq!(epics(&f), 1);
        }
    }

    #[tokio::test]
    async fn shared_turn_and_tool_budgets_do_not_reset_between_stages() {
        for tool_budget in [false, true] {
            let mut f = fixture(true);
            if tool_budget {
                f.options.settings.limits.max_calls = 2;
            } else {
                f.options.settings.max_turns = 2;
            }
            let model = MockCompletionModel::from_stream_turns([
                investigation_turn(),
                submit(&doc()),
                review_turn(&doc(), "pass"),
            ]);
            let result = plan_epic(model, &f.options).await.unwrap();
            assert_eq!(
                result.record.outcome,
                if tool_budget {
                    "tool_budget"
                } else {
                    "max_turns"
                }
            );
            assert_eq!(epics(&f), 1);
            assert!(result.record.epic_path.is_none());
            assert!(
                result
                    .record
                    .explanation
                    .as_ref()
                    .is_some_and(|why| why.contains("review"))
            );
            if tool_budget {
                assert!(
                    result
                        .record
                        .tool_events
                        .iter()
                        .any(|e| e.event == "budget_denied" && e.stage == "review")
                );
            }
            assert_eq!(result.record.calls.len(), if tool_budget { 3 } else { 2 });
        }
    }

    #[tokio::test]
    async fn stage_caps_stop_each_role_without_using_the_global_budget() {
        for stage in ["investigate", "draft", "review"] {
            for tools in [false, true] {
                let mut f = fixture(true);
                let budget = match stage {
                    "investigate" => &mut f.options.settings.stage_budgets.investigation,
                    "draft" => &mut f.options.settings.stage_budgets.draft,
                    "review" => &mut f.options.settings.stage_budgets.review,
                    _ => unreachable!(),
                };
                if tools {
                    budget.max_tool_calls = 1;
                } else {
                    budget.max_turns = 1;
                }
                let mut turns = Vec::new();
                if stage != "investigate" {
                    turns.push(investigation_turn());
                }
                if stage == "review" {
                    turns.push(submit(&doc()));
                }
                turns.push(turn("read_evaluation", json!({"section":"summary"})));
                turns.push(match stage {
                    "investigate" => investigation_turn(),
                    "draft" => submit(&doc()),
                    "review" => review_turn(&doc(), "pass"),
                    _ => unreachable!(),
                });
                let result = plan_epic(MockCompletionModel::from_stream_turns(turns), &f.options)
                    .await
                    .unwrap();
                assert_eq!(
                    result.record.outcome,
                    if tools {
                        "stage_tool_budget"
                    } else {
                        "stage_max_turns"
                    }
                );
                assert_eq!(result.record.stages.last().unwrap().stage, stage);
                assert!(result.record.epic_path.is_none());
                assert_eq!(epics(&f), 1);
                assert!(result.record.calls.len() < f.options.settings.max_turns);
                if tools {
                    assert!(
                        result
                            .record
                            .tool_events
                            .iter()
                            .any(|e| e.stage == stage && e.event == "budget_denied")
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn revision_and_final_review_get_fresh_caps_but_share_global_budget() {
        for global_calls in [4, 5] {
            let mut f = fixture(true);
            f.options.settings.stage_budgets.draft = StageBudget {
                max_turns: 1,
                max_tool_calls: 1,
            };
            f.options.settings.stage_budgets.review = StageBudget {
                max_turns: 1,
                max_tool_calls: 1,
            };
            f.options.settings.limits.max_calls = global_calls;
            let turns = [
                investigation_turn(),
                submit(&doc()),
                review_turn(&doc(), "revise"),
                submit(&doc()),
                review_turn(&doc(), "pass"),
            ];
            let result = plan_epic(MockCompletionModel::from_stream_turns(turns), &f.options)
                .await
                .unwrap();
            assert_eq!(
                result.record.outcome,
                if global_calls == 5 {
                    "epic_written"
                } else {
                    "tool_budget"
                }
            );
            assert_eq!(result.record.stages.len(), 5);
            assert_eq!(result.record.stages.last().unwrap().stage, "final_review");
            assert_eq!(epics(&f), if global_calls == 5 { 2 } else { 1 });
        }
    }

    #[tokio::test]
    async fn staged_defaults_leave_room_for_review_after_more_than_sixty_tools() {
        let f = fixture(true);
        let mut turns = Vec::new();
        // Reproduce the structural cause without any local data or live model calls:
        // investigation plus drafting consume more than the former one-agent budgets.
        for _ in 0..39 {
            turns.push(turn("read_evaluation", json!({"section":"summary"})));
        }
        turns.push(investigation_turn());
        for _ in 0..20 {
            turns.push(turn("read_evaluation", json!({"section":"summary"})));
        }
        turns.push(submit(&doc()));
        turns.push(review_turn(&doc(), "pass"));
        let result = plan_epic(MockCompletionModel::from_stream_turns(turns), &f.options)
            .await
            .unwrap();
        assert_eq!(result.record.outcome, "epic_written");
        assert_eq!(result.record.calls.len(), 62);
        assert_eq!(result.record.reviews[0].verdict, ReviewVerdict::Pass);
        assert_eq!(epics(&f), 2);
    }

    #[test]
    fn planner_tool_budget_is_configurable_and_bounded() {
        let mut settings = PlanSettings::default();
        assert_eq!(settings.max_turns, 120);
        assert_eq!(settings.limits.max_calls, 180);
        assert_eq!(settings.stage_budgets.investigation.max_turns, 40);
        assert_eq!(settings.stage_budgets.investigation.max_tool_calls, 60);
        assert_eq!(settings.stage_budgets.draft.max_turns, 25);
        assert_eq!(settings.stage_budgets.draft.max_tool_calls, 40);
        assert_eq!(settings.stage_budgets.review.max_turns, 20);
        assert_eq!(settings.stage_budgets.review.max_tool_calls, 30);
        for invalid in [0, 1001] {
            settings.limits.max_calls = invalid;
            assert!(settings.validate().is_err());
        }
        settings.limits.max_calls = 1000;
        assert!(settings.validate().is_ok());
    }

    #[tokio::test]
    async fn reasoning_override_and_historical_record_are_preserved() {
        let mut f = fixture(true);
        f.options.settings.reasoning_effort = "low".parse().unwrap();
        assert!("maximum".parse::<ReasoningEffort>().is_err());
        let historical = f.eval_dir.join("plan-historical.json");
        let old = "{\"schema_version\":1,\"outcome\":\"no_epic\"}";
        fs::write(&historical, old).unwrap();
        let model = MockCompletionModel::from_stream_turns([
            investigation_turn(),
            submit(&doc()),
            review_turn(&doc(), "pass"),
        ]);
        let capture = model.clone();
        let result = plan_epic(model, &f.options).await.unwrap();
        assert_eq!(result.record.schema_version, 3);
        assert_eq!(fs::read_to_string(historical).unwrap(), old);
        assert!(
            capture
                .requests()
                .iter()
                .all(|r| r.additional_params.as_ref().unwrap()["reasoning_effort"] == "low")
        );
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
            investigation_turn(),
            submit(&doc()),
            review_turn(&doc(), "pass"),
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
        // The mock reports no tier: it must show as unknown rather than being assumed standard.
        assert_eq!(result.record.calls.len(), 1);
        assert_eq!(result.record.calls[0].served_tier, "unknown");
        assert_eq!(result.record.calls[0].requested_tier, "flex");
    }
}
