//! Three independent Rig sessions for one bounded repair attempt.
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rig::agent::{AgentHook, CompletionCallAction, CompletionCallEvent, HookContext};
use rig::completion::Message;
use rig::completion::message::ToolChoice;
use rig::core::error::{ErrorKind, ProviderError};
use rig::http_client::Error as HttpError;
use rig::prelude::PromptError;
use rig::tool::PortableTool;
use rig::{Agent, AgentBuilder, DynModel, operation::Completion};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::anchored_edits::{
    CodeAnswer, Edit, MAX_EDITS, MAX_NEW_FILE_BYTES, MAX_NEW_FILES, NewFile, Planned, apply,
    materialize_review_view, plan, render_diff, restore,
};
use crate::redaction::Redactor;
use crate::repair::SuiteId;
use crate::repair_http::RetryEvents;
use crate::repair_live::LiveWorker;
use crate::repair_progress::RepairProgress;
use crate::repair_trace::AgentTrace;
use crate::source_tools::{
    OutlineTool, ReadLinesTool, SearchTextTool, SourceLimits, SourceSession,
};

#[derive(Debug, thiserror::Error)]
enum ToolDenied {
    #[error("repair tool denied request")]
    Denied,
    #[error("read_before_edit: call read_lines for every edited line using the baseline file hash")]
    ReadBeforeEdit,
    #[error("draft_rejected: {0}")]
    DraftRejected(String),
    #[error("staged_check_limit: at most four staged checks per coding attempt")]
    StagedCheckLimit,
    #[error(
        "unchecked_draft: run check_staged(additive_tests) successfully on the current draft before submission"
    )]
    UncheckedDraft,
    #[error(
        "invalid_build_id: use the exact build_id issued in the stage prompt or a prior build result; do not derive or shorten it"
    )]
    InvalidBuildId,
    #[error(
        "invalid_sql: use SELECT _row FROM data WHERE lower(\"Role\") = lower('VALUE') ORDER BY _row (replace VALUE with a synthetic Role value, double embedded single quotes, omit the semicolon)"
    )]
    InvalidSql,
}

fn require_issued_build_id(issued: bool) -> Result<(), ToolDenied> {
    issued.then_some(()).ok_or(ToolDenied::InvalidBuildId)
}

/// Probe and check tools draw on the same call budget as source navigation.
#[derive(Clone)]
struct SourceTools {
    nav: SourceSession,
}

impl SourceTools {
    fn charge(&self) -> Result<(), ToolDenied> {
        self.nav.charge_call().map_err(|_| ToolDenied::Denied)
    }
}

#[derive(Clone)]
struct DraftTools {
    tools: SourceTools,
    checkout: PathBuf,
    draft: Arc<Mutex<DraftState>>,
    max_bytes: usize,
}

#[derive(Default)]
struct DraftState {
    answer: CodeAnswer,
    submitted: bool,
    additive_check_sha256: Option<String>,
    require_additive_check: bool,
}

impl DraftState {
    fn record_staged_check(&mut self, sha256: &str, suite: SuiteId, passed: bool) -> bool {
        let current = !self.submitted
            && serde_json::to_vec(&self.answer)
                .ok()
                .is_some_and(|bytes| digest(bytes) == sha256);
        if current && passed && suite == SuiteId::AdditiveTests {
            self.additive_check_sha256 = Some(sha256.to_owned());
        }
        current
    }

    fn checked_current_draft(&self) -> bool {
        !self.require_additive_check
            || serde_json::to_vec(&self.answer).ok().is_some_and(|bytes| {
                self.additive_check_sha256.as_deref() == Some(digest(bytes).as_str())
            })
    }
}

impl DraftTools {
    fn stage(
        &self,
        read_anchor: Option<&Edit>,
        change: impl FnOnce(&mut CodeAnswer),
    ) -> Result<Value, ToolDenied> {
        self.tools.charge()?;
        if read_anchor.is_some_and(|edit| {
            !self.tools.nav.was_read(
                &edit.path,
                &edit.baseline_sha256,
                edit.start_line,
                edit.end_line,
            )
        }) {
            return Err(ToolDenied::ReadBeforeEdit);
        }
        let mut draft = self.draft.lock().map_err(|_| ToolDenied::Denied)?;
        if draft.submitted {
            return Err(ToolDenied::Denied);
        }
        let mut proposed = draft.answer.clone();
        change(&mut proposed);
        if serde_json::to_vec(&proposed)
            .map_err(|_| ToolDenied::Denied)?
            .len()
            > self.max_bytes
        {
            return Err(ToolDenied::DraftRejected(
                "draft exceeds output byte limit".into(),
            ));
        }
        plan(&self.checkout, &proposed, allowed).map_err(|rejection| {
            ToolDenied::DraftRejected(format!("{} ({})", rejection.code, rejection.detail))
        })?;
        let result = json!({"staged_edits":proposed.edits.len(),"staged_new_files":proposed.new_files.len()});
        draft.answer = proposed;
        draft.additive_check_sha256 = None;
        Ok(result)
    }
}

#[derive(Clone)]
struct SubmitStagedTool(DraftTools);

impl PortableTool for SubmitStagedTool {
    const NAME: &'static str = "submit_staged";
    type Args = Value;
    type Output = Value;
    type Error = ToolDenied;
    fn description(&self) -> String {
        "Submit the complete staged patch after check_staged(additive_tests) passes for this exact draft. The harness validates it before any checkout write. Call this once after all stage calls, or return {\"submit_staged\":true} as the final answer.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn call(&self, args: Value) -> Result<Value, ToolDenied> {
        self.0.tools.charge()?;
        if args != json!({}) {
            return Err(ToolDenied::Denied);
        }
        let mut draft = self.0.draft.lock().map_err(|_| ToolDenied::Denied)?;
        if draft.submitted {
            return Err(ToolDenied::Denied);
        }
        plan(&self.0.checkout, &draft.answer, allowed).map_err(|rejection| {
            ToolDenied::DraftRejected(format!("{} ({})", rejection.code, rejection.detail))
        })?;
        if !draft.checked_current_draft() {
            return Err(ToolDenied::UncheckedDraft);
        }
        draft.submitted = true;
        Ok(
            json!({"submitted":true,"edits":draft.answer.edits.len(),"new_files":draft.answer.new_files.len()}),
        )
    }
}

#[derive(Clone)]
struct StageEditTool(DraftTools);

impl PortableTool for StageEditTool {
    const NAME: &'static str = "stage_edit";
    type Args = Edit;
    type Output = Value;
    type Error = ToolDenied;
    fn description(&self) -> String {
        "Stage or replace one anchored edit in memory. Reuse the same path, baseline hash, and line range to correct an earlier staged edit. The entire draft is validated against the unchanged baseline after each call. Nothing is written until the patch is submitted.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"baseline_sha256":{"type":"string"},"start_line":{"type":"integer"},"end_line":{"type":"integer"},"old":{"type":"string"},"new":{"type":"string"}},"required":["path","baseline_sha256","start_line","end_line","old","new"],"additionalProperties":false})
    }
    async fn call(&self, edit: Edit) -> Result<Value, ToolDenied> {
        self.0.stage(Some(&edit), |draft| {
            if let Some(existing) = draft.edits.iter_mut().find(|existing| {
                existing.path == edit.path
                    && existing.baseline_sha256 == edit.baseline_sha256
                    && existing.start_line == edit.start_line
                    && existing.end_line == edit.end_line
            }) {
                *existing = edit.clone();
            } else {
                draft.edits.push(edit.clone());
            }
        })
    }
}

#[derive(Clone)]
struct StageNewFileTool(DraftTools);

impl PortableTool for StageNewFileTool {
    const NAME: &'static str = "stage_new_file";
    type Args = NewFile;
    type Output = Value;
    type Error = ToolDenied;
    fn description(&self) -> String {
        format!(
            "Stage or replace one new Rust test file in memory, at most {MAX_NEW_FILE_BYTES} bytes. Reuse the same path to correct a staged file. The checkout stays unchanged until final submission."
        )
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false})
    }
    async fn call(&self, file: NewFile) -> Result<Value, ToolDenied> {
        self.0.stage(None, |draft| {
            if let Some(existing) = draft
                .new_files
                .iter_mut()
                .find(|existing| existing.path == file.path)
            {
                *existing = file;
            } else {
                draft.new_files.push(file);
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeArgs {
    pub(crate) build_id: String,
    pub(crate) csv: String,
    pub(crate) prompt: String,
    pub(crate) sql: Option<String>,
}

#[derive(Clone)]
struct ProbeTool {
    tools: SourceTools,
    worker: Arc<Mutex<LiveWorker>>,
    redactor: Redactor,
}

fn probe_tool_output(observation: crate::probe::ProbeObservation) -> Result<Value, ToolDenied> {
    let mut output = serde_json::to_value(observation).map_err(|_| ToolDenied::Denied)?;
    output["asserted_desired_behavior"] = Value::Bool(false);
    Ok(output)
}

const PROBE_SQL_TEMPLATE: &str =
    "SELECT _row FROM data WHERE lower(\"Role\") = lower('VALUE') ORDER BY _row";

fn validate_probe_sql(csv: &str, sql: &str) -> Result<(), ToolDenied> {
    let rows = crate::probe::validate_synthetic(csv.as_bytes()).map_err(|_| ToolDenied::Denied)?;
    let role = rows[0]
        .cells
        .iter()
        .position(|cell| cell == "Role")
        .ok_or(ToolDenied::InvalidSql)?;
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
    crate::sql::parse_role_query(sql, &profile)
        .map(|_| ())
        .map_err(|_| ToolDenied::InvalidSql)
}

impl PortableTool for ProbeTool {
    const NAME: &'static str = "probe";
    type Args = ProbeArgs;
    type Output = Value;
    type Error = ToolDenied;
    fn description(&self) -> String {
        format!(
            "Run the confined public CLI on bounded neutral synthetic CSV. Optional SQL must use exactly {PROBE_SQL_TEMPLATE}; replace VALUE with a value in the synthetic Role column (double embedded single quotes). No semicolon or extra clauses."
        )
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"build_id":{"type":"string"},"csv":{"type":"string"},"prompt":{"type":"string"},"sql":{"type":["string","null"]}},"required":["build_id","csv","prompt"],"additionalProperties":false})
    }
    async fn call(&self, args: ProbeArgs) -> Result<Value, ToolDenied> {
        self.tools.charge()?;
        if !self
            .redactor
            .scan(&format!(
                "{}\n{}\n{}",
                args.csv,
                args.prompt,
                args.sql.as_deref().unwrap_or_default()
            ))
            .is_empty()
        {
            return Err(ToolDenied::Denied);
        }
        if let Some(sql) = args.sql.as_deref() {
            validate_probe_sql(&args.csv, sql)?;
        }
        let mut worker = self.worker.lock().map_err(|_| ToolDenied::Denied)?;
        require_issued_build_id(worker.has_issued_build_id(&args.build_id))?;
        let observation = worker
            .synthetic_probe(&args.build_id, &args.csv, &args.prompt, args.sql.as_deref())
            .map_err(|_| ToolDenied::Denied)?;
        probe_tool_output(observation)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckArgs {
    pub(crate) build_id: String,
    pub(crate) suite: SuiteId,
}

#[derive(Clone)]
struct RunChecksTool {
    tools: SourceTools,
    worker: Arc<Mutex<LiveWorker>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StagedCheckArgs {
    pub(crate) suite: SuiteId,
}

#[derive(Clone)]
struct CheckStagedTool {
    draft: DraftTools,
    worker: Arc<Mutex<LiveWorker>>,
}

impl PortableTool for CheckStagedTool {
    const NAME: &'static str = "check_staged";
    type Args = StagedCheckArgs;
    type Output = Value;
    type Error = ToolDenied;
    fn description(&self) -> String {
        "Run one fixed check suite against a private confined copy of the current staged patch and frozen regression. The response identifies the exact draft and tree checked; later stage calls invalidate that result. At most four staged checks are allowed. No authoritative checkout files are changed.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"suite":{"type":"string","enum":["protected_compatibility","additive_tests","owning_crate","workspace_format","workspace_check","workspace_tests"]}},"required":["suite"],"additionalProperties":false})
    }
    async fn call(&self, args: StagedCheckArgs) -> Result<Value, ToolDenied> {
        self.draft.tools.charge()?;
        let answer = {
            let draft = self.draft.draft.lock().map_err(|_| ToolDenied::Denied)?;
            if draft.submitted {
                return Err(ToolDenied::Denied);
            }
            draft.answer.clone()
        };
        let planned = plan(&self.draft.checkout, &answer, allowed).map_err(|rejection| {
            ToolDenied::DraftRejected(format!("{} ({})", rejection.code, rejection.detail))
        })?;
        let draft_sha256 = digest(serde_json::to_vec(&answer).map_err(|_| ToolDenied::Denied)?);
        let mut worker = self.worker.lock().map_err(|_| ToolDenied::Denied)?;
        let preview = worker
            .check_staged(&planned, args.suite)
            .map_err(|outcome| {
                if outcome == Outcome::ModelExhausted {
                    ToolDenied::StagedCheckLimit
                } else {
                    ToolDenied::Denied
                }
            })?;
        let check = preview.check;
        let passed = check.passed(args.suite);
        let draft_current = {
            let mut draft = self.draft.draft.lock().map_err(|_| ToolDenied::Denied)?;
            draft.record_staged_check(&draft_sha256, args.suite, passed)
        };
        Ok(
            json!({"draft_sha256":draft_sha256,"tree_sha256":preview.tree_sha256,"draft_current":draft_current,"suite":check.suite,"command":check.command,"exit_code":check.exit_code,"timed_out":check.timed_out,"passed":passed,"output_excerpt":check.output_excerpt}),
        )
    }
}
impl PortableTool for RunChecksTool {
    const NAME: &'static str = "run_checks";
    type Args = CheckArgs;
    type Output = Value;
    type Error = ToolDenied;
    fn description(&self) -> String {
        "Run one predefined fixed check suite for an issued build ID in confinement.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"build_id":{"type":"string"},"suite":{"type":"string","enum":["protected_compatibility","additive_tests","owning_crate","workspace_format","workspace_check","workspace_tests"]}},"required":["build_id","suite"],"additionalProperties":false})
    }
    async fn call(&self, args: CheckArgs) -> Result<Value, ToolDenied> {
        self.tools.charge()?;
        let mut worker = self.worker.lock().map_err(|_| ToolDenied::Denied)?;
        require_issued_build_id(worker.has_issued_build_id(&args.build_id))?;
        let result = worker
            .fixed_check(&args.build_id, args.suite)
            .map_err(|_| ToolDenied::Denied)?;
        serde_json::to_value(result).map_err(|_| ToolDenied::Denied)
    }
}
use crate::repair::{
    Agents, AttemptSettings, Investigation, Observation, Outcome, PatchFile, SyntheticReduction,
    check_edits, snapshot,
};

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Deserialize)]
struct InvestigationAnswer {
    result_ids: Vec<String>,
    causal_hypothesis: String,
    mechanism: String,
    allowed_files: Vec<String>,
    compatibility_checks: Vec<String>,
    reduction: SyntheticReduction,
}

#[derive(Clone)]
struct SubmitInvestigationTool {
    tools: SourceTools,
    submitted: Arc<Mutex<Option<Value>>>,
    max_bytes: usize,
}

impl PortableTool for SubmitInvestigationTool {
    const NAME: &'static str = "submit_investigation";
    type Args = Value;
    type Output = Value;
    type Error = ToolDenied;

    fn description(&self) -> String {
        "Submit the complete investigation handoff with result_ids, causal_hypothesis, mechanism, allowed_files, compatibility_checks, and reduction. This is the final handoff; the harness checks its evidence and scope.".into()
    }

    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"result_ids":{"type":"array","items":{"type":"string"}},"causal_hypothesis":{"type":"string"},"mechanism":{"type":"string"},"allowed_files":{"type":"array","items":{"type":"string"}},"compatibility_checks":{"type":"array","items":{"type":"string"}},"reduction":{"type":"object","properties":{"csv":{"type":"string"},"prompt":{"type":"string"},"sql":{"type":"string"}},"required":["csv","prompt","sql"],"additionalProperties":false}},"required":["result_ids","causal_hypothesis","mechanism","allowed_files","compatibility_checks","reduction"],"additionalProperties":false})
    }

    async fn call(&self, args: Value) -> Result<Value, ToolDenied> {
        self.tools.charge()?;
        if serde_json::to_vec(&args)
            .map_err(|_| ToolDenied::Denied)?
            .len()
            > self.max_bytes
        {
            return Err(ToolDenied::DraftRejected(
                "handoff exceeds output byte limit".into(),
            ));
        }
        serde_json::from_value::<InvestigationAnswer>(args.clone())
            .map_err(|_| ToolDenied::DraftRejected("handoff fields are invalid".into()))?;
        let mut submitted = self.submitted.lock().map_err(|_| ToolDenied::Denied)?;
        if submitted.is_some() {
            return Err(ToolDenied::Denied);
        }
        *submitted = Some(args);
        Ok(json!({"submitted":true}))
    }
}

