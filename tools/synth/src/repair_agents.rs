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

#[derive(Debug, thiserror::Error)]
#[error("repair source tool denied request")]
struct SourceToolError;

#[derive(Clone)]
struct SourceTools {
    root: PathBuf,
    calls: Arc<AtomicU32>,
    max_calls: u32,
}

impl SourceTools {
    fn charge(&self) -> Result<(), SourceToolError> {
        let before = self.calls.fetch_add(1, Ordering::Relaxed);
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
        let source_rows =
            crate::probe::validate_synthetic(args.csv.as_bytes()).map_err(|_| SourceToolError)?;
        let observation = self
            .worker
            .lock()
            .map_err(|_| SourceToolError)?
            .synthetic_probe(&args.build_id, &args.csv, &args.prompt, args.sql.as_deref())
            .map_err(|_| SourceToolError)?;
        Ok(
            json!({"build_id":observation.build_id,"fixture_sha256":observation.fixture_sha256,"prompt_sha256":observation.prompt_sha256,"result_id":observation.result_id,"source_rows":source_rows,"actual_result_rows":observation.source_rows,"plan_comparison":observation.plan,"asserted_desired_behavior":false}),
        )
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
    patch: Vec<PatchFile>,
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
            patch: Vec::new(),
        }
    }

    fn ask<T: for<'de> Deserialize<'de>>(&self, role: &str, prompt: String) -> Result<T, Outcome> {
        let model = self.model.clone();
        let tools = SourceTools {
            root: self.checkout.clone(),
            calls: Arc::new(AtomicU32::new(0)),
            max_calls: self.settings.max_tool_calls,
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
        let answer = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let agent = AgentBuilder::new(model)
                    .preamble(role)
                    .temperature(0.2)
                    .max_tokens(8192)
                    .tool(ReadSource(tools.clone()))
                    .tool(SearchSource(tools))
                    .tool(probe)
                    .tool(checks)
                    .build();
                tokio::time::timeout(timeout, agent.prompt(prompt).max_turns(max_turns)).await
            })
        })
        .map_err(|_| Outcome::ModelExhausted)?
        .map_err(|_| Outcome::ModelExhausted)?;
        let output = answer.output();
        if output.len() > self.settings.max_output_bytes {
            return Err(Outcome::ModelExhausted);
        }
        // Reject prose wrappers and markdown fences: the stage protocol is JSON.
        serde_json::from_str(output).map_err(|_| Outcome::ModelExhausted)
    }
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

impl Agents for RigRepairAgents {
    fn investigate(&mut self, target: &Observation) -> Result<Investigation, Outcome> {
        let prompt = format!(
            "Investigate this reproduced Role text-equality mismatch. Return only JSON with result_ids, causal_hypothesis, mechanism, allowed_files, compatibility_checks, reduction. Reduction has csv, prompt, sql fields only; use a small neutral synthetic CSV and restricted Role equality SQL. The harness computes expected rows. The result_ids must include the observed result. Target prompt: {:?}. Baseline result ID: {}. Baseline plan: {:?}. Allowed production scope: crates/baho-core/src/.",
            self.target_prompt, target.result_id, target.plan,
        );
        let answer: InvestigationAnswer = self.ask("You are the independent repair investigator. State one testable mechanism. Do not request scope expansion. Return JSON only.", prompt)?;
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
        let answer: CodeAnswer = self.ask("You are the repair coder. Edit only the allowed core source files and add tests under crates/baho-core/tests/. Do not alter expectations or protected files. Return JSON only.", prompt)?;
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
        self.patch = answer.files;
        Ok(())
    }

    fn review(&mut self, patch_sha256: &str, evidence: &[Observation]) -> Result<bool, Outcome> {
        let patch = serde_json::to_string(&self.patch).map_err(|_| Outcome::ReviewIncomplete)?;
        if digest(&patch) == patch_sha256 { /* hash is independently checked by the harness */ }
        let prompt = format!(
            "Review this frozen repair. Return only JSON {{\"pass\":bool,\"reason\":string}}. Patch hash: {}. Patch: {}. Before/after observations: {:?}. Reduced CSV: {:?}; prompt: {:?}; SQL: {:?}. Check NL/SQL meaning, overfitting, preserved tests, and counterexamples. Reject uncertain semantics.",
            patch_sha256, patch, evidence, self.reduced_csv, self.reduced_prompt, self.reduced_sql
        );
        let answer: ReviewAnswer = self.ask("You are an independent reviewer with a fresh Rig history. You cannot edit source or expectations. Return JSON only.", prompt)?;
        if answer.reason.trim().is_empty() {
            return Err(Outcome::ReviewIncomplete);
        }
        Ok(answer.pass)
    }
}
