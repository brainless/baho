//! Three independent Rig sessions for one bounded repair attempt.
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;

use rig::tool::PortableTool;
use rig::{AgentBuilder, DynModel, operation::Completion};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::redaction::Redactor;
use crate::repair::SuiteId;
use crate::repair_live::LiveWorker;
use crate::repair_trace::AgentTrace;

#[derive(Debug, thiserror::Error)]
#[error("repair source tool denied request")]
struct SourceToolError;

#[derive(Clone)]
struct SourceTools {
    root: PathBuf,
    calls: Arc<AtomicU32>,
    max_calls: u32,
    trace: AgentTrace,
}

impl SourceTools {
    fn charge(&self) -> Result<(), SourceToolError> {
        let before = self.calls.fetch_add(1, Ordering::Relaxed);
        if before >= self.max_calls {
            self.trace.event(
                "tool_budget_exhausted",
                json!({"limit":self.max_calls,"attempted_call":before + 1}),
            );
        }
        (before < self.max_calls)
            .then_some(())
            .ok_or(SourceToolError)
    }
    fn resolve(&self, raw: &str) -> Result<PathBuf, SourceToolError> {
        if raw.len() > 256 || !allowed(Path::new(raw), true) {
            return Err(SourceToolError);
        }
        let path = self.root.join(raw);
        let kind = fs::symlink_metadata(&path)
            .map_err(|_| SourceToolError)?
            .file_type();
        if !kind.is_file() {
            return Err(SourceToolError);
        }
        let canon = fs::canonicalize(&path).map_err(|_| SourceToolError)?;
        let root = fs::canonicalize(&self.root).map_err(|_| SourceToolError)?;
        if !canon.starts_with(root)
            || fs::metadata(&canon).map_err(|_| SourceToolError)?.len() > 64 * 1024
        {
            return Err(SourceToolError);
        }
        Ok(canon)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
}

#[derive(Clone)]
struct ReadSource(SourceTools);
impl PortableTool for ReadSource {
    const NAME: &'static str = "read_file";
    type Args = ReadArgs;
    type Output = Value;
    type Error = SourceToolError;
    fn description(&self) -> String {
        "Read one bounded baho-core source file in the isolated checkout.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false})
    }
    async fn call(&self, args: ReadArgs) -> Result<Value, SourceToolError> {
        self.0.charge()?;
        let path = self.0.resolve(&args.path)?;
        let source = fs::read_to_string(path).map_err(|_| SourceToolError)?;
        Ok(json!({"path":args.path,"content":source}))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
}

#[derive(Clone)]
struct SearchSource(SourceTools);
impl PortableTool for SearchSource {
    const NAME: &'static str = "search_text";
    type Args = SearchArgs;
    type Output = Value;
    type Error = SourceToolError;
    fn description(&self) -> String {
        "Search bounded baho-core source text in the isolated checkout.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string","maxLength":100}},"required":["query"],"additionalProperties":false})
    }
    async fn call(&self, args: SearchArgs) -> Result<Value, SourceToolError> {
        self.0.charge()?;
        if args.query.is_empty() || args.query.len() > 100 {
            return Err(SourceToolError);
        }
        let mut matches = Vec::new();
        let dir = self.0.root.join("crates/baho-core/src");
        for entry in fs::read_dir(dir).map_err(|_| SourceToolError)?.take(100) {
            let entry = entry.map_err(|_| SourceToolError)?;
            if !entry.file_type().map_err(|_| SourceToolError)?.is_file() {
                continue;
            }
            let name = format!(
                "crates/baho-core/src/{}",
                entry.file_name().to_string_lossy()
            );
            let path = self.0.resolve(&name)?;
            let source = fs::read_to_string(path).map_err(|_| SourceToolError)?;
            for (index, line) in source.lines().enumerate() {
                if line.contains(&args.query) {
                    matches.push(json!({"path":name,"line":index+1,"excerpt":line.chars().take(200).collect::<String>()}));
                }
                if matches.len() >= 40 {
                    break;
                }
            }
            if matches.len() >= 40 {
                break;
            }
        }
        Ok(json!({"matches":matches}))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeArgs {
    build_id: String,
    csv: String,
    prompt: String,
    sql: Option<String>,
}

#[derive(Clone)]
struct ProbeTool {
    tools: SourceTools,
    worker: Arc<Mutex<LiveWorker>>,
    redactor: Redactor,
}

fn probe_tool_output(
    observation: crate::probe::ProbeObservation,
) -> Result<Value, SourceToolError> {
    let mut output = serde_json::to_value(observation).map_err(|_| SourceToolError)?;
    output["asserted_desired_behavior"] = Value::Bool(false);
    Ok(output)
}

impl PortableTool for ProbeTool {
    const NAME: &'static str = "probe";
    type Args = ProbeArgs;
    type Output = Value;
    type Error = SourceToolError;
    fn description(&self) -> String {
        "Run the confined public CLI on bounded neutral synthetic CSV; optional SQL uses restricted Role equality.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"build_id":{"type":"string"},"csv":{"type":"string"},"prompt":{"type":"string"},"sql":{"type":["string","null"]}},"required":["build_id","csv","prompt"],"additionalProperties":false})
    }
    async fn call(&self, args: ProbeArgs) -> Result<Value, SourceToolError> {
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
            return Err(SourceToolError);
        }
        let observation = self
            .worker
            .lock()
            .map_err(|_| SourceToolError)?
            .synthetic_probe(&args.build_id, &args.csv, &args.prompt, args.sql.as_deref())
            .map_err(|_| SourceToolError)?;
        probe_tool_output(observation)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckArgs {
    build_id: String,
    suite: SuiteId,
}

#[derive(Clone)]
struct RunChecksTool {
    tools: SourceTools,
    worker: Arc<Mutex<LiveWorker>>,
}
impl PortableTool for RunChecksTool {
    const NAME: &'static str = "run_checks";
    type Args = CheckArgs;
    type Output = Value;
    type Error = SourceToolError;
    fn description(&self) -> String {
        "Run one predefined fixed check suite for an issued build ID in confinement.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"build_id":{"type":"string"},"suite":{"type":"string","enum":["protected_compatibility","additive_tests","owning_crate","workspace_format","workspace_check","workspace_tests"]}},"required":["build_id","suite"],"additionalProperties":false})
    }
    async fn call(&self, args: CheckArgs) -> Result<Value, SourceToolError> {
        self.tools.charge()?;
        let result = self
            .worker
            .lock()
            .map_err(|_| SourceToolError)?
            .fixed_check(&args.build_id, args.suite)
            .map_err(|_| SourceToolError)?;
        serde_json::to_value(result).map_err(|_| SourceToolError)
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

#[derive(Deserialize)]
struct CodeAnswer {
    files: Vec<PatchFile>,
}

#[derive(Deserialize)]
struct ReviewAnswer {
    pass: bool,
    reason: String,
}

pub struct RigRepairAgents {
    model: DynModel<Completion>,
    settings: AttemptSettings,
    checkout: PathBuf,
    target_prompt: String,
    redactor: Redactor,
    worker: Arc<Mutex<LiveWorker>>,
    reduced_csv: String,
    reduced_prompt: String,
    reduced_sql: String,
    hypothesis: Option<Investigation>,
    failure_reason: Option<String>,
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
            worker,
            reduced_csv: String::new(),
            reduced_prompt: String::new(),
            reduced_sql: String::new(),
            hypothesis: None,
            failure_reason: None,
        }
    }

    fn ask<T: for<'de> Deserialize<'de>>(
        &mut self,
        stage: &'static str,
        role: &str,
        prompt: String,
    ) -> Result<T, Outcome> {
        self.failure_reason = None;
        let trace_path = self
            .checkout
            .parent()
            .ok_or(Outcome::EnvironmentFailure)?
            .join("agent-events.jsonl");
        let trace = AgentTrace::open(&trace_path, stage).map_err(|_| {
            self.failure_reason = Some(format!("{stage}: logging_io_error"));
            Outcome::EnvironmentFailure
        })?;
        trace.event("agent_started", json!({"max_agent_turns":self.settings.max_agent_turns,"max_tool_calls":self.settings.max_tool_calls,"session_timeout_seconds":self.settings.process_timeout_seconds,"max_output_bytes":self.settings.max_output_bytes}));
        let model = self.model.clone();
        let tools = SourceTools {
            root: self.checkout.clone(),
            calls: Arc::new(AtomicU32::new(0)),
            max_calls: self.settings.max_tool_calls,
            trace: trace.clone(),
        };
        let probe = ProbeTool {
            tools: tools.clone(),
            worker: self.worker.clone(),
            redactor: self.redactor.clone(),
        };
        let checks = RunChecksTool {
            tools: tools.clone(),
            worker: self.worker.clone(),
        };
        let max_turns = self.settings.max_agent_turns as usize;
        let timeout = Duration::from_secs(self.settings.process_timeout_seconds);
        let calls = tools.calls.clone();
        let hook = trace.clone();
        let max_bytes = self.settings.max_output_bytes;
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let agent = AgentBuilder::new(model)
                    .preamble(role)
                    .add_hook(hook)
                    .temperature(0.2)
                    .max_tokens(8192)
                    .tool(ReadSource(tools.clone()))
                    .tool(SearchSource(tools))
                    .tool(probe)
                    .tool(checks)
                    .build();
                timed_answer(
                    timeout,
                    async {
                        agent
                            .prompt(prompt)
                            .max_turns(max_turns)
                            .await
                            .map(|answer| answer.output().to_owned())
                    },
                    max_bytes,
                )
                .await
            })
        });
        let (cause, limit) = result
            .as_ref()
            .err()
            .copied()
            .unwrap_or(("completed", None));
        let logged = trace.finish(cause, limit, calls.load(Ordering::Relaxed));
        if !logged {
            self.failure_reason = Some(format!("{stage}: logging_io_error; agent cause={cause}"));
            return Err(Outcome::EnvironmentFailure);
        }
        result.map_err(|(cause, limit)| {
            self.failure_reason = Some(format!(
                "{stage}: {cause}{}; tool_budget_exhausted={}",
                limit.map(|n| format!(" (limit={n})")).unwrap_or_default(),
                calls.load(Ordering::Relaxed) > self.settings.max_tool_calls
            ));
            Outcome::ModelExhausted
        })
    }
}