#[derive(Deserialize)]
struct ReviewAnswer {
    pass: bool,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StagedAnswer {
    submit_staged: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CodingAnswer {
    Staged(StagedAnswer),
    Inline(CodeAnswer),
}

pub struct RigRepairAgents {
    model: DynModel<Completion>,
    settings: AttemptSettings,
    checkout: PathBuf,
    target_prompt: String,
    redactor: Redactor,
    progress: Option<RepairProgress>,
    retry_events: RetryEvents,
    worker: Arc<Mutex<LiveWorker>>,
    reduced_csv: String,
    reduced_prompt: String,
    reduced_sql: String,
    hypothesis: Option<Investigation>,
    failure_reason: Option<String>,
    completed_after_budget_stages: Vec<&'static str>,
    open_questions: Vec<String>,
    /// The verified edit plan applied by `code`, kept so review can show a diff.
    applied: Option<Planned>,
    require_staged_check: bool,
}

impl RigRepairAgents {
    pub fn new(
        model: DynModel<Completion>,
        settings: AttemptSettings,
        checkout: PathBuf,
        target_prompt: String,
        redactor: Redactor,
        worker: Arc<Mutex<LiveWorker>>,
    ) -> Self {
        Self {
            model,
            settings,
            checkout,
            target_prompt,
            redactor,
            progress: None,
            retry_events: RetryEvents::default(),
            worker,
            reduced_csv: String::new(),
            reduced_prompt: String::new(),
            reduced_sql: String::new(),
            hypothesis: None,
            failure_reason: None,
            completed_after_budget_stages: Vec::new(),
            open_questions: Vec::new(),
            applied: None,
            require_staged_check: true,
        }
    }

    pub fn with_progress(mut self, progress: RepairProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn with_retry_events(mut self, events: RetryEvents) -> Self {
        self.retry_events = events;
        self
    }

    fn ask<T: DeserializeOwned>(
        &mut self,
        stage: &'static str,
        role: &str,
        prompt: String,
        root: PathBuf,
        scopes: &[&str],
        coding_draft: Option<Arc<Mutex<DraftState>>>,
    ) -> Result<T, Outcome> {
        self.failure_reason = None;
        self.open_questions.clear();
        let max_agent_turns = if stage == "investigation" {
            self.settings.max_investigator_turns
        } else {
            self.settings.max_agent_turns
        };
        let trace_path = self
            .checkout
            .parent()
            .ok_or(Outcome::EnvironmentFailure)?
            .join("agent-events.jsonl");
        let trace = AgentTrace::open(&trace_path, stage)
            .map(|t| t.with_redactor(self.redactor.clone()))
            .map_err(|_| {
                self.failure_reason = Some(format!("{stage}: logging_io_error"));
                Outcome::EnvironmentFailure
            })?;
        let trace = match &self.progress {
            Some(progress) => trace.with_progress(progress.clone()),
            None => trace,
        };
        trace.event("agent_started", json!({"max_agent_turns":max_agent_turns,"max_tool_calls":self.settings.max_tool_calls,"api_request_timeout_seconds":self.settings.api_request_timeout_seconds,"max_output_bytes":self.settings.max_output_bytes,"max_read_bytes":self.settings.max_read_bytes,"max_outline_bytes":self.settings.max_outline_bytes,"max_search_matches":self.settings.max_search_matches,"max_exposed_bytes":self.settings.max_exposed_bytes,"navigation_tools":true}));
        let model = self.model.clone();
        let nav = SourceSession::new(
            root.clone(),
            scopes,
            SourceLimits::from_settings(&self.settings),
        );
        let prompt = if stage == "investigation" {
            initial_investigation_prompt(&nav, &self.settings, prompt, &trace)
        } else {
            prompt
        };
        let tools = SourceTools { nav: nav.clone() };
        let submitted_investigation =
            (stage == "investigation").then(|| Arc::new(Mutex::new(None::<Value>)));
        let probe = ProbeTool {
            tools: tools.clone(),
            worker: self.worker.clone(),
            redactor: self.redactor.clone(),
        };
        let checks = RunChecksTool {
            tools: tools.clone(),
            worker: self.worker.clone(),
        };
        let draft_tools = coding_draft.as_ref().map(|draft| DraftTools {
            tools: tools.clone(),
            checkout: root.clone(),
            draft: draft.clone(),
            max_bytes: self.settings.max_output_bytes,
        });
        let limits = StageLimits {
            max_agent_turns: max_agent_turns as usize,
            max_bytes: self.settings.max_output_bytes,
            api_request_timeout_seconds: self.settings.api_request_timeout_seconds,
            answer_shape: answer_shape(stage),
            reserved_final_max_tokens: (stage == "coding").then_some(CODING_FINAL_MAX_TOKENS),
            recover_output_limit: stage == "coding",
        };
        if stage == "coding" {
            trace.event("model_reasoning_effort", json!({"value":"low"}));
        }
        let hook = trace.clone();
        let run_trace = trace.clone();
        let observer = nav.clone();
        let recovery_draft = coding_draft.clone();
        let staged_worker = self.worker.clone();
        let investigation_handoff = submitted_investigation.clone();
        let mut result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let snapshot = RequestSnapshot::default();
                let base = coding_reasoning_effort(AgentBuilder::new(model), stage)
                    .preamble(role)
                    .add_hook(hook)
                    .add_hook(snapshot.clone())
                    .temperature(0.2)
                    .max_tokens(stage_max_tokens(stage));
                let mut builder = base
                    .tool(OutlineTool(nav.clone()))
                    .tool(ReadLinesTool(nav.clone()))
                    .tool(SearchTextTool(nav))
                    .tool(probe);
                if stage != "coding" {
                    builder = builder.tool(checks);
                }
                if let Some(draft_tools) = draft_tools {
                    builder = builder
                        .tool(StageEditTool(draft_tools.clone()))
                        .tool(StageNewFileTool(draft_tools.clone()))
                        .tool(CheckStagedTool {
                            draft: draft_tools.clone(),
                            worker: staged_worker,
                        })
                        .tool(SubmitStagedTool(draft_tools));
                }
                if let Some(submitted) = investigation_handoff {
                    builder = builder.tool(SubmitInvestigationTool {
                        tools: tools.clone(),
                        submitted,
                        max_bytes: limits.max_bytes,
                    });
                }
                let agent = builder.build();
                run_stage::<T>(
                    &agent,
                    prompt,
                    limits,
                    &run_trace,
                    Some(&snapshot),
                    recovery_draft.as_ref(),
                )
                .await
            })
        });
        if (matches!(result, Ok(Finish::Answer(..) | Finish::Insufficient(_)))
            || matches!(
                result,
                Err((
                    "invalid_final_json"
                        | "output_token_limit"
                        | "max_agent_turns"
                        | "final_answer_tool_call",
                    _
                ))
            ))
            && let Some(value) = submitted_investigation
                .as_ref()
                .and_then(|slot| slot.lock().ok())
                .and_then(|slot| slot.clone())
            && let Ok(answer) = serde_json::from_value::<T>(value)
        {
            trace.event(
                "submitted_investigation_used",
                json!({"prior_cause":result.as_ref().err().map(|failure| failure.0)}),
            );
            result = Ok(Finish::Answer(answer, false));
        }
        let submitted = coding_draft
            .as_ref()
            .and_then(|draft| draft.lock().ok())
            .is_some_and(|draft| draft.submitted);
        if submitted
            && (matches!(result, Ok(Finish::Insufficient(_)))
                || matches!(
                    result,
                    Err((
                        "output_token_limit" | "invalid_final_json" | "max_agent_turns",
                        _
                    ))
                ))
            && let Ok(answer) = serde_json::from_value(json!({"submit_staged":true}))
        {
            trace.event(
                "submitted_patch_survived_final_answer_failure",
                json!({"cause":result.as_ref().err().map(|e| e.0)}),
            );
            result = Ok(Finish::Answer(answer, false));
        }
        let (cause, limit) = finish_cause(&result);
        for event in self.retry_events.drain() {
            trace.event(
                "provider_request_retry",
                json!({"attempts":event.attempts,"transport_result":event.transport_result,"initial_http_status":event.initial_http_status,"final_http_status":event.final_http_status,"attempt_http_statuses":event.attempt_http_statuses,"request_body_bytes":event.request_body_bytes}),
            );
        }
        let logged = trace.finish(cause, limit, observer.calls_charged());
        if !logged {
            self.failure_reason = Some(format!("{stage}: logging_io_error; agent cause={cause}"));
            return Err(Outcome::EnvironmentFailure);
        }
        match result {
            Ok(Finish::Answer(value, after_budget)) => {
                if after_budget {
                    self.completed_after_budget_stages.push(stage);
                }
                Ok(value)
            }
            Ok(Finish::Insufficient(questions)) => {
                self.open_questions = redact_questions(&self.redactor, &questions);
                self.failure_reason = Some(format!("{stage}: insufficient_evidence"));
                Err(insufficient_outcome(stage))
            }
            Err((cause, limit)) => {
                let status = matches!(cause, "http" | "api_request_timeout" | "provider_response")
                    .then(|| {
                        format!(
                            "; http_status={}",
                            trace
                                .failure_http_status()
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "unknown".to_owned())
                        )
                    })
                    .unwrap_or_default();
                self.failure_reason = Some(format!(
                    "{stage}: {cause}{}{status}; tool_budget_exhausted={}",
                    limit.map(|n| format!(" (limit={n})")).unwrap_or_default(),
                    observer.budget_denied()
                ));
                Err(failure_outcome(cause))
            }
        }
    }
}

fn coding_reasoning_effort(builder: AgentBuilder, stage: &str) -> AgentBuilder {
    if stage == "coding" {
        builder.additional_params(json!({"reasoning_effort":"low"}))
    } else {
        builder
    }
}

fn json_object_params() -> Map<String, Value> {
    Map::from_iter([("response_format".into(), json!({"type":"json_object"}))])
}

fn failure_outcome(cause: &str) -> Outcome {
    if matches!(cause, "http" | "api_request_timeout" | "provider_response") {
        Outcome::EnvironmentFailure
    } else {
        Outcome::ModelExhausted
    }
}

fn finish_cause<T>(result: &Result<Finish<T>, Failure>) -> Failure {
    match result {
        Ok(Finish::Answer(_, false)) => ("completed", None),
        Ok(Finish::Answer(_, true)) => ("completed_after_budget", None),
        Ok(Finish::Insufficient(_)) => ("insufficient_evidence", None),
        Err(failure) => *failure,
    }
}

/// Insufficient evidence never yields acceptance: review cannot complete, and
/// the other stages are out of model attempts.
fn insufficient_outcome(stage: &str) -> Outcome {
    if stage == "review" {
        Outcome::ReviewIncomplete
    } else {
        Outcome::ModelExhausted
    }
}

/// Prompt text stating the navigation tools and the budgets they run under.
fn navigation_note(s: &AttemptSettings, stage: &str, paths: &str) -> String {
    let turns = if stage == "investigation" {
        s.max_investigator_turns
    } else {
        s.max_agent_turns
    };
    format!(
        "Source navigation: the initial source map lists allowed files and item spans; call outline(path) for more of a file, read_lines(path, ranges) for inclusive 1-based ranges (large bodies may require pagination), and search_text(query) to locate code; {paths}. Budgets: at most {} tool-using model turns (the last of {} model calls is reserved for your final answer), {} tool calls in total, {} bytes of source text in total; read_lines returns at most {} bytes per call and an outline page at most {} bytes. Tool errors state a code and the next step. If evidence is insufficient, answer {{\"insufficient_evidence\":true,\"open_questions\":[up to 5 short strings without data values]}} instead.",
        turns.saturating_sub(1),
        turns,
        s.max_tool_calls,
        s.max_exposed_bytes,
        s.max_read_bytes,
        s.max_outline_bytes,
    )
}

/// Instruction for the reserved call made after the tool-enabled run ends at
/// its turn limit. It grants no authority beyond the stage's normal answer.
const FINAL_ANSWER_PROMPT: &str = "The turn budget is exhausted and tools are no longer available. Return the final JSON answer now, using only what you have already seen. If the evidence is insufficient, return {\"insufficient_evidence\":true,\"open_questions\":[up to 5 short strings without data values]} instead. Do not call tools.";
const CODING_FINAL_ANSWER_PROMPT: &str = "The turn budget is exhausted. If the current staged patch is complete and check_staged(additive_tests) passed for this exact draft, return only {\"submit_staged\":true} to submit it. Otherwise return {\"insufficient_evidence\":true,\"open_questions\":[\"patch was not checked or complete\"]}. Do not call tools or repeat staged content.";
const MAX_OPEN_QUESTIONS: usize = 5;
const MAX_QUESTION_CHARS: usize = 200;
/// Reasoning models spend many output tokens before answering, so the
/// investigation and coding stages (including coding's reserved final call)
/// get the larger cap; a length-truncated turn would otherwise end the stage.
const REASONING_STAGE_MAX_TOKENS: u64 = 16_384;
const OTHER_STAGE_MAX_TOKENS: u64 = 8_192;
const CODING_FINAL_MAX_TOKENS: u64 = REASONING_STAGE_MAX_TOKENS;

fn stage_max_tokens(stage: &str) -> u64 {
    match stage {
        "investigation" | "coding" => REASONING_STAGE_MAX_TOKENS,
        _ => OTHER_STAGE_MAX_TOKENS,
    }
}

const CORE_SCOPE: &str = "crates/baho-core/src/";
const CORE_TESTS_SCOPE: &str = "crates/baho-core/tests/";
const INITIAL_MAP_MAX_BYTES: usize = 12 * 1024;

fn initial_investigation_prompt(
    nav: &SourceSession,
    settings: &AttemptSettings,
    prompt: String,
    trace: &AgentTrace,
) -> String {
    let map = nav.initial_outline_map(INITIAL_MAP_MAX_BYTES.min(settings.max_exposed_bytes / 4));
    trace.event("initial_source_map", json!({"bytes":map.len(),"source_bytes_charged":nav.exposed_bytes(),"tool_calls_charged":nav.calls_charged()}));
    format!("{prompt}\n\n{map}")
}

fn investigation_prompt(
    target: &Observation,
    target_prompt: &str,
    settings: &AttemptSettings,
) -> String {
    format!(
        "Investigate this reproduced Role text-equality mismatch. Submit the completed handoff with submit_investigation; alternatively return one JSON object with result_ids, causal_hypothesis, mechanism, allowed_files, compatibility_checks, reduction. Reduction has csv, prompt, sql fields only; use a small neutral synthetic CSV. The only supported SQL is {PROBE_SQL_TEMPLATE}; replace VALUE with a value present in the synthetic Role column, double embedded single quotes, and add no semicolon or clauses. The harness computes expected rows. The result_ids must include the observed result. Target prompt: {target_prompt:?}. Baseline result ID: {}. Baseline build_id: {}. Pass this exact build_id to probe and run_checks; it is an issued opaque identifier, so do not derive it from the revision, result ID, or binary hash and do not shorten it. Baseline plan: {:?}. Allowed production scope: crates/baho-core/src/. {}",
        target.result_id,
        target.build_id,
        target.plan,
        navigation_note(settings, "investigation", CORE_PATHS),
    )
}
const CORE_PATHS: &str = "paths are root-relative under crates/baho-core/src/";
const REVIEW_SCOPES: [&str; 3] = [
    "candidate/crates/baho-core/src/",
    "candidate/crates/baho-core/tests/",
    "baseline/crates/baho-core/src/",
];
const REVIEW_PATHS: &str = "paths start with candidate/ (the patched tree, including tests) or baseline/ (the pre-patch text of each edited file only), e.g. candidate/crates/baho-core/src/...; search_text may list a line in both trees";
const DIFF_CONTEXT_LINES: usize = 3;
const MAX_REVIEW_DIFF_BYTES: usize = 24 * 1024;

type Failure = (&'static str, Option<u64>);

#[derive(Clone, Copy)]
struct StageLimits {
    max_agent_turns: usize,
    max_bytes: usize,
    api_request_timeout_seconds: u64,
    answer_shape: &'static str,
    reserved_final_max_tokens: Option<u64>,
    recover_output_limit: bool,
}

#[derive(Clone, Default)]
struct RequestSnapshot(Arc<Mutex<Option<(Vec<Message>, usize)>>>);

impl RequestSnapshot {
    fn take(&self) -> Option<(Vec<Message>, usize)> {
        self.0.lock().ok()?.take()
    }
}

impl AgentHook for RequestSnapshot {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if let Ok(mut slot) = self.0.lock() {
            let mut history = event.history.to_vec();
            history.push(event.prompt.clone());
            *slot = Some((history, event.turn));
        }
        CompletionCallAction::Continue
    }
}

const CODING_LENGTH_RECOVERY_PROMPT: &str = "Your last response reached the output token limit before producing an answer. Continue from the previous tool results. Stage a concrete edit or test with the available tools, then submit the staged patch. Keep tool arguments small. If you cannot safely complete the patch within the remaining budget, return {\"insufficient_evidence\":true,\"open_questions\":[\"patch was not complete\"]}. Do not repeat source text or draft a patch in prose.";

fn answer_shape(stage: &str) -> &'static str {
    match stage {
        "investigation" => {
            "{\"result_ids\":[string],\"causal_hypothesis\":string,\"mechanism\":string,\"allowed_files\":[string],\"compatibility_checks\":[string],\"reduction\":{\"csv\":string,\"prompt\":string,\"sql\":string}}"
        }
        "coding" => "{\"submit_staged\":true}",
        "review" => "{\"pass\":boolean,\"reason\":string}",
        _ => "a JSON object with the stage's required fields and types",
    }
}

enum Finish<T> {
    /// The stage's answer, and whether it came from the reserved final call.
    Answer(T, bool),
    Insufficient(Vec<String>),
}

/// Runs one stage. The tool-enabled run gets `max_agent_turns - 1` model calls
/// so that one call remains for the reserved final answer.
async fn run_stage<T: DeserializeOwned>(
    agent: &Agent,
    prompt: String,
    limits: StageLimits,
    trace: &AgentTrace,
    snapshot: Option<&RequestSnapshot>,
    coding_draft: Option<&Arc<Mutex<DraftState>>>,
) -> Result<Finish<T>, Failure> {
    let reserve = limits.max_agent_turns > 0;
    let tool_turns = limits.max_agent_turns.saturating_sub(usize::from(reserve));
    match agent.prompt(prompt).max_turns(tool_turns).await {
        Ok(response) => {
            let decoded = decode_answer(response.output(), limits.max_bytes, false);
            if !matches!(decoded, Err(("invalid_final_json", _))) {
                return decoded;
            }
            let correction = correction_prompt(response.output(), limits.answer_shape);
            let validation = invalid_answer_category(response.output());
            // The tool-enabled run uses at most max_agent_turns - 1 calls.
            // Reuse its transcript for one corrective call, without tools.
            let Some(history) = response.messages else {
                return decoded;
            };
            if response.completion_calls.len() >= limits.max_agent_turns {
                return decoded;
            }
            trace.event(
                "final_answer_correction_requested",
                json!({"cause":"invalid_final_json","validation":validation,"tool_choice":"none","response_format":"json_object"}),
            );
            let (corrected, corrected_validation) = match agent
                .prompt(correction)
                .history(history)
                .tool_choice(ToolChoice::None)
                .merge_additional_params(json_object_params())
                .max_turns(1)
                .await
            {
                Ok(answer) => {
                    let decoded = decode_answer(answer.output(), limits.max_bytes, false);
                    let validation = matches!(decoded, Err(("invalid_final_json", _)))
                        .then(|| invalid_answer_category(answer.output()));
                    (decoded, validation)
                }
                Err(PromptError::UnknownToolCall { .. } | PromptError::MaxTurnsError { .. }) => {
                    (Err(("final_answer_tool_call", None)), None)
                }
                Err(error) => (Err(stage_failure(&error, limits, trace)), None),
            };
            let outcome = match &corrected {
                Ok(Finish::Answer(..)) => "answer",
                Ok(Finish::Insufficient(_)) => "insufficient_evidence",
                Err((cause, _)) => cause,
            };
            trace.event(
                "final_answer_correction_finished",
                json!({"outcome":outcome,"validation":corrected_validation}),
            );
            corrected
        }
        Err(PromptError::MaxTurnsError { chat_history, .. }) if reserve => {
            trace.event(
                "final_answer_requested",
                json!({"cause":"max_agent_turns","history_messages":chat_history.len(),"tool_choice":"none"}),
            );
            // Tools stay declared (some providers reject tool-call history
            // without them) but Rig's `ToolChoice::None` leaves no tool
            // callable for this run, so a tool call fails as unknown.
            let mut final_run = agent
                .prompt(if limits.reserved_final_max_tokens.is_some() {
                    CODING_FINAL_ANSWER_PROMPT
                } else {
                    FINAL_ANSWER_PROMPT
                })
                .history(chat_history)
                .tool_choice(ToolChoice::None)
                .merge_additional_params(json_object_params())
                .max_turns(1);
            if let Some(max_tokens) = limits.reserved_final_max_tokens {
                final_run = final_run.max_tokens(max_tokens);
            }
            let finished = match final_run.await {
                Ok(response) => decode_answer(response.output(), limits.max_bytes, true),
                Err(PromptError::UnknownToolCall { .. } | PromptError::MaxTurnsError { .. }) => {
                    Err(("final_answer_tool_call", None))
                }
                Err(error) => Err(stage_failure(&error, limits, trace)),
            };
            let outcome = match &finished {
                Ok(Finish::Answer(..)) => "answer",
                Ok(Finish::Insufficient(_)) => "insufficient_evidence",
                Err((cause, _)) => cause,
            };
            trace.event("final_answer_finished", json!({"outcome":outcome}));
            finished
        }
        Err(error) => {
            let failure = stage_failure(&error, limits, trace);
            if failure.0 != "output_token_limit" || !limits.recover_output_limit {
                return Err(failure);
            }
            if coding_draft
                .and_then(|draft| draft.lock().ok())
                .is_some_and(|draft| draft.submitted)
            {
                return Err(failure);
            }
            let Some((history, used_turns)) = snapshot.and_then(RequestSnapshot::take) else {
                return Err(failure);
            };
            let remaining = limits.max_agent_turns.saturating_sub(used_turns);
            if remaining == 0 {
                return Err(failure);
            }
            trace.event(
                "output_limit_recovery_requested",
                json!({"cause":"output_token_limit","history_messages":history.len(),"remaining_model_calls":remaining}),
            );
            let recovered = match agent
                .prompt(CODING_LENGTH_RECOVERY_PROMPT)
                .history(history)
                .max_turns(remaining.saturating_sub(1))
                .await
            {
                Ok(response) => decode_answer(response.output(), limits.max_bytes, false),
                Err(PromptError::MaxTurnsError { chat_history, .. }) => {
                    trace.event(
                        "final_answer_requested",
                        json!({"cause":"max_agent_turns","history_messages":chat_history.len(),"tool_choice":"none","after_output_limit_recovery":true}),
                    );
                    let final_run = agent
                        .prompt(CODING_FINAL_ANSWER_PROMPT)
                        .history(chat_history)
                        .tool_choice(ToolChoice::None)
                        .merge_additional_params(json_object_params())
                        .max_tokens(CODING_FINAL_MAX_TOKENS)
                        .max_turns(1)
                        .await;
                    match final_run {
                        Ok(response) => decode_answer(response.output(), limits.max_bytes, true),
                        Err(
                            PromptError::UnknownToolCall { .. } | PromptError::MaxTurnsError { .. },
                        ) => Err(("final_answer_tool_call", None)),
                        Err(error) => Err(stage_failure(&error, limits, trace)),
                    }
                }
                Err(error) => Err(stage_failure(&error, limits, trace)),
            };
            let outcome = match &recovered {
                Ok(Finish::Answer(..)) => "answer",
                Ok(Finish::Insufficient(_)) => "insufficient_evidence",
                Err((cause, _)) => cause,
            };
            trace.event("output_limit_recovery_finished", json!({"outcome":outcome}));
            recovered
        }
    }
}

fn stage_failure(error: &PromptError, limits: StageLimits, trace: &AgentTrace) -> Failure {
    let status = match error {
        PromptError::Report(report) => report.http_status,
        PromptError::CompletionError(error) => {
            rig::core::error::ErrorReport::from(error).http_status
        }
        _ => None,
    };
    trace.record_failure_http_status(status);
    // Rig returns an answerless length-truncated turn as CompletionError and
    // does not expose its accumulated history for a bounded final-answer call.
    if matches!(error, PromptError::CompletionError(_)) && trace.last_turn_hit_output_limit() {
        return ("output_token_limit", None);
    }
    prompt_failure(error, limits)
}

fn prompt_failure(error: &PromptError, limits: StageLimits) -> Failure {
    let cause = prompt_error_cause(error);
    let limit = match error {
        PromptError::MaxTurnsError { max_turns, .. } => Some(*max_turns as u64),
        _ if cause == "api_request_timeout" => Some(limits.api_request_timeout_seconds),
        _ => None,
    };
    (cause, limit)
}