async fn timed_answer<T: for<'de> Deserialize<'de>>(
    timeout: Duration,
    future: impl std::future::Future<Output = Result<String, rig::prelude::PromptError>>,
    max_bytes: usize,
) -> Result<T, (&'static str, Option<u64>)> {
    match tokio::time::timeout(timeout, future).await {
        Err(_) => Err(("session_timeout", Some(timeout.as_secs()))),
        Ok(Err(error)) => Err((
            prompt_error_cause(&error),
            match error {
                rig::prelude::PromptError::MaxTurnsError { max_turns, .. } => {
                    Some(max_turns as u64)
                }
                _ => None,
            },
        )),
        Ok(Ok(output)) => decode_answer(&output, max_bytes),
    }
}

fn prompt_error_cause(error: &rig::prelude::PromptError) -> &'static str {
    use rig::prelude::PromptError;
    match error {
        PromptError::MaxTurnsError { .. } => "max_agent_turns",
        PromptError::CompletionError(_) => "provider_error",
        PromptError::Report(report) => report.kind.code(),
        PromptError::MemoryError(_) => "memory_error",
        PromptError::PromptCancelled { .. } => "agent_cancelled",
        PromptError::UnknownToolCall { .. } => "unknown_tool_call",
    }
}
fn decode_answer<T: for<'de> Deserialize<'de>>(
    output: &str,
    max_bytes: usize,
) -> Result<T, (&'static str, Option<u64>)> {
    if output.len() > max_bytes {
        return Err(("max_output_bytes", Some(max_bytes as u64)));
    }
    serde_json::from_str(output).map_err(|_| ("invalid_final_json", None))
}

fn allowed(path: &Path, present: bool) -> bool {
    let parts: Vec<_> = path.components().collect();
    if parts.iter().any(|c| !matches!(c, Component::Normal(_))) {
        return false;
    }
    let text = path.to_string_lossy();
    text.starts_with("crates/baho-core/src/")
        || (!present && text.starts_with("crates/baho-core/tests/") && text.ends_with(".rs"))
}

fn verified_patch_json(patch_sha256: &str, patch: &[PatchFile]) -> Result<String, Outcome> {
    let bytes = serde_json::to_vec(patch).map_err(|_| Outcome::ReviewIncomplete)?;
    if digest(&bytes) != patch_sha256 {
        return Err(Outcome::ReviewIncomplete);
    }
    String::from_utf8(bytes).map_err(|_| Outcome::ReviewIncomplete)
}

impl Agents for RigRepairAgents {
    fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }
    fn investigate(&mut self, target: &Observation) -> Result<Investigation, Outcome> {
        let prompt = format!(
            "Investigate this reproduced Role text-equality mismatch. Return only JSON with result_ids, causal_hypothesis, mechanism, allowed_files, compatibility_checks, reduction. Reduction has csv, prompt, sql fields only; use a small neutral synthetic CSV and restricted Role equality SQL. The harness computes expected rows. The result_ids must include the observed result. Target prompt: {:?}. Baseline result ID: {}. Baseline plan: {:?}. Allowed production scope: crates/baho-core/src/.",
            self.target_prompt, target.result_id, target.plan,
        );
        let answer: InvestigationAnswer = self.ask("investigation", "You are the independent repair investigator. State one testable mechanism. Do not request scope expansion. Return JSON only.", prompt)?;
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
        let mut sources = Vec::new();
        for name in &handoff.allowed_files {
            let path = checkout.join(name);
            let metadata = fs::symlink_metadata(&path).map_err(|_| Outcome::NeedsDesign)?;
            if !metadata.file_type().is_file() || metadata.len() > 64 * 1024 {
                return Err(Outcome::NeedsDesign);
            }
            sources.push((
                name.clone(),
                fs::read_to_string(path).map_err(|_| Outcome::NeedsDesign)?,
            ));
        }
        let prompt = format!(
            "Fix the frozen Role text-equality regression. Return only JSON {{\"files\":[{{\"path\":\"crates/baho-core/src/...\",\"baseline_sha256\":\"...\",\"content\":\"complete new file\"}}]}}. One final patch ends this stage. Hypothesis: {}. Mechanism: {}. Compatibility: {:?}. Reduced CSV: {:?}. Prompt: {:?}. SQL: {:?}. Source files: {:?}.",
            handoff.causal_hypothesis,
            handoff.mechanism,
            handoff.compatibility_checks,
            self.reduced_csv,
            self.reduced_prompt,
            self.reduced_sql,
            sources
        );
        let answer: CodeAnswer = self.ask("coding", "You are the repair coder. Edit only the allowed core source files and add tests under crates/baho-core/tests/. Do not alter expectations or protected files. Return JSON only.", prompt)?;
        if answer.files.is_empty() || answer.files.len() > 8 {
            return Err(Outcome::Rejected);
        }
        let before = snapshot(checkout).map_err(|_| Outcome::EnvironmentFailure)?;
        for file in &answer.files {
            let present = before.contains_key(&file.path);
            if !allowed(&file.path, present)
                || file.content.len() > 64 * 1024
                || (present && file.baseline_sha256.as_ref() != before.get(&file.path))
            {
                return Err(Outcome::Rejected);
            }
            let dest = checkout.join(&file.path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|_| Outcome::EnvironmentFailure)?;
            }
            fs::write(&dest, &file.content).map_err(|_| Outcome::EnvironmentFailure)?;
        }
        let after = snapshot(checkout).map_err(|_| Outcome::Rejected)?;
        check_edits(&before, &after, checkout).map_err(|_| Outcome::Rejected)?;
        Ok(())
    }

    fn review(
        &mut self,
        patch_sha256: &str,
        patch: &[PatchFile],
        evidence: &[Observation],
    ) -> Result<bool, Outcome> {
        let patch = verified_patch_json(patch_sha256, patch)?;
        let prompt = format!(
            "Review this frozen repair. Return only JSON {{\"pass\":bool,\"reason\":string}}. Patch hash: {}. Patch: {}. Before/after observations: {:?}. Reduced CSV: {:?}; prompt: {:?}; SQL: {:?}. Check NL/SQL meaning, overfitting, preserved tests, and counterexamples. Reject uncertain semantics.",
            patch_sha256, patch, evidence, self.reduced_csv, self.reduced_prompt, self.reduced_sql
        );
        let answer: ReviewAnswer = self.ask("review", "You are an independent reviewer with a fresh Rig history. You cannot edit source or expectations. Return JSON only.", prompt)?;
        if answer.reason.trim().is_empty() {
            return Err(Outcome::ReviewIncomplete);
        }
        Ok(answer.pass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_run::DiagnosticSummary;
    use crate::contracts::{
        PlanComparison, PlanEvidence, PlanLiteral, PlanOperator, TextMatchPolicy,
    };
    use crate::probe::{BodyObservation, OracleObservation, ProbeObservation, SyntheticRow};

    #[tokio::test]
    async fn distinguishes_timeout_turn_limit_provider_and_output_failures() {
        use rig::test_utils::{MockCompletionModel, MockTurn};
        let timeout =
            timed_answer::<Value>(Duration::from_millis(1), std::future::pending(), 100).await;
        assert_eq!(timeout.unwrap_err().0, "session_timeout");
        let agent = AgentBuilder::new(MockCompletionModel::text("{}")).build();
        let turns = timed_answer::<Value>(
            Duration::from_secs(1),
            async {
                agent
                    .prompt("private prompt")
                    .max_turns(0)
                    .await
                    .map(|a| a.output().to_owned())
            },
            100,
        )
        .await;
        assert_eq!(turns.unwrap_err(), ("max_agent_turns", Some(0)));
        let agent = AgentBuilder::new(MockCompletionModel::from_turns([MockTurn::error(
            "secret provider body",
        )]))
        .build();
        let provider = timed_answer::<Value>(
            Duration::from_secs(1),
            async {
                agent
                    .prompt("private prompt")
                    .await
                    .map(|a| a.output().to_owned())
            },
            100,
        )
        .await;
        assert!(matches!(
            provider.unwrap_err().0,
            "provider" | "provider_error"
        ));
        assert_eq!(
            decode_answer::<Value>("private malformed output", 100)
                .unwrap_err()
                .0,
            "invalid_final_json"
        );
        assert_eq!(
            decode_answer::<Value>("12345", 4).unwrap_err(),
            ("max_output_bytes", Some(4))
        );
    }

    #[tokio::test]
    async fn records_tool_budget_without_recording_source_or_arguments() {
        use rig::test_utils::{MockCompletionModel, MockTurn};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-events.jsonl");
        let trace = AgentTrace::open(&path, "investigation").unwrap();
        let tools = SourceTools {
            root: dir.path().into(),
            calls: Arc::new(AtomicU32::new(0)),
            max_calls: 0,
            trace: trace.clone(),
        };
        let agent = AgentBuilder::new(MockCompletionModel::from_turns([
            MockTurn::tool_call(
                "private-call-id",
                "read_file",
                json!({"path":"private source path"}),
            ),
            MockTurn::text("{}"),
        ]))
        .add_hook(trace.clone())
        .tool(ReadSource(tools.clone()))
        .build();
        agent.prompt("private prompt").max_turns(2).await.unwrap();
        assert!(trace.finish("completed", None, tools.calls.load(Ordering::Relaxed)));
        let text = fs::read_to_string(path).unwrap();
        assert!(!text.contains("private"));
        let events: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for name in [
            "model_call_started",
            "model_call_finished",
            "tool_call_started",
            "tool_budget_exhausted",
            "tool_call_finished",
            "agent_finished",
        ] {
            assert!(events.iter().any(|event| event["name"] == name), "{name}");
        }
        let finished = events
            .iter()
            .find(|event| event["name"] == "tool_call_finished")
            .unwrap();
        assert_eq!(finished["detail"]["succeeded"], false);
        assert!(finished["detail"]["elapsed_ms"].is_number());
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
        assert_eq!(
            verified_patch_json(&sha256, &patch).unwrap().as_bytes(),
            serialized
        );

        let mut different_patch = patch;
        different_patch[0].content = "different content".into();
        assert!(matches!(
            verified_patch_json(&sha256, &different_patch),
            Err(Outcome::ReviewIncomplete)
        ));
    }
}