fn prompt_error_cause(error: &PromptError) -> &'static str {
    match error {
        PromptError::MaxTurnsError { .. } => "max_agent_turns",
        PromptError::CompletionError(ProviderError::Http(http))
            if matches!(http.as_ref(), HttpError::Instance(source)
                if source.downcast_ref::<rig::rig_reqwest::reqwest::Error>()
                    .is_some_and(rig::rig_reqwest::reqwest::Error::is_timeout)) =>
        {
            "api_request_timeout"
        }
        PromptError::CompletionError(_) => "provider_error",
        PromptError::Report(report) if report.kind == ErrorKind::Timeout => "api_request_timeout",
        PromptError::Report(report) => report.kind.code(),
        PromptError::MemoryError(_) => "memory_error",
        PromptError::PromptCancelled { .. } => "agent_cancelled",
        PromptError::UnknownToolCall { .. } => "unknown_tool_call",
    }
}

fn decode_answer<T: DeserializeOwned>(
    output: &str,
    max_bytes: usize,
    after_budget: bool,
) -> Result<Finish<T>, Failure> {
    if output.len() > max_bytes {
        return Err(("max_output_bytes", Some(max_bytes as u64)));
    }
    let value: Value = serde_json::from_str(output).map_err(|_| ("invalid_final_json", None))?;
    if value.get("insufficient_evidence") == Some(&Value::Bool(true)) {
        let questions = value
            .get("open_questions")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .take(MAX_OPEN_QUESTIONS)
                    .map(|q| q.chars().take(MAX_QUESTION_CHARS).collect())
                    .collect()
            })
            .unwrap_or_default();
        return Ok(Finish::Insufficient(questions));
    }
    serde_json::from_value(value)
        .map(|answer| Finish::Answer(answer, after_budget))
        .map_err(|_| ("invalid_final_json", None))
}

fn invalid_answer_category(output: &str) -> &'static str {
    if serde_json::from_str::<Value>(output).is_ok() {
        "answer_shape"
    } else {
        "json_syntax"
    }
}

fn correction_prompt(output: &str, shape: &str) -> String {
    let feedback = match serde_json::from_str::<Value>(output) {
        Ok(_) => "The JSON object did not match the required fields or field types.".to_owned(),
        Err(error) => format!(
            "JSON syntax is invalid at line {}, column {}.",
            error.line(),
            error.column()
        ),
    };
    format!(
        "Your previous final answer could not be validated. {feedback} Return one corrected JSON object with this exact shape and types: {shape}. Do not call tools. Do not include Markdown or commentary. If the evidence is insufficient, return {{\"insufficient_evidence\":true,\"open_questions\":[up to 5 short strings without data values]}} instead."
    )
}

/// Model-written open questions are prose: any the redactor flags are replaced
/// by a hash and the violation kinds.
fn redact_questions(redactor: &Redactor, questions: &[String]) -> Vec<String> {
    questions
        .iter()
        .take(MAX_OPEN_QUESTIONS)
        .map(|question| {
            let kinds = redactor.scan_prose(question);
            if kinds.is_empty() {
                return question.clone();
            }
            let names: Vec<String> = kinds
                .iter()
                .filter_map(|k| serde_json::to_value(k).ok()?.as_str().map(str::to_owned))
                .collect();
            format!(
                "[withheld sha256:{} kinds:{}]",
                &digest(question)[..16],
                names.join(",")
            )
        })
        .collect()
}

fn allowed(path: &Path, present: bool) -> bool {
    let parts: Vec<_> = path.components().collect();
    if parts.iter().any(|c| !matches!(c, Component::Normal(_))) {
        return false;
    }
    let text = path.to_string_lossy();
    text.starts_with("crates/baho-core/src/")
        || (!present
            && text.starts_with("crates/baho-core/tests/")
            && text.ends_with(".rs")
            && text != "crates/baho-core/tests/repair_regression.rs")
}

fn verified_patch_hash(patch_sha256: &str, patch: &[PatchFile]) -> Result<(), Outcome> {
    let bytes = serde_json::to_vec(patch).map_err(|_| Outcome::ReviewIncomplete)?;
    (digest(&bytes) == patch_sha256)
        .then_some(())
        .ok_or(Outcome::ReviewIncomplete)
}

impl Agents for RigRepairAgents {
    fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }
    fn completed_after_budget_stages(&self) -> &[&'static str] {
        &self.completed_after_budget_stages
    }
    fn open_questions(&self) -> &[String] {
        &self.open_questions
    }
    fn investigate(&mut self, target: &Observation) -> Result<Investigation, Outcome> {
        let prompt = investigation_prompt(target, &self.target_prompt, &self.settings);
        let root = self.checkout.clone();
        let answer: InvestigationAnswer = self.ask("investigation", "You are the independent repair investigator. State one testable mechanism. Do not request scope expansion. Navigate source with outline, read_lines, and search_text; never request whole files. Return JSON only.", prompt, root, &[CORE_SCOPE], None)?;
        let handoff = Investigation {
            result_ids: answer.result_ids,
            causal_hypothesis: answer.causal_hypothesis,
            mechanism: answer.mechanism,
            allowed_files: answer.allowed_files,
            compatibility_checks: answer.compatibility_checks,
            reduction: Some(answer.reduction.clone()),
        };
        let combined = format!(
            "{}\n{}\n{}",
            answer.reduction.csv, answer.reduction.prompt, answer.reduction.sql
        );
        if !self.redactor.scan(&combined).is_empty() {
            return Err(Outcome::NeedsDesign);
        }
        if !handoff.result_ids.contains(&target.result_id)
            || handoff
                .allowed_files
                .iter()
                .any(|p| !allowed(Path::new(p), true))
        {
            return Err(Outcome::NeedsDesign);
        }
        self.reduced_csv = answer.reduction.csv;
        self.reduced_prompt = answer.reduction.prompt;
        self.reduced_sql = answer.reduction.sql;
        self.hypothesis = Some(handoff.clone());
        Ok(handoff)
    }

    fn code(&mut self, _: &Path, checkout: &Path) -> Result<(), Outcome> {
        if checkout != self.checkout {
            return Err(Outcome::EnvironmentFailure);
        }
        let handoff = self.hypothesis.as_ref().ok_or(Outcome::NeedsDesign)?;
        let prompt = format!(
            "Fix the frozen Role text-equality regression by anchored line-range edits; you never receive or return whole files. Read the lines you edit with read_lines first. Call stage_edit with {{\"path\":\"crates/baho-core/src/...\",\"baseline_sha256\":\"file_sha256 from read_lines\",\"start_line\":N,\"end_line\":M,\"old\":\"exact current text of lines N..M without the final line break\",\"new\":\"replacement text\"}} for each edit. Call stage_new_file with {{\"path\":\"crates/baho-core/tests/<name>.rs\",\"content\":\"...\"}} for each new test. You must get a passing check_staged({{\"suite\":\"additive_tests\"}}) for the exact current draft before submission; it checks the staged code in a confined copy and returns a draft hash. Fix compile errors by calling stage_edit again with the same anchor or stage_new_file again with the same path to replace staged content, then recheck the changed draft. Baseline run_checks results do not validate staged code. Once all edits and files are staged, call submit_staged({{}}), or return only {{\"submit_staged\":true}} as your final answer. Both submit the staged patch for harness validation. Lines are 1-based inclusive; edits in one file must not overlap. At most {MAX_EDITS} edits and {MAX_NEW_FILES} new files of {MAX_NEW_FILE_BYTES} bytes each. The harness rejects the whole patch if any hash, old text, or range differs. One submitted patch ends this stage. Hypothesis: {}. Mechanism: {}. Compatibility: {:?}. Allowed files: {:?}. Reduced CSV: {:?}. Prompt: {:?}. SQL: {:?}. {}",
            handoff.causal_hypothesis,
            handoff.mechanism,
            handoff.compatibility_checks,
            handoff.allowed_files,
            self.reduced_csv,
            self.reduced_prompt,
            self.reduced_sql,
            navigation_note(&self.settings, "coding", CORE_PATHS),
        );
        let draft = Arc::new(Mutex::new(DraftState {
            require_additive_check: self.require_staged_check,
            ..DraftState::default()
        }));
        let answer: CodingAnswer = self.ask("coding", "You are the repair coder. Edit only the allowed core source files by anchored line ranges and add tests under crates/baho-core/tests/. Do not alter expectations or protected files. Navigate with outline, read_lines, and search_text; stage edits, use check_staged to test the current draft, and submit once complete. Return short JSON only.", prompt, checkout.to_path_buf(), &[CORE_SCOPE, CORE_TESTS_SCOPE], Some(draft.clone()))?;
        let answer = {
            let state = draft.lock().map_err(|_| Outcome::EnvironmentFailure)?;
            if state.submitted {
                state.answer.clone()
            } else {
                match answer {
                    CodingAnswer::Inline(answer) if !self.require_staged_check => answer,
                    CodingAnswer::Inline(_) => {
                        self.failure_reason = Some("coding: inline_patch_requires_staging".into());
                        return Err(Outcome::Rejected);
                    }
                    CodingAnswer::Staged(marker) if marker.submit_staged => {
                        if !state.checked_current_draft() {
                            self.failure_reason = Some("coding: unchecked_draft".into());
                            return Err(Outcome::Rejected);
                        }
                        state.answer.clone()
                    }
                    CodingAnswer::Staged(_) => {
                        self.failure_reason = Some("coding: staged_patch_not_submitted".into());
                        return Err(Outcome::Rejected);
                    }
                }
            }
        };
        let before = snapshot(checkout).map_err(|_| Outcome::EnvironmentFailure)?;
        let planned = match plan(checkout, &answer, allowed) {
            Ok(planned) => planned,
            Err(rejection) => {
                self.failure_reason =
                    Some(format!("coding: {} ({})", rejection.code, rejection.detail));
                return Err(Outcome::Rejected);
            }
        };
        apply(checkout, &planned).map_err(|_| Outcome::EnvironmentFailure)?;
        let validation =
            snapshot(checkout).and_then(|after| check_edits(&before, &after, checkout).map(|_| ()));
        if validation.is_err() {
            restore(checkout, &planned).map_err(|_| Outcome::EnvironmentFailure)?;
            return Err(Outcome::Rejected);
        }
        self.applied = Some(planned);
        Ok(())
    }

    fn review(
        &mut self,
        patch_sha256: &str,
        patch: &[PatchFile],
        evidence: &[Observation],
    ) -> Result<bool, Outcome> {
        verified_patch_hash(patch_sha256, patch)?;
        let planned = self.applied.as_ref().ok_or(Outcome::ReviewIncomplete)?;
        if !patch_matches_plan(patch, planned) {
            return Err(Outcome::ReviewIncomplete);
        }
        let (diff, truncated) = render_diff(planned, DIFF_CONTEXT_LINES, MAX_REVIEW_DIFF_BYTES);
        let view = self
            .checkout
            .parent()
            .ok_or(Outcome::EnvironmentFailure)?
            .join("review-view");
        materialize_review_view(&self.checkout, &view, planned)
            .map_err(|_| Outcome::EnvironmentFailure)?;
        let cut = if truncated {
            format!(
                " (cut at {MAX_REVIEW_DIFF_BYTES} bytes; read the rest from the candidate tree)"
            )
        } else {
            String::new()
        };
        let prompt = format!(
            "Review this frozen repair. Return only JSON {{\"pass\":bool,\"reason\":string}}. Patch hash: {}. Unified diff{} with {} context lines; diff paths are repository paths:\n{}\nBefore/after observations: {:?}. Reduced CSV: {:?}; prompt: {:?}; SQL: {:?}. Check NL/SQL meaning, overfitting, preserved tests, and counterexamples. Reject uncertain semantics. {}",
            patch_sha256,
            cut,
            DIFF_CONTEXT_LINES,
            diff,
            evidence,
            self.reduced_csv,
            self.reduced_prompt,
            self.reduced_sql,
            navigation_note(&self.settings, "review", REVIEW_PATHS),
        );
        let answer: Result<ReviewAnswer, Outcome> = self.ask("review", "You are an independent reviewer with a fresh Rig history. You cannot edit source or expectations. Inspect the patch with the diff and the read-only outline, read_lines, and search_text tools over the baseline and candidate trees; never request whole files. Return JSON only.", prompt, view.clone(), &REVIEW_SCOPES, None);
        let _ = fs::remove_dir_all(&view);
        let answer = answer?;
        if answer.reason.trim().is_empty() {
            return Err(Outcome::ReviewIncomplete);
        }
        Ok(answer.pass)
    }
}

/// Every patch file must be exactly what the verified plan produced.
fn patch_matches_plan(patch: &[PatchFile], planned: &Planned) -> bool {
    patch.len() == planned.files.len() + planned.new_files.len()
        && patch.iter().all(|file| {
            planned
                .files
                .iter()
                .any(|f| f.path == file.path && f.updated == file.content)
                || planned
                    .new_files
                    .iter()
                    .any(|(path, content)| *path == file.path && *content == file.content)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_run::DiagnosticSummary;
    use crate::contracts::{
        PlanComparison, PlanEvidence, PlanLiteral, PlanOperator, TextMatchPolicy,
    };
    use crate::probe::{BodyObservation, OracleObservation, ProbeObservation, SyntheticRow};
    use std::time::Duration;

    use rig::test_utils::{MockCompletionModel, MockTurn};

    const SCOPE: &str = "crates/baho-core/src/";

    struct Harness {
        _dir: tempfile::TempDir,
        trace_path: PathBuf,
        trace: AgentTrace,
        session: SourceSession,
        model: MockCompletionModel,
        agent: Agent,
        snapshot: RequestSnapshot,
    }

    fn harness(turns: impl IntoIterator<Item = MockTurn>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let file = format!("{SCOPE}big.rs");
        fs::create_dir_all(dir.path().join(SCOPE)).unwrap();
        let mut source = String::new();
        for i in 0..40 {
            source.push_str(&format!(
                "pub fn item_{i}(x: u32) -> u32 {{\n    x + {i}\n}}\n\n"
            ));
        }
        fs::write(dir.path().join(&file), source).unwrap();
        let trace_path = dir.path().join("agent-events.jsonl");
        let trace = AgentTrace::open(&trace_path, "investigation")
            .unwrap()
            .with_redactor(Redactor::new(["zebra_literal"]).unwrap());
        let session = SourceSession::new(
            dir.path().to_path_buf(),
            &[SCOPE],
            SourceLimits {
                max_calls: 20,
                read_bytes: 1024,
                outline_bytes: 6 * 1024,
                search_matches: 40,
                stage_bytes: 48 * 1024,
            },
        );
        let model = MockCompletionModel::from_turns(turns);
        let snapshot = RequestSnapshot::default();
        let agent = AgentBuilder::new(model.clone())
            .add_hook(trace.clone())
            .add_hook(snapshot.clone())
            .max_tokens(OTHER_STAGE_MAX_TOKENS)
            .tool(OutlineTool(session.clone()))
            .tool(ReadLinesTool(session.clone()))
            .tool(SearchTextTool(session.clone()))
            .build();
        Harness {
            _dir: dir,
            trace_path,
            trace,
            session,
            model,
            agent,
            snapshot,
        }
    }

    impl Harness {
        async fn run(&self, max_agent_turns: usize) -> Result<Finish<Value>, Failure> {
            let limits = StageLimits {
                max_agent_turns,
                max_bytes: 4096,
                api_request_timeout_seconds: 600,
                answer_shape: answer_shape("review"),
                reserved_final_max_tokens: None,
                recover_output_limit: false,
            };
            let result = run_stage::<Value>(
                &self.agent,
                "private prompt".into(),
                limits,
                &self.trace,
                None,
                None,
            )
            .await;
            let (cause, limit) = finish_cause(&result);
            assert!(
                self.trace
                    .finish(cause, limit, self.session.calls_charged())
            );
            result
        }
        fn events(&self) -> Vec<Value> {
            fs::read_to_string(&self.trace_path)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    fn named<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
        events.iter().filter(|e| e["name"] == name).collect()
    }

    fn tool_turns(file: &str) -> Vec<MockTurn> {
        vec![
            MockTurn::tool_call("c1", "outline", json!({"path":file})),
            MockTurn::tool_call("c2", "search_text", json!({"query":"item_3"})),
            MockTurn::tool_call(
                "c3",
                "read_lines",
                json!({"path":file,"ranges":[{"start":1,"end":3}]}),
            ),
            MockTurn::tool_call("c4", "outline", json!({"path":file})),
        ]
    }

    fn turns_then(extra: MockTurn) -> Vec<MockTurn> {
        let mut turns = tool_turns("crates/baho-core/src/big.rs");
        turns.push(extra);
        turns
    }

    #[tokio::test]
    async fn decodes_turn_limits_provider_errors_and_bad_output() {
        let h = harness([MockTurn::text("{}")]);
        let limits = StageLimits {
            max_agent_turns: 0,
            max_bytes: 100,
            api_request_timeout_seconds: 600,
            answer_shape: answer_shape("review"),
            reserved_final_max_tokens: None,
            recover_output_limit: false,
        };
        let turns = run_stage::<Value>(
            &h.agent,
            "private prompt".into(),
            limits,
            &h.trace,
            None,
            None,
        )
        .await;
        assert_eq!(turns.err(), Some(("max_agent_turns", Some(0))));
        let h = harness([MockTurn::error("secret provider body")]);
        let provider = h.run(3).await.err().unwrap();
        assert!(matches!(provider.0, "provider" | "provider_error"));
        assert_eq!(
            decode_answer::<Value>("private malformed output", 100, false)
                .err()
                .unwrap()
                .0,
            "invalid_final_json"
        );
        assert_eq!(
            decode_answer::<Value>("12345", 4, false).err().unwrap(),
            ("max_output_bytes", Some(4))
        );
        assert!(matches!(
            decode_answer::<Value>("{\"a\":1}", 100, false),
            Ok(Finish::Answer(_, false))
        ));
    }

    #[tokio::test]
    async fn answerless_length_turn_has_output_limit_cause() {
        let h =
            harness([MockTurn::text("").with_finish_reason(rig::completion::FinishReason::Length)]);
        assert_eq!(h.run(3).await.err(), Some(("output_token_limit", None)));
        let events = h.events();
        assert_eq!(
            named(&events, "agent_finished")[0]["detail"]["cause"],
            "output_token_limit"
        );
        assert_eq!(h.model.requests().len(), 1);
    }

    #[tokio::test]
    async fn coding_length_recovery_keeps_history_and_remaining_tool_budget() {
        let h = harness([
            MockTurn::tool_call(
                "before",
                "outline",
                json!({"path":"crates/baho-core/src/big.rs"}),
            ),
            MockTurn::text("").with_finish_reason(rig::completion::FinishReason::Length),
            MockTurn::tool_call("after", "search_text", json!({"query":"item_3"})),
            MockTurn::text("{\"submit_staged\":true}"),
        ]);
        let limits = StageLimits {
            max_agent_turns: 4,
            max_bytes: 4096,
            api_request_timeout_seconds: 600,
            answer_shape: answer_shape("coding"),
            reserved_final_max_tokens: Some(CODING_FINAL_MAX_TOKENS),
            recover_output_limit: true,
        };
        let result = run_stage::<Value>(
            &h.agent,
            "private prompt".into(),
            limits,
            &h.trace,
            Some(&h.snapshot),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(result, Finish::Answer(_, true)));
        let requests = h.model.requests();
        assert_eq!(requests.len(), 4);
        let history = serde_json::to_string(&requests[2].chat_history).unwrap();
        assert!(history.contains("private prompt"));
        assert!(history.contains("outline_kind"));
        assert_eq!(h.session.calls_charged(), 2);
        let events = h.events();
        assert_eq!(named(&events, "output_limit_recovery_requested").len(), 1);
        assert_eq!(
            named(&events, "output_limit_recovery_finished")[0]["detail"]["outcome"],
            "answer"
        );
        assert!(
            !fs::read_to_string(&h.trace_path)
                .unwrap()
                .contains("private prompt")
        );
    }

    #[tokio::test]
    async fn coding_length_recovery_stops_after_second_truncation() {
        let h = harness([
            MockTurn::text("").with_finish_reason(rig::completion::FinishReason::Length),
            MockTurn::text("").with_finish_reason(rig::completion::FinishReason::Length),
        ]);
        let limits = StageLimits {
            max_agent_turns: 4,
            max_bytes: 4096,
            api_request_timeout_seconds: 600,
            answer_shape: answer_shape("coding"),
            reserved_final_max_tokens: Some(CODING_FINAL_MAX_TOKENS),
            recover_output_limit: true,
        };
        let result = run_stage::<Value>(
            &h.agent,
            "private prompt".into(),
            limits,
            &h.trace,
            Some(&h.snapshot),
            None,
        )
        .await;
        assert_eq!(result.err(), Some(("output_token_limit", None)));
        assert_eq!(h.model.requests().len(), 2);
        assert_eq!(
            named(&h.events(), "output_limit_recovery_requested").len(),
            1
        );
    }

    #[tokio::test]
    async fn coding_length_recovery_submits_the_same_staged_draft() {
        let h = harness([
            MockTurn::tool_call(
                "before",
                "outline",
                json!({"path":"crates/baho-core/src/big.rs"}),
            ),
            MockTurn::text("").with_finish_reason(rig::completion::FinishReason::Length),
            MockTurn::tool_call(
                "stage",
                "stage_new_file",
                json!({"path":"crates/baho-core/tests/recovery.rs","content":"#[test]\nfn recovery() {}\n"}),
            ),
            MockTurn::tool_call("submit", "submit_staged", json!({})),
            MockTurn::text("{\"submit_staged\":true}"),
        ]);
        let draft = Arc::new(Mutex::new(DraftState::default()));
        let draft_tools = DraftTools {
            tools: SourceTools {
                nav: h.session.clone(),
            },
            checkout: h._dir.path().to_path_buf(),
            draft: draft.clone(),
            max_bytes: 4096,
        };
        let snapshot = RequestSnapshot::default();
        let agent = AgentBuilder::new(h.model.clone())
            .add_hook(h.trace.clone())
            .add_hook(snapshot.clone())
            .tool(OutlineTool(h.session.clone()))
            .tool(StageNewFileTool(draft_tools.clone()))
            .tool(SubmitStagedTool(draft_tools))
            .build();
        let limits = StageLimits {
            max_agent_turns: 5,
            max_bytes: 4096,
            api_request_timeout_seconds: 600,
            answer_shape: answer_shape("coding"),
            reserved_final_max_tokens: Some(CODING_FINAL_MAX_TOKENS),
            recover_output_limit: true,
        };
        let result = run_stage::<CodingAnswer>(
            &agent,
            "private prompt".into(),
            limits,
            &h.trace,
            Some(&snapshot),
            Some(&draft),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Finish::Answer(CodingAnswer::Staged(_), true)
        ));
        let state = draft.lock().unwrap();
        assert!(state.submitted);
        assert_eq!(state.answer.new_files.len(), 1);
        assert_eq!(h.session.calls_charged(), 3);
        assert_eq!(
            named(&h.events(), "output_limit_recovery_requested")[0]["detail"]["remaining_model_calls"],
            3
        );
        let requests = h.model.requests();
        assert_eq!(requests.len(), 5);
        let final_request = &requests[4];
        assert_eq!(final_request.tool_choice, Some(ToolChoice::None));
        let history = serde_json::to_string(&final_request.chat_history).unwrap();
        assert!(history.contains("stage_new_file"));
        assert!(history.contains("submit_staged"));
    }

    #[tokio::test]
    async fn failed_staged_check_can_be_corrected_before_submission() {
        let h = harness([]);
        let draft = Arc::new(Mutex::new(DraftState {
            require_additive_check: true,
            ..DraftState::default()
        }));
        let tools = DraftTools {
            tools: SourceTools {
                nav: h.session.clone(),
            },
            checkout: h._dir.path().to_path_buf(),
            draft: draft.clone(),
            max_bytes: 4096,
        };
        let stage = StageNewFileTool(tools.clone());
        let submit = SubmitStagedTool(tools);
        let path = "crates/baho-core/tests/staged.rs";
        stage
            .call(NewFile {
                path: path.into(),
                content: "use baho_core::missing_import;\n".into(),
            })
            .await
            .unwrap();
        let failed_sha = digest(serde_json::to_vec(&draft.lock().unwrap().answer).unwrap());
        assert!(draft.lock().unwrap().record_staged_check(
            &failed_sha,
            SuiteId::AdditiveTests,
            false
        ));
        assert_eq!(
            submit.call(json!({})).await.unwrap_err().to_string(),
            ToolDenied::UncheckedDraft.to_string()
        );
        stage
            .call(NewFile {
                path: path.into(),
                content: "#[test] fn staged() {}\n".into(),
            })
            .await
            .unwrap();
        let corrected_sha = digest(serde_json::to_vec(&draft.lock().unwrap().answer).unwrap());
        assert_ne!(failed_sha, corrected_sha);
        assert!(!draft.lock().unwrap().record_staged_check(
            &failed_sha,
            SuiteId::AdditiveTests,
            true
        ));
        assert_eq!(
            submit.call(json!({})).await.unwrap_err().to_string(),
            ToolDenied::UncheckedDraft.to_string()
        );
        assert!(draft.lock().unwrap().record_staged_check(
            &corrected_sha,
            SuiteId::AdditiveTests,
            true
        ));
        assert_eq!(submit.call(json!({})).await.unwrap()["submitted"], true);
        let state = draft.lock().unwrap();
        assert_eq!(state.answer.new_files.len(), 1);
        assert_eq!(
            state.answer.new_files[0].content,
            "#[test] fn staged() {}\n"
        );
        assert!(state.submitted);
        assert!(!h._dir.path().join(path).exists());
    }

    #[tokio::test]
    async fn staged_edit_replacement_keeps_one_anchor_and_invalidates_check() {
        let h = harness([]);
        let path = "crates/baho-core/src/big.rs";
        h.session
            .read_lines(path, &[crate::source_tools::LineRange { start: 1, end: 1 }])
            .unwrap();
        let baseline_sha256 = digest(fs::read(h._dir.path().join(path)).unwrap());
        let draft = Arc::new(Mutex::new(DraftState {
            require_additive_check: true,
            ..DraftState::default()
        }));
        let stage = StageEditTool(DraftTools {
            tools: SourceTools {
                nav: h.session.clone(),
            },
            checkout: h._dir.path().to_path_buf(),
            draft: draft.clone(),
            max_bytes: 4096,
        });
        let edit = Edit {
            path: path.into(),
            baseline_sha256,
            start_line: 1,
            end_line: 1,
            old: "pub fn item_0(x: u32) -> u32 {".into(),
            new: "pub fn first(x: u32) -> u32 {".into(),
        };
        stage.call(edit.clone()).await.unwrap();
        let first_sha = digest(serde_json::to_vec(&draft.lock().unwrap().answer).unwrap());
        assert!(draft.lock().unwrap().record_staged_check(
            &first_sha,
            SuiteId::AdditiveTests,
            true
        ));
        let mut replacement = edit;
        replacement.new = "pub fn corrected(x: u32) -> u32 {".into();
        stage.call(replacement).await.unwrap();
        let state = draft.lock().unwrap();
        assert_eq!(state.answer.edits.len(), 1);
        assert_eq!(
            state.answer.edits[0].new,
            "pub fn corrected(x: u32) -> u32 {"
        );
        assert!(!state.checked_current_draft());
        assert!(!allowed(
            Path::new("crates/baho-core/tests/repair_regression.rs"),
            false
        ));
    }

    #[test]
    fn probe_sql_feedback_is_stable_and_contains_no_fixture_values() {
        let csv = "Id,Role\n1,zebra_literal\n2,analyst\n";
        assert!(
            validate_probe_sql(
                csv,
                "SELECT _row FROM data WHERE lower(\"Role\") = lower('analyst') ORDER BY _row"
            )
            .is_ok()
        );
        for sql in [
            "SELECT * FROM data WHERE Role = 'zebra_literal'",
            "SELECT _row FROM data WHERE lower(\"Role\") = lower('missing') ORDER BY _row",
        ] {
            let error = validate_probe_sql(csv, sql).unwrap_err().to_string();
            assert!(error.starts_with("invalid_sql:"));
            assert!(!error.contains("zebra_literal"));
            assert!(!error.contains("missing"));
        }
    }

    #[tokio::test]
    async fn timed_out_provider_request_has_named_cause_and_limit() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(150));
        });
        let client = rig::rig_reqwest::reqwest::Client::builder()
            .timeout(Duration::from_millis(30))
            .build()
            .unwrap();
        let error = client
            .get(format!("http://{address}/waiting"))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_timeout());
        let prompt =
            PromptError::CompletionError(ProviderError::Http(Arc::new(HttpError::instance(error))));
        assert_eq!(
            prompt_failure(
                &prompt,
                StageLimits {
                    max_agent_turns: 12,
                    max_bytes: 4096,
                    api_request_timeout_seconds: 600,
                    answer_shape: answer_shape("review"),
                    reserved_final_max_tokens: None,
                    recover_output_limit: false,
                }
            ),
            ("api_request_timeout", Some(600))
        );
        let PromptError::CompletionError(provider_error) = prompt else {
            unreachable!()
        };
        let report = rig::core::error::ErrorReport::from(&provider_error);
        assert_eq!(report.kind, ErrorKind::Http);
        assert_eq!(
            prompt_failure(
                &PromptError::Report(report),
                StageLimits {
                    max_agent_turns: 12,
                    max_bytes: 4096,
                    api_request_timeout_seconds: 600,
                    answer_shape: answer_shape("review"),
                    reserved_final_max_tokens: None,
                    recover_output_limit: false,
                }
            ),
            ("http", None)
        );
        assert_eq!(failure_outcome("http"), Outcome::EnvironmentFailure);
        assert_eq!(
            failure_outcome("api_request_timeout"),
            Outcome::EnvironmentFailure
        );
        assert_eq!(failure_outcome("max_agent_turns"), Outcome::ModelExhausted);
        server.join().unwrap();
    }

    #[test]
    fn provider_response_status_is_sanitized_and_inconclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-events.jsonl");
        let trace = AgentTrace::open(&path, "coding").unwrap();
        let mut report = rig::core::error::ErrorReport::new(
            ErrorKind::ProviderResponse,
            "private provider body",
        );
        report.http_status = Some(503);
        let failure = stage_failure(
            &PromptError::Report(report),
            StageLimits {
                max_agent_turns: 12,
                max_bytes: 4096,
                api_request_timeout_seconds: 600,
                answer_shape: answer_shape("coding"),
                reserved_final_max_tokens: Some(CODING_FINAL_MAX_TOKENS),
                recover_output_limit: false,
            },
            &trace,
        );
        assert_eq!(failure, ("provider_response", None));
        assert_eq!(trace.failure_http_status(), Some(503));
        assert_eq!(failure_outcome(failure.0), Outcome::EnvironmentFailure);
        let events = fs::read_to_string(path).unwrap();
        assert!(events.contains("\"http_status\":503"));
        assert!(!events.contains("private provider body"));
    }

    #[tokio::test]
    async fn reserves_one_tool_free_call_after_tool_turns() {
        // Four turns issue tools; the reserved call is the fifth and last.
        let h = harness(turns_then(MockTurn::text("{\"mechanism\":\"found\"}")));
        let result = h.run(5).await.unwrap();
        let Finish::Answer(value, true) = result else {
            panic!("expected an answer after the budget");
        };
        assert_eq!(value["mechanism"], "found");
        let requests = h.model.requests();
        assert_eq!(requests.len(), 5, "never more than max_agent_turns calls");
        assert!(requests[..4].iter().all(|r| r.tool_choice.is_none()));
        let last = &requests[4];
        assert_eq!(last.tool_choice, Some(ToolChoice::None));
        assert_eq!(
            last.additional_params.as_ref().unwrap()["response_format"],
            json!({"type":"json_object"})
        );
        assert!(!last.tools.is_empty(), "tools stay declared");
        let history = serde_json::to_string(&last.chat_history).unwrap();
        for id in ["c1", "c2", "c3", "c4"] {
            assert!(history.contains(id), "accumulated history keeps {id}");
        }
        assert!(history.contains("The turn budget is exhausted"));
        let events = h.events();
        assert_eq!(named(&events, "final_answer_requested").len(), 1);
        assert_eq!(
            named(&events, "final_answer_finished")[0]["detail"]["outcome"],
            "answer"
        );
        let finished = named(&events, "agent_finished")[0];
        assert_eq!(finished["detail"]["cause"], "completed_after_budget");
        assert!(
            !fs::read_to_string(&h.trace_path)
                .unwrap()
                .contains("private prompt")
        );
    }

    #[test]
    fn stage_max_tokens_gives_reasoning_stages_the_larger_cap() {
        assert_eq!(stage_max_tokens("investigation"), 16_384);
        assert_eq!(stage_max_tokens("coding"), 16_384);
        assert_eq!(stage_max_tokens("planning"), 8_192);
        assert_eq!(stage_max_tokens("review"), 8_192);
    }

    #[tokio::test]
    async fn coding_requests_low_reasoning_effort() {
        let model = MockCompletionModel::from_turns([MockTurn::text("done")]);
        let agent = coding_reasoning_effort(AgentBuilder::new(model.clone()), "coding").build();
        agent.prompt("code").await.unwrap();
        assert_eq!(
            model.requests()[0].additional_params,
            Some(json!({"reasoning_effort":"low"}))
        );

        let model = MockCompletionModel::from_turns([MockTurn::text("done")]);
        let agent = coding_reasoning_effort(AgentBuilder::new(model.clone()), "review").build();
        agent.prompt("review").await.unwrap();
        assert_eq!(model.requests()[0].additional_params, None);
    }

    #[tokio::test]
    async fn coding_reserved_answer_uses_larger_cap_without_repeating_tools() {
        let h = harness(turns_then(MockTurn::text(
            "{\"edits\":[],\"new_files\":[]}",
        )));
        let limits = StageLimits {
            max_agent_turns: 5,
            max_bytes: 65_536,
            api_request_timeout_seconds: 600,
            answer_shape: answer_shape("coding"),
            reserved_final_max_tokens: Some(CODING_FINAL_MAX_TOKENS),
            recover_output_limit: false,
        };
        let result = run_stage::<Value>(
            &h.agent,
            "private prompt".into(),
            limits,
            &h.trace,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(result, Finish::Answer(_, true)));
        let requests = h.model.requests();
        assert_eq!(requests.len(), 5);
        assert!(
            requests[..4]
                .iter()
                .all(|request| request.max_tokens == Some(OTHER_STAGE_MAX_TOKENS))
        );
        assert_eq!(requests[4].max_tokens, Some(CODING_FINAL_MAX_TOKENS));
        assert_eq!(requests[4].tool_choice, Some(ToolChoice::None));
        let events = h.events();
        assert_eq!(named(&events, "tool_call_finished").len(), 4);
        assert_eq!(named(&events, "final_answer_finished").len(), 1);
    }

    #[tokio::test]
    async fn tool_enabled_run_is_capped_below_max_agent_turns() {
        // The model would keep calling tools; only max_agent_turns - 1 are served.
        let h = harness(turns_then(MockTurn::text("{}")));
        h.run(3).await.err();
        assert_eq!(h.model.requests().len(), 3);
        let events = h.events();
        assert_eq!(named(&events, "tool_call_finished").len(), 2);
    }

    #[tokio::test]
    async fn normal_completion_makes_no_reserved_call() {
        let h = harness([MockTurn::text("{\"ok\":true}")]);
        assert!(matches!(h.run(5).await, Ok(Finish::Answer(_, false))));
        let events = h.events();
        assert!(named(&events, "final_answer_requested").is_empty());
        assert_eq!(
            named(&events, "agent_finished")[0]["detail"]["cause"],
            "completed"
        );
    }

    #[tokio::test]
    async fn malformed_final_answer_gets_one_tool_free_correction() {
        let h = harness([
            MockTurn::text("{\"pass\": tru"),
            MockTurn::text("{\"pass\":false,\"reason\":\"unsupported\"}"),
        ]);
        let limits = StageLimits {
            max_agent_turns: 3,
            max_bytes: 4096,
            api_request_timeout_seconds: 600,
            answer_shape: answer_shape("review"),
            reserved_final_max_tokens: None,
            recover_output_limit: false,
        };
        let result = run_stage::<ReviewAnswer>(
            &h.agent,
            "private prompt".into(),
            limits,
            &h.trace,
            None,
            None,
        )
        .await
        .unwrap();
        let Finish::Answer(answer, false) = result else {
            panic!("expected corrected review answer");
        };
        assert!(!answer.pass);
        assert_eq!(answer.reason, "unsupported");
        let requests = h.model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].tool_choice, Some(ToolChoice::None));
        assert_eq!(
            requests[1].additional_params.as_ref().unwrap()["response_format"],
            json!({"type":"json_object"})
        );
        let history = serde_json::to_string(&requests[1].chat_history).unwrap();
        assert!(history.contains("pass"));
        let events = h.events();
        assert_eq!(named(&events, "final_answer_correction_requested").len(), 1);
        assert_eq!(
            named(&events, "final_answer_correction_requested")[0]["detail"]["validation"],
            "json_syntax"
        );
        assert_eq!(
            named(&events, "final_answer_correction_finished")[0]["detail"]["outcome"],
            "answer"
        );
        assert!(
            !fs::read_to_string(&h.trace_path)
                .unwrap()
                .contains("private prompt")
        );
    }

    #[tokio::test]
    async fn invalid_schema_still_invalid_after_one_correction() {
        let h = harness([
            MockTurn::text("{\"pass\":\"yes\",\"reason\":3}"),
            MockTurn::text("{\"pass\":\"yes\",\"reason\":3}"),
        ]);
        let limits = StageLimits {
            max_agent_turns: 3,
            max_bytes: 4096,
            api_request_timeout_seconds: 600,
            answer_shape: answer_shape("review"),
            reserved_final_max_tokens: None,
            recover_output_limit: false,
        };
        let result = run_stage::<ReviewAnswer>(
            &h.agent,
            "private prompt".into(),
            limits,
            &h.trace,
            None,
            None,
        )
        .await;
        assert_eq!(result.err(), Some(("invalid_final_json", None)));
        assert_eq!(h.model.requests().len(), 2);
        let requests = h.model.requests();
        let correction = serde_json::to_string(&requests[1].chat_history).unwrap();
        assert!(correction.contains("boolean"));
        assert_eq!(
            named(&h.events(), "final_answer_correction_finished")[0]["detail"]["outcome"],
            "invalid_final_json"
        );
        assert_eq!(
            named(&h.events(), "final_answer_correction_finished")[0]["detail"]["validation"],
            "answer_shape"
        );
    }

    #[tokio::test]
    async fn reserved_call_may_declare_insufficient_evidence() {
        let answer =
            json!({"insufficient_evidence":true,"open_questions":["a","b","c","d","e","f","g"]});
        let h = harness(turns_then(MockTurn::text(answer.to_string())));
        let Finish::Insufficient(questions) = h.run(5).await.unwrap() else {
            panic!("expected insufficient evidence");
        };
        assert_eq!(questions.len(), 5, "bounded to five questions");
        let events = h.events();
        assert_eq!(
            named(&events, "final_answer_finished")[0]["detail"]["outcome"],
            "insufficient_evidence"
        );
        assert_eq!(
            named(&events, "agent_finished")[0]["detail"]["cause"],
            "insufficient_evidence"
        );
        assert_eq!(
            insufficient_outcome("investigation"),
            Outcome::ModelExhausted
        );
        assert_eq!(insufficient_outcome("coding"), Outcome::ModelExhausted);
        assert_eq!(insufficient_outcome("review"), Outcome::ReviewIncomplete);
    }

    #[test]
    fn open_questions_are_redacted_and_bounded() {
        let redactor = Redactor::new(["zebra_literal"]).unwrap();
        let long = "x".repeat(500);
        let out = redact_questions(
            &redactor,
            &[
                "Where is the operator parsed?".into(),
                "Does zebra_literal matter?".into(),
                long,
            ],
        );
        assert_eq!(out[0], "Where is the operator parsed?");
        assert!(out[1].starts_with("[withheld sha256:") && out[1].contains("source_literal"));
        assert!(!out[1].contains("zebra_literal"));
        let decoded = decode_answer::<Value>(
            &json!({"insufficient_evidence":true,"open_questions":["y".repeat(500)]}).to_string(),
            4096,
            true,
        );
        let Ok(Finish::Insufficient(q)) = decoded else {
            panic!("insufficient expected");
        };
        assert_eq!(q[0].chars().count(), MAX_QUESTION_CHARS);
    }

    #[tokio::test]
    async fn tool_call_or_bad_json_in_reserved_call_ends_with_named_cause() {
        let h = harness(turns_then(MockTurn::tool_call(
            "c5",
            "outline",
            json!({"path":"crates/baho-core/src/big.rs"}),
        )));
        assert_eq!(h.run(5).await.err(), Some(("final_answer_tool_call", None)));
        let events = h.events();
        assert_eq!(
            named(&events, "agent_finished")[0]["detail"]["cause"],
            "final_answer_tool_call"
        );
        assert_eq!(h.session.calls_charged(), 4, "the refused call ran no tool");

        let h = harness(turns_then(MockTurn::text("private not json")));
        assert_eq!(h.run(5).await.err(), Some(("invalid_final_json", None)));
        let events = h.events();
        assert_eq!(
            named(&events, "final_answer_finished")[0]["detail"]["outcome"],
            "invalid_final_json"
        );
        assert!(
            !fs::read_to_string(&h.trace_path)
                .unwrap()
                .contains("private")
        );
    }

    #[tokio::test]
    async fn typed_tool_errors_reach_the_model_and_the_trace() {
        let h = harness([
            MockTurn::tool_call("e1", "outline", json!({"path":"../outside.rs"})),
            MockTurn::tool_call(
                "e2",
                "read_lines",
                json!({"path":h_file(),"ranges":[{"start":9,"end":2}]}),
            ),
            MockTurn::tool_call(
                "e3",
                "outline",
                json!({"path":"crates/baho-core/src/nope.rs"}),
            ),
            MockTurn::text("{}"),
        ]);
        h.run(6).await.unwrap();
        let history =
            serde_json::to_string(&h.model.requests().last().unwrap().chat_history).unwrap();
        for text in [
            "path_not_allowed",
            "invalid_range",
            "not_found",
            "use search_text",
        ] {
            assert!(history.contains(text), "model sees {text}: {history}");
        }
        assert!(!history.contains("the tool failed"));
        let events = h.events();
        let codes: Vec<&str> = named(&events, "tool_call_finished")
            .iter()
            .map(|e| e["detail"]["code"].as_str().unwrap())
            .collect();
        assert_eq!(codes, ["path_not_allowed", "invalid_range", "not_found"]);
        assert!(
            named(&events, "tool_call_finished")
                .iter()
                .all(|e| e["detail"]["succeeded"] == false)
        );
    }

    fn h_file() -> &'static str {
        "crates/baho-core/src/big.rs"
    }

    #[test]
    fn investigator_prompt_includes_charged_initial_map() {
        let h = harness([]);
        let settings = crate::repair_entry::RepairOptions::default_settings();
        let prompt =
            initial_investigation_prompt(&h.session, &settings, "case evidence".into(), &h.trace);
        assert!(prompt.contains("case evidence"));
        assert!(prompt.contains(h_file()));
        assert!(prompt.contains("item_0"));
        assert!(prompt.contains("item_39"));
        let map = prompt.strip_prefix("case evidence\n\n").unwrap();
        assert!(map.len() <= INITIAL_MAP_MAX_BYTES);
        assert_eq!(h.session.exposed_bytes(), map.len());
        assert_eq!(h.session.calls_charged(), 0);
    }

    #[test]
    fn investigator_receives_exact_issued_build_id_for_executable_tools() {
        let target = Observation {
            build_id: "issued-build-identity-123".into(),
            result_id: "observed-result-456".into(),
            fixture_sha256: "fixture".into(),
            prompt_sha256: "prompt".into(),
            source_rows: None,
            plan: PlanComparison::Unavailable,
        };
        let settings = crate::repair_entry::RepairOptions::default_settings();
        let prompt = investigation_prompt(&target, "private target", &settings);
        assert!(prompt.contains("Baseline build_id: issued-build-identity-123"));
        assert!(prompt.contains("Pass this exact build_id to probe and run_checks"));
        assert!(prompt.contains("do not derive it from the revision, result ID, or binary hash"));
        assert!(prompt.contains("Baseline result ID: observed-result-456"));
        assert!(prompt.contains(PROBE_SQL_TEMPLATE));
        assert!(prompt.contains("double embedded single quotes"));
    }

    #[test]
    fn unknown_build_id_error_is_specific_and_payload_free() {
        assert!(require_issued_build_id(true).is_ok());
        let error = require_issued_build_id(false).unwrap_err().to_string();
        assert!(error.starts_with("invalid_build_id:"));
        assert!(error.contains("exact build_id"));
        assert!(!error.contains("private target"));
        assert_eq!(
            crate::source_tools::ToolErrorCode::find_in(&error),
            Some(crate::source_tools::ToolErrorCode::InvalidBuildId)
        );
    }

    #[test]
    fn navigation_note_states_turn_and_byte_budgets() {
        let settings = crate::repair_entry::RepairOptions::default_settings();
        assert_eq!(settings.max_investigator_turns, 12);
        assert_eq!(settings.max_agent_turns, 24);
        assert_eq!(settings.max_tool_calls, 40);
        assert_eq!(
            settings.investigator_prompt_version,
            "repair-investigator-v6"
        );
        assert_eq!(settings.reviewer_prompt_version, "repair-reviewer-v2");
        assert_eq!(settings.coder_prompt_version, "repair-coder-v4");
        let note = navigation_note(&settings, "investigation", CORE_PATHS);
        for needle in [
            "11 tool-using",
            "12 model calls",
            "40 tool calls",
            "49152 bytes",
            "16384 bytes",
            "insufficient_evidence",
        ] {
            assert!(note.contains(needle), "{needle}: {note}");
        }
        for stage in ["coding", "review"] {
            let note = navigation_note(&settings, stage, CORE_PATHS);
            assert!(note.contains("23 tool-using"));
            assert!(note.contains("24 model calls"));
        }
    }

    #[tokio::test]
    async fn records_tool_budget_without_recording_source_or_arguments() {
        let h = harness([
            MockTurn::tool_call("call-1", "outline", json!({"path":"private source path"})),
            MockTurn::tool_call("call-2", "outline", json!({"path":"private source path"})),
            MockTurn::text("{}"),
        ]);
        let session = SourceSession::new(
            h._dir.path().to_path_buf(),
            &[SCOPE],
            SourceLimits {
                max_calls: 1,
                read_bytes: 1024,
                outline_bytes: 6 * 1024,
                search_matches: 40,
                stage_bytes: 48 * 1024,
            },
        );
        let agent = AgentBuilder::new(h.model.clone())
            .add_hook(h.trace.clone())
            .tool(OutlineTool(session.clone()))
            .build();
        agent.prompt("private prompt").max_turns(4).await.unwrap();
        assert!(h.trace.finish("completed", None, session.calls_charged()));
        assert!(session.budget_denied());
        let text = fs::read_to_string(&h.trace_path).unwrap();
        assert!(!text.contains("private prompt"));
        let events = h.events();
        for name in [
            "model_call_started",
            "model_call_finished",
            "tool_call_started",
            "tool_call_finished",
            "agent_finished",
        ] {
            assert!(events.iter().any(|event| event["name"] == name), "{name}");
        }
        let codes: Vec<&str> = named(&events, "tool_call_finished")
            .iter()
            .map(|e| e["detail"]["code"].as_str().unwrap())
            .collect();
        assert_eq!(codes, ["path_not_allowed", "budget_exhausted"]);
    }
    #[test]
    fn synthetic_probe_exposes_observed_membership_diagnostics_and_typed_plan() {
        let observation = ProbeObservation {
            build_id: "build".into(),
            binary_sha256: "binary".into(),
            fixture_sha256: "fixture".into(),
            prompt_sha256: "prompt".into(),
            result_id: "result".into(),
            run_id: "000001".into(),
            outcome: "Materialized".into(),
            source_rows: vec![SyntheticRow {
                source_row: 1,
                cells: vec!["Analyst".into()],
            }],
            body_rows: vec![BodyObservation {
                source_row: 1,
                kind: "Data".into(),
            }],
            result_rows: Some(vec![1]),
            diagnostic_codes: vec!["exec.rows_filtered".into()],
            diagnostics: vec![DiagnosticSummary {
                code: "exec.rows_filtered".into(),
                severity: "Info".into(),
                message: "one row removed".into(),
            }],
            plan: Some(PlanEvidence {
                column: "Role".into(),
                operator: PlanOperator::Equal,
                literal: PlanLiteral::Text("Analyst".into()),
                text_match_policy: TextMatchPolicy::UnicodeLowercase,
            }),
            oracle: Some(OracleObservation {
                expected_rows: vec![1],
                ordered_rows_match: true,
                plan_comparison: PlanComparison::Equivalent,
            }),
        };
        let output = probe_tool_output(observation).unwrap();
        assert_eq!(output["body_rows"][0]["source_row"], 1);
        assert_eq!(output["result_rows"], serde_json::json!([1]));
        assert_eq!(output["diagnostics"][0]["code"], "exec.rows_filtered");
        assert_eq!(output["plan"]["column"], "Role");
        assert_eq!(output["oracle"]["expected_rows"], serde_json::json!([1]));
        assert_eq!(output["asserted_desired_behavior"], false);
        assert_eq!(output["result_id"], "result");
    }

    #[test]
    fn review_patch_must_match_the_frozen_hash() {
        let patch = vec![PatchFile {
            path: PathBuf::from("crates/baho-core/src/intent.rs"),
            baseline_sha256: Some("old".into()),
            content: "checked content".into(),
        }];
        let serialized = serde_json::to_vec(&patch).unwrap();
        let sha256 = digest(&serialized);
        assert!(verified_patch_hash(&sha256, &patch).is_ok());

        let mut different_patch = patch;
        different_patch[0].content = "different content".into();
        assert!(matches!(
            verified_patch_hash(&sha256, &different_patch),
            Err(Outcome::ReviewIncomplete)
        ));
    }

    #[test]
    fn review_patch_must_equal_the_applied_plan() {
        let planned = Planned {
            files: vec![crate::anchored_edits::FileEdit {
                path: PathBuf::from("crates/baho-core/src/a.rs"),
                original: "a\n".into(),
                updated: "b\n".into(),
                hunks: vec![],
            }],
            new_files: vec![(PathBuf::from("crates/baho-core/tests/t.rs"), "t".into())],
        };
        let file = |path: &str, content: &str| PatchFile {
            path: PathBuf::from(path),
            baseline_sha256: None,
            content: content.into(),
        };
        let good = [
            file("crates/baho-core/src/a.rs", "b\n"),
            file("crates/baho-core/tests/t.rs", "t"),
        ];
        assert!(patch_matches_plan(&good, &planned));
        assert!(!patch_matches_plan(&good[..1], &planned));
        assert!(!patch_matches_plan(
            &[good[0].clone(), file("crates/baho-core/tests/t.rs", "x")],
            &planned
        ));
    }

    fn stage_agents(
        turns: impl IntoIterator<Item = MockTurn>,
    ) -> Option<(tempfile::TempDir, RigRepairAgents, MockCompletionModel)> {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        fs::create_dir_all(checkout.join(CORE_SCOPE)).unwrap();
        fs::write(
            checkout.join("crates/baho-core/src/intent.rs"),
            "pub fn before() {}\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("protected")).unwrap();
        // No build or probe runs here. The worker is needed only because the
        // source-session agent declares those tools alongside navigation.
        let worker = match LiveWorker::new(
            dir.path(),
            dir.path(),
            &"a".repeat(40),
            Duration::from_secs(30),
            Vec::new(),
        ) {
            Ok(worker) => worker,
            Err(crate::repair_build::BuildError::WorkerUnavailable(_)) => return None,
            Err(error) => panic!("worker setup failed: {error}"),
        };
        let model = MockCompletionModel::from_turns(turns);
        let mut agents = RigRepairAgents::new(
            model.clone().into(),
            crate::repair_entry::RepairOptions::default_settings(),
            checkout,
            "show alpha".into(),
            Redactor::new(["zebra_literal"]).unwrap(),
            Arc::new(Mutex::new(worker)),
        );
        // These mock turns exercise staging and final-answer mechanics without
        // a confined source build; staged-check behavior has separate tests.
        agents.require_staged_check = false;
        agents.hypothesis = Some(Investigation {
            result_ids: vec!["r".into()],
            causal_hypothesis: "operator mismatch".into(),
            mechanism: "one branch is wrong".into(),
            allowed_files: vec!["crates/baho-core/src/intent.rs".into()],
            compatibility_checks: vec!["existing tests".into()],
            reduction: None,
        });
        Some((dir, agents, model))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn submitted_investigation_survives_malformed_final_answer() {
        let handoff = json!({
            "result_ids":["result-1"],
            "causal_hypothesis":"operator recognition missed a phrase",
            "mechanism":"recognize the phrase before filtering",
            "allowed_files":["crates/baho-core/src/intent.rs"],
            "compatibility_checks":["existing equality filters"],
            "reduction":{"csv":"Role\nZzaa\n","prompt":"Role is Zzaa","sql":"SELECT _row FROM data WHERE lower(\"Role\") = lower('Zzaa') ORDER BY _row"}
        });
        let Some((_dir, mut agents, model)) = stage_agents([
            MockTurn::tool_call("submit", "submit_investigation", handoff),
            MockTurn::text("malformed final answer"),
            MockTurn::text("still malformed"),
        ]) else {
            return;
        };
        let answer: InvestigationAnswer = agents
            .ask(
                "investigation",
                "investigate",
                "private prompt".into(),
                agents.checkout.clone(),
                &[CORE_SCOPE],
                None,
            )
            .unwrap();
        assert_eq!(answer.result_ids, ["result-1"]);
        assert_eq!(model.requests().len(), 3);
        let trace =
            fs::read_to_string(agents.checkout.parent().unwrap().join("agent-events.jsonl"))
                .unwrap();
        assert!(trace.contains("submitted_investigation_used"));
        assert!(!trace.contains("operator recognition missed a phrase"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn submitted_investigation_does_not_hide_provider_failure() {
        let handoff = json!({"result_ids":["result-1"],"causal_hypothesis":"hypothesis","mechanism":"mechanism","allowed_files":["crates/baho-core/src/intent.rs"],"compatibility_checks":[],"reduction":{"csv":"Role\nZzaa\n","prompt":"Role is Zzaa","sql":"SELECT _row FROM data WHERE lower(\"Role\") = lower('Zzaa') ORDER BY _row"}});
        let Some((_dir, mut agents, _model)) = stage_agents([
            MockTurn::tool_call("submit", "submit_investigation", handoff),
            MockTurn::error("private provider failure"),
        ]) else {
            return;
        };
        let result: Result<InvestigationAnswer, _> = agents.ask(
            "investigation",
            "investigate",
            "private prompt".into(),
            agents.checkout.clone(),
            &[CORE_SCOPE],
            None,
        );
        assert!(result.is_err());
        let trace =
            fs::read_to_string(agents.checkout.parent().unwrap().join("agent-events.jsonl"))
                .unwrap();
        assert!(!trace.contains("submitted_investigation_used"));
        assert!(!trace.contains("private provider failure"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn coder_and_reviewer_use_separate_mock_sessions() {
        let path = "crates/baho-core/src/intent.rs";
        let old = "pub fn before() {}";
        let new = "pub fn after() {}";
        let answer = json!({"edits":[{"path":path,"baseline_sha256":digest(format!("{old}\n")),"start_line":1,"end_line":1,"old":old,"new":new}],"new_files":[]});
        let Some((_dir, mut agents, coder_model)) = stage_agents([
            MockTurn::tool_call(
                "read-1",
                "read_lines",
                json!({"path":path,"ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::text(answer.to_string()),
        ]) else {
            return;
        };
        let before = snapshot(&agents.checkout).unwrap();
        let checkout = agents.checkout.clone();
        assert_eq!(
            agents.code(Path::new("unused-regression"), &checkout),
            Ok(())
        );
        assert_eq!(
            fs::read_to_string(checkout.join(path)).unwrap(),
            format!("{new}\n")
        );
        assert_eq!(coder_model.requests().len(), 2);
        let after = snapshot(&checkout).unwrap();
        let (patch_hash, patch) = check_edits(&before, &after, &checkout).unwrap();
        let reviewer_model = MockCompletionModel::from_turns([
            MockTurn::tool_call(
                "review-read",
                "read_lines",
                json!({"path":format!("candidate/{path}"),"ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::text(r#"{"pass":true,"reason":"the edit matches the evidence"}"#),
        ]);
        agents.model = reviewer_model.clone().into();
        assert_eq!(agents.review(&patch_hash, &patch, &[]), Ok(true));
        assert_eq!(reviewer_model.requests().len(), 2);
        assert!(!checkout.parent().unwrap().join("review-view").exists());
        let events: Vec<Value> =
            fs::read_to_string(checkout.parent().unwrap().join("agent-events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        for stage in ["coding", "review"] {
            assert!(events.iter().any(|event| {
                event["stage"] == stage
                    && event["name"] == "agent_started"
                    && event["detail"]["max_agent_turns"] == 24
            }));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn coder_can_submit_after_five_navigation_turns() {
        let path = "crates/baho-core/src/intent.rs";
        let old = "pub fn before() {}";
        let new = "pub fn after() {}";
        let read = || json!({"path":path,"ranges":[{"start":1,"end":1}]});
        let Some((_dir, mut agents, model)) = stage_agents([
            MockTurn::tool_call("inspect-1", "outline", json!({"path":path})),
            MockTurn::tool_call("inspect-2", "read_lines", read()),
            MockTurn::tool_call("inspect-3", "search_text", json!({"query":"before"})),
            MockTurn::tool_call("inspect-4", "outline", json!({"path":path})),
            MockTurn::tool_call("inspect-5", "read_lines", read()),
            MockTurn::tool_call(
                "edit",
                "stage_edit",
                json!({"path":path,"baseline_sha256":digest(format!("{old}\n")),"start_line":1,"end_line":1,"old":old,"new":new}),
            ),
            MockTurn::tool_call("submit", "submit_staged", json!({})),
            MockTurn::text(r#"{"submit_staged":true}"#),
        ]) else {
            return;
        };
        let checkout = agents.checkout.clone();
        assert_eq!(
            agents.code(Path::new("unused-regression"), &checkout),
            Ok(())
        );
        assert_eq!(
            fs::read_to_string(checkout.join(path)).unwrap(),
            format!("{new}\n")
        );
        assert_eq!(model.requests().len(), 8);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn coder_can_submit_after_the_previous_turn_limit() {
        let path = "crates/baho-core/src/intent.rs";
        let old = "pub fn before() {}";
        let new = "pub fn after() {}";
        let mut turns: Vec<MockTurn> = (0..12)
            .map(|index| {
                MockTurn::tool_call(format!("inspect-{index}"), "outline", json!({"path":path}))
            })
            .collect();
        turns.extend([
            MockTurn::tool_call(
                "read",
                "read_lines",
                json!({"path":path,"ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::tool_call(
                "edit",
                "stage_edit",
                json!({"path":path,"baseline_sha256":digest(format!("{old}\n")),"start_line":1,"end_line":1,"old":old,"new":new}),
            ),
            MockTurn::tool_call("submit", "submit_staged", json!({})),
            MockTurn::text(r#"{"submit_staged":true}"#),
        ]);
        let Some((_dir, mut agents, model)) = stage_agents(turns) else {
            return;
        };
        let checkout = agents.checkout.clone();
        assert_eq!(
            agents.code(Path::new("unused-regression"), &checkout),
            Ok(())
        );
        assert_eq!(
            fs::read_to_string(checkout.join(path)).unwrap(),
            format!("{new}\n")
        );
        assert_eq!(model.requests().len(), 16);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reserved_final_answer_can_submit_a_staged_patch() {
        let path = "crates/baho-core/src/intent.rs";
        let old = "pub fn before() {}";
        let new = "pub fn after() {}";
        let Some((_dir, mut agents, model)) = stage_agents([
            MockTurn::tool_call(
                "existing-test",
                "read_lines",
                json!({"path":"crates/baho-core/tests/existing.rs","ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::tool_call(
                "read",
                "read_lines",
                json!({"path":path,"ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::tool_call(
                "edit",
                "stage_edit",
                json!({"path":path,"baseline_sha256":digest(format!("{old}\n")),"start_line":1,"end_line":1,"old":old,"new":new}),
            ),
            MockTurn::text(r#"{"submit_staged":true}"#),
        ]) else {
            return;
        };
        agents.settings.max_agent_turns = 4;
        let checkout = agents.checkout.clone();
        fs::create_dir_all(checkout.join(CORE_TESTS_SCOPE)).unwrap();
        fs::write(
            checkout.join("crates/baho-core/tests/existing.rs"),
            "#[test] fn existing() {}\n",
        )
        .unwrap();
        assert_eq!(
            agents.code(Path::new("unused-regression"), &checkout),
            Ok(())
        );
        assert_eq!(
            fs::read_to_string(checkout.join(path)).unwrap(),
            format!("{new}\n")
        );
        assert_eq!(model.requests().len(), 4);
        let events: Vec<Value> =
            fs::read_to_string(checkout.parent().unwrap().join("agent-events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert!(events.iter().any(|event| {
            event["name"] == "tool_call_finished"
                && event["detail"]["call_id"] == "existing-test"
                && event["detail"]["succeeded"] == true
        }));
        assert!(events.iter().any(|event| {
            event["name"] == "tool_call_finished"
                && event["detail"]["tool"] == "stage_edit"
                && event["detail"]["staged_edits"] == 1
        }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn final_submission_without_a_staged_patch_is_rejected() {
        let Some((_dir, mut agents, _model)) =
            stage_agents([MockTurn::text(r#"{"submit_staged":true}"#)])
        else {
            return;
        };
        let checkout = agents.checkout.clone();
        assert_eq!(
            agents.code(Path::new("unused-regression"), &checkout),
            Err(Outcome::Rejected)
        );
        assert!(
            agents
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("edit_set_invalid"))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn staged_submission_survives_truncated_final_turn() {
        let path = "crates/baho-core/src/intent.rs";
        let old = "pub fn before() {}";
        let new = "pub fn after() {}";
        let test_path = "crates/baho-core/tests/repair_extra.rs";
        let Some((_dir, mut agents, model)) = stage_agents([
            MockTurn::tool_call(
                "read",
                "read_lines",
                json!({"path":path,"ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::tool_call(
                "edit",
                "stage_edit",
                json!({"path":path,"baseline_sha256":digest(format!("{old}\n")),"start_line":1,"end_line":1,"old":old,"new":new}),
            ),
            MockTurn::tool_call(
                "test",
                "stage_new_file",
                json!({"path":test_path,"content":"#[test] fn repair_regression() {}\n"}),
            ),
            MockTurn::tool_call("submit", "submit_staged", json!({})),
            MockTurn::text("").with_finish_reason(rig::completion::FinishReason::Length),
        ]) else {
            return;
        };
        let checkout = agents.checkout.clone();
        assert_eq!(
            agents.code(Path::new("unused-regression"), &checkout),
            Ok(())
        );
        assert_eq!(
            fs::read_to_string(checkout.join(path)).unwrap(),
            format!("{new}\n")
        );
        assert!(checkout.join(test_path).is_file());
        assert_eq!(model.requests().len(), 5);
        let events: Vec<Value> =
            fs::read_to_string(checkout.parent().unwrap().join("agent-events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(
            named(&events, "submitted_patch_survived_final_answer_failure").len(),
            1
        );
        assert_eq!(
            named(&events, "submitted_patch_survived_final_answer_failure")[0]["detail"]["cause"],
            "output_token_limit"
        );
        assert_eq!(
            named(&events, "agent_finished")[0]["detail"]["cause"],
            "completed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn submitted_patch_survives_insufficient_final_answer() {
        let path = "crates/baho-core/src/intent.rs";
        let old = "pub fn before() {}";
        let new = "pub fn after() {}";
        let Some((_dir, mut agents, _model)) = stage_agents([
            MockTurn::tool_call(
                "read",
                "read_lines",
                json!({"path":path,"ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::tool_call(
                "edit",
                "stage_edit",
                json!({"path":path,"baseline_sha256":digest(format!("{old}\n")),"start_line":1,"end_line":1,"old":old,"new":new}),
            ),
            MockTurn::tool_call("submit", "submit_staged", json!({})),
            MockTurn::text(
                r#"{"insufficient_evidence":true,"open_questions":["patch was not submitted"]}"#,
            ),
        ]) else {
            return;
        };
        let checkout = agents.checkout.clone();
        assert_eq!(
            agents.code(Path::new("unused-regression"), &checkout),
            Ok(())
        );
        assert_eq!(
            fs::read_to_string(checkout.join(path)).unwrap(),
            format!("{new}\n")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn truncated_unsubmitted_draft_leaves_checkout_unchanged() {
        let path = "crates/baho-core/src/intent.rs";
        let old = "pub fn before() {}";
        let Some((_dir, mut agents, _model)) = stage_agents([
            MockTurn::tool_call(
                "read",
                "read_lines",
                json!({"path":path,"ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::tool_call(
                "edit",
                "stage_edit",
                json!({"path":path,"baseline_sha256":digest(format!("{old}\n")),"start_line":1,"end_line":1,"old":old,"new":"pub fn after() {}"}),
            ),
            MockTurn::text("").with_finish_reason(rig::completion::FinishReason::Length),
        ]) else {
            return;
        };
        let checkout = agents.checkout.clone();
        assert_eq!(
            agents.code(Path::new("unused-regression"), &checkout),
            Err(Outcome::ModelExhausted)
        );
        assert_eq!(
            fs::read_to_string(checkout.join(path)).unwrap(),
            format!("{old}\n")
        );
    }

    #[tokio::test]
    async fn stage_tools_reject_bad_anchors_scope_and_duplicate_submission() {
        let dir = tempfile::tempdir().unwrap();
        let path = "crates/baho-core/src/intent.rs";
        fs::create_dir_all(dir.path().join(CORE_SCOPE)).unwrap();
        fs::write(dir.path().join(path), "a\nb\n").unwrap();
        let nav = SourceSession::new(
            dir.path().to_path_buf(),
            &[CORE_SCOPE],
            SourceLimits {
                max_calls: 11,
                read_bytes: 1024,
                outline_bytes: 1024,
                search_matches: 10,
                stage_bytes: 4096,
            },
        );
        let draft = Arc::new(Mutex::new(DraftState::default()));
        let tools = DraftTools {
            tools: SourceTools { nav: nav.clone() },
            checkout: dir.path().to_path_buf(),
            draft: draft.clone(),
            max_bytes: 4096,
        };
        let edit = |path: &str, old: &str| Edit {
            path: path.into(),
            baseline_sha256: digest("a\nb\n"),
            start_line: 1,
            end_line: 1,
            old: old.into(),
            new: "A".into(),
        };
        assert!(
            StageEditTool(tools.clone())
                .call(edit(path, "a"))
                .await
                .is_err(),
            "unread edit"
        );
        nav.read_lines(path, &[crate::source_tools::LineRange { start: 1, end: 1 }])
            .unwrap();
        let mut malicious_range = edit(path, "a");
        malicious_range.end_line = usize::MAX;
        assert!(
            StageEditTool(tools.clone())
                .call(malicious_range)
                .await
                .is_err(),
            "unbounded range must be rejected promptly"
        );
        assert!(
            StageEditTool(tools.clone())
                .call(edit(path, "wrong"))
                .await
                .is_err()
        );
        assert!(
            StageEditTool(tools.clone())
                .call(edit("crates/baho-model/src/lib.rs", "a"))
                .await
                .is_err()
        );
        assert_eq!(draft.lock().unwrap().answer.edits.len(), 0);
        assert!(
            StageEditTool(tools.clone())
                .call(edit(path, "a"))
                .await
                .is_ok()
        );
        assert!(
            StageEditTool(tools.clone())
                .call(edit(path, "a"))
                .await
                .is_ok()
        );
        assert_eq!(draft.lock().unwrap().answer.edits.len(), 1);
        assert!(
            StageNewFileTool(tools.clone())
                .call(NewFile {
                    path: "crates/baho-core/tests/big.rs".into(),
                    content: "x".repeat(MAX_NEW_FILE_BYTES + 1)
                })
                .await
                .is_err()
        );
        assert_eq!(
            SubmitStagedTool(tools.clone())
                .call(json!({}))
                .await
                .unwrap()["submitted"],
            true
        );
        assert!(
            SubmitStagedTool(tools.clone())
                .call(json!({}))
                .await
                .is_err()
        );
        assert!(StageEditTool(tools).call(edit(path, "a")).await.is_err());
        assert_eq!(nav.calls_charged(), 11);
        assert_eq!(fs::read_to_string(dir.path().join(path)).unwrap(), "a\nb\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reviewer_reserved_insufficient_answer_maps_to_review_incomplete() {
        let path = "crates/baho-core/src/intent.rs";
        let old = "pub fn before() {}";
        let answer = json!({"edits":[{"path":path,"baseline_sha256":digest(format!("{old}\n")),"start_line":1,"end_line":1,"old":old,"new":"pub fn after() {}"}],"new_files":[]});
        let Some((_dir, mut agents, _)) = stage_agents([MockTurn::text(answer.to_string())]) else {
            return;
        };
        let before = snapshot(&agents.checkout).unwrap();
        let checkout = agents.checkout.clone();
        agents
            .code(Path::new("unused-regression"), &checkout)
            .unwrap();
        let after = snapshot(&checkout).unwrap();
        let (patch_hash, patch) = check_edits(&before, &after, &checkout).unwrap();
        agents.settings.max_agent_turns = 2;
        let reviewer_model = MockCompletionModel::from_turns([
            MockTurn::tool_call(
                "review-read",
                "read_lines",
                json!({"path":format!("candidate/{path}"),"ranges":[{"start":1,"end":1}]}),
            ),
            MockTurn::text(
                r#"{"insufficient_evidence":true,"open_questions":["Need one more counterexample"]}"#,
            ),
        ]);
        agents.model = reviewer_model.clone().into();
        assert_eq!(
            agents.review(&patch_hash, &patch, &[]),
            Err(Outcome::ReviewIncomplete)
        );
        assert_eq!(
            agents.failure_reason(),
            Some("review: insufficient_evidence")
        );
        assert_eq!(agents.open_questions(), &["Need one more counterexample"]);
        assert_eq!(reviewer_model.requests().len(), 2);
        assert_eq!(
            reviewer_model.requests()[1].tool_choice,
            Some(ToolChoice::None)
        );
    }
}
