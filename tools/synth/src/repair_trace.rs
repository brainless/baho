//! Incremental repair-agent activity ledger. This sidecar has its own schema
//! (v3, Epic 019); existing attempt manifests/contracts are unaffected.
//!
//! The ledger records what an agent asked for (a safe projection of tool
//! arguments) and which source spans reached it (file hash, line span, byte
//! count). It never records source text, prompt text, CSV/SQL text, or model
//! output; exposure is reproducible from the pinned checkout and these spans.
use rig::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
    HookContext, ModelTurnAction, ModelTurnFinished, OutcomeAction, OutcomeEvent,
};
use rig::core::completion::FinishReason;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::redaction::Redactor;
use crate::repair_agents::{CheckArgs, ProbeArgs};
use crate::repair_progress::RepairProgress;
use crate::source_tools::{
    Exposed, OutlineArgs, ReadLinesArgs, SOURCE_TOOL_NAMES, SearchTextArgs, ToolErrorCode,
};

const MAX_ACTIVITY_EVENTS: u32 = 2000;
/// V2 added the request and exposure ledger; v3 adds normalized finish reasons
/// and applied output-token caps. Older traces remain readable by their version.
pub(crate) const TRACE_SCHEMA_VERSION: u32 = 3;
const MAX_VERBATIM_CHARS: usize = 256;
// A validated contract allows 200 search matches, each with an excerpt and
// an enclosing item label exposure.
const MAX_EXPOSED_PER_CALL: usize = 400;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum TraceReadError {
    #[error("agent trace schema {found} is not supported by a schema {supported} reader")]
    UnsupportedSchema { found: u64, supported: u32 },
    #[error("malformed agent trace line")]
    Malformed,
}

/// Parses one trace line for a reader that understands exactly `supported`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn read_event(line: &str, supported: u32) -> Result<Value, TraceReadError> {
    let event: Value = serde_json::from_str(line).map_err(|_| TraceReadError::Malformed)?;
    let found = event
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or(TraceReadError::Malformed)?;
    if found != u64::from(supported) {
        return Err(TraceReadError::UnsupportedSchema { found, supported });
    }
    Ok(event)
}

#[derive(Clone)]
pub(crate) struct AgentTrace(Arc<Mutex<State>>);
struct State {
    file: File,
    stage: &'static str,
    start: Instant,
    model_calls: u32,
    active_model: Option<Instant>,
    last_finish_reason: Option<FinishReason>,
    failure_http_status: Option<u16>,
    tools: BTreeMap<String, Instant>,
    io_failed: bool,
    events: u32,
    dropped_events: u32,
    redactor: Option<Redactor>,
    progress: Option<RepairProgress>,
    source_bytes_exposed: usize,
    /// Exposed inclusive line spans per (path, file hash).
    spans: BTreeMap<(String, String), Vec<(usize, usize)>>,
}
impl AgentTrace {
    pub fn open(path: &Path, stage: &'static str) -> std::io::Result<Self> {
        Ok(Self(Arc::new(Mutex::new(State {
            file: OpenOptions::new().create(true).append(true).open(path)?,
            stage,
            start: Instant::now(),
            model_calls: 0,
            active_model: None,
            last_finish_reason: None,
            failure_http_status: None,
            tools: BTreeMap::new(),
            io_failed: false,
            events: 0,
            dropped_events: 0,
            redactor: None,
            progress: None,
            source_bytes_exposed: 0,
            spans: BTreeMap::new(),
        }))))
    }
    /// Without a redactor, search queries and other model-chosen strings are
    /// never logged verbatim.
    pub fn with_redactor(self, redactor: Redactor) -> Self {
        self.0.lock().unwrap().redactor = Some(redactor);
        self
    }
    pub fn with_progress(self, progress: RepairProgress) -> Self {
        self.0.lock().unwrap().progress = Some(progress);
        self
    }
    pub fn event(&self, name: &'static str, detail: Value) {
        self.0.lock().unwrap().write(name, detail);
    }
    pub fn last_turn_hit_output_limit(&self) -> bool {
        matches!(
            self.0.lock().unwrap().last_finish_reason,
            Some(FinishReason::Length)
        )
    }
    pub fn record_failure_http_status(&self, status: Option<u16>) {
        if let Some(status) = status {
            let mut state = self.0.lock().unwrap();
            state.failure_http_status = Some(status);
            state.write("provider_request_failed", json!({"http_status":status}));
        }
    }
    pub fn failure_http_status(&self) -> Option<u16> {
        self.0.lock().unwrap().failure_http_status
    }
    pub fn finish(&self, cause: &'static str, limit: Option<u64>, calls: u32) -> bool {
        let mut s = self.0.lock().unwrap();
        if let Some(start) = s.active_model.take() {
            s.write(
                "model_call_interrupted",
                json!({"elapsed_ms":start.elapsed().as_millis(),"cause":cause}),
            );
        }
        for (id, start) in std::mem::take(&mut s.tools) {
            s.write(
                "tool_call_interrupted",
                json!({"call_id":id,"elapsed_ms":start.elapsed().as_millis(),"cause":cause}),
            );
        }
        let elapsed = s.start.elapsed().as_millis();
        let dropped_events = s.dropped_events;
        let files = s.exposed_files();
        let bytes = s.source_bytes_exposed;
        s.write(
            "agent_finished",
            json!({"cause":cause,"limit":limit,"elapsed_ms":elapsed,"tool_calls_charged":calls,"dropped_events":dropped_events,"source_bytes_exposed":bytes,"source_files_exposed":files}),
        );
        !s.io_failed
    }
}
impl State {
    fn write(&mut self, name: &str, detail: Value) {
        if self.events >= MAX_ACTIVITY_EVENTS && name != "agent_finished" {
            self.dropped_events = self.dropped_events.saturating_add(1);
            return;
        }
        self.events += 1;
        let event = json!({"schema_version":TRACE_SCHEMA_VERSION,"timestamp_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),"stage":self.stage,"name":name,"model_call":self.model_calls,"detail":detail});
        let result = (|| -> std::io::Result<()> {
            serde_json::to_writer(&mut self.file, &event)?;
            self.file.write_all(b"\n")?;
            self.file.flush()?;
            Ok(())
        })();
        self.io_failed |= result.is_err();
        if result.is_ok() {
            if let Some(progress) = &self.progress {
                progress.agent(self.stage, name, &detail, self.model_calls);
            }
        }
    }
    fn record_exposure(&mut self, exposed: &[Exposed]) {
        for e in exposed {
            self.source_bytes_exposed = self.source_bytes_exposed.saturating_add(e.bytes);
            self.spans
                .entry((e.path.clone(), e.file_sha256.clone()))
                .or_default()
                .push((e.start_line, e.end_line));
        }
    }
    /// Per-file union of exposed spans, merging overlapping and adjacent lines.
    fn exposed_files(&self) -> Vec<Value> {
        self.spans
            .iter()
            .map(|((path, sha), spans)| {
                let mut sorted = spans.clone();
                sorted.sort_unstable();
                let mut merged: Vec<(usize, usize)> = Vec::new();
                for (start, end) in sorted {
                    match merged.last_mut() {
                        Some(last) if start <= last.1.saturating_add(1) => {
                            last.1 = last.1.max(end);
                        }
                        _ => merged.push((start, end)),
                    }
                }
                let lines: usize = merged.iter().map(|(a, b)| b - a + 1).sum();
                json!({"path":path,"file_sha256":sha,"lines":lines,"spans":merged})
            })
            .collect()
    }
}
fn tool_name(name: &str) -> &str {
    match name {
        "outline" | "read_lines" | "search_text" | "probe" | "run_checks" => name,
        _ => "unknown",
    }
}

/// Rig's call id when the model supplied one; model-chosen, so it is reduced
/// to a short safe alphabet. Falls back to the dispatch's effect id.
fn call_key(call_id: Option<&impl std::fmt::Display>, effect: impl std::fmt::Debug) -> String {
    match call_id {
        Some(id) => id
            .to_string()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .take(64)
            .collect(),
        None => format!("{effect:?}"),
    }
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Builds an argument projection. Model-chosen strings are logged verbatim
/// only when the redactor finds nothing in them.
struct Projection<'a> {
    redactor: Option<&'a Redactor>,
    out: Map<String, Value>,
    withheld: BTreeSet<String>,
}
impl<'a> Projection<'a> {
    fn new(redactor: Option<&'a Redactor>) -> Self {
        Self {
            redactor,
            out: Map::new(),
            withheld: BTreeSet::new(),
        }
    }
    fn put(&mut self, key: &str, value: Value) {
        self.out.insert(key.into(), value);
    }
    fn verbatim(&mut self, key: &str, text: &str) {
        let kinds: Vec<String> = match self.redactor {
            Some(r) => r
                .scan(text)
                .into_iter()
                .filter_map(|k| serde_json::to_value(k).ok()?.as_str().map(str::to_owned))
                .collect(),
            None => vec!["no_redactor".into()],
        };
        if kinds.is_empty() && text.chars().count() <= MAX_VERBATIM_CHARS {
            self.put(key, json!(text));
        } else {
            self.put(&format!("{key}_sha256"), json!(sha(text)));
            self.put(&format!("{key}_chars"), json!(text.chars().count()));
            self.withheld.extend(kinds);
            if text.chars().count() > MAX_VERBATIM_CHARS {
                self.withheld.insert("too_long".into());
            }
        }
    }
    /// Text that may carry dataset content: hash and size only.
    fn opaque(&mut self, key: &str, text: &str) {
        self.put(key, json!({"sha256":sha(text),"bytes":text.len()}));
    }
    fn finish(mut self) -> Value {
        if !self.withheld.is_empty() {
            self.out.insert("withheld".into(), json!(self.withheld));
        }
        Value::Object(self.out)
    }
}

fn project_args(tool: &str, args: &str, redactor: Option<&Redactor>) -> Value {
    let mut p = Projection::new(redactor);
    let ok = match tool {
        "outline" => serde_json::from_str::<OutlineArgs>(args)
            .map(|a| {
                p.verbatim("path", &a.path);
                p.put("start_line", json!(a.start_line));
            })
            .is_ok(),
        "read_lines" => serde_json::from_str::<ReadLinesArgs>(args)
            .map(|a| {
                p.verbatim("path", &a.path);
                p.put("ranges", json!(a.ranges));
            })
            .is_ok(),
        "search_text" => serde_json::from_str::<SearchTextArgs>(args)
            .map(|a| {
                p.verbatim("query", &a.query);
            })
            .is_ok(),
        "probe" => serde_json::from_str::<ProbeArgs>(args)
            .map(|a| {
                p.verbatim("build_id", &a.build_id);
                p.opaque("csv", &a.csv);
                p.opaque("prompt", &a.prompt);
                if let Some(sql) = &a.sql {
                    p.opaque("sql", sql);
                }
            })
            .is_ok(),
        "run_checks" => serde_json::from_str::<CheckArgs>(args)
            .map(|a| {
                p.verbatim("build_id", &a.build_id);
                p.put("suite", json!(a.suite));
            })
            .is_ok(),
        _ => false,
    };
    if !ok {
        return json!({"args_invalid":true,"argument_bytes":args.len()});
    }
    p.finish()
}

/// Outcome fields for `tool_call_finished` taken from a source tool's
/// structured result. Only counts, spans, hashes, and enum values are copied;
/// response text never is.
fn project_outcome(tool: &str, response: &Value) -> (Map<String, Value>, Vec<Exposed>) {
    let mut out = Map::new();
    let mut copy = |key: &str| {
        if let Some(v) = response.get(key).filter(|v| v.is_boolean() || v.is_u64()) {
            out.insert(key.into(), v.clone());
        }
    };
    for key in [
        "truncated",
        "next_line",
        "bytes",
        "files_skipped",
        "files_searched",
    ] {
        copy(key);
    }
    if let Some(kind) = response
        .get("outline_kind")
        .and_then(Value::as_str)
        .filter(|k| matches!(*k, "syn" | "fallback"))
    {
        out.insert("outline_kind".into(), json!(kind));
    }
    if tool == "search_text" {
        let n = response
            .get("matches")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        out.insert("match_count".into(), json!(n));
    }
    if tool == "read_lines" {
        let ranges: Vec<Value> = response
            .get("ranges")
            .and_then(Value::as_array)
            .map(|rs| {
                rs.iter()
                    .take(8)
                    .map(|r| {
                        json!({
                            "start":r.get("start"),
                            "end":r.get("end"),
                            "status":r.get("status").filter(|s| s.is_string()),
                            "served_end":r.get("served_end"),
                            "next_line":r.get("next_line"),
                            "line_truncated":r.get("line_truncated"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.insert("ranges".into(), json!(ranges));
    }
    let exposed: Vec<Exposed> = response
        .get("exposed")
        .and_then(|e| serde_json::from_value(e.clone()).ok())
        .map(|mut v: Vec<Exposed>| {
            v.truncate(MAX_EXPOSED_PER_CALL);
            v
        })
        .unwrap_or_default();
    (out, exposed)
}

impl AgentHook for AgentTrace {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        _: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        let mut s = self.0.lock().unwrap();
        s.model_calls += 1;
        s.active_model = Some(Instant::now());
        s.last_finish_reason = None;
        s.write("model_call_started", json!({}));
        CompletionCallAction::Continue
    }
    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        let mut s = self.0.lock().unwrap();
        let elapsed = s
            .active_model
            .take()
            .map(|start| start.elapsed().as_millis());
        let finish_reason = match event.finish_reason {
            Some(FinishReason::Stop) => Some("stop"),
            Some(FinishReason::Length) => Some("length"),
            Some(FinishReason::ToolCalls) => Some("tool_calls"),
            Some(FinishReason::ContentFilter) => Some("content_filter"),
            Some(FinishReason::Other(_)) => Some("other"),
            None => None,
        };
        s.last_finish_reason = event.finish_reason.cloned();
        s.write("model_call_finished", json!({"elapsed_ms":elapsed,"input_tokens":event.usage.input_tokens,"output_tokens":event.usage.output_tokens,"total_tokens":event.usage.total_tokens,"cached_input_tokens":event.usage.cached_input_tokens,"finish_reason":finish_reason,"max_tokens":event.max_tokens}));
        ModelTurnAction::Continue
    }
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if let Some(tool) = event.tool_name() {
            let mut s = self.0.lock().unwrap();
            let id = call_key(event.call_id, event.id);
            let tool = tool_name(tool);
            if s.tools.len() < MAX_ACTIVITY_EVENTS as usize {
                s.tools.insert(id.clone(), Instant::now());
            }
            let args = event.tool_args().unwrap_or_default();
            s.write(
                "tool_call_started",
                json!({"call_id":id,"tool":tool,"argument_bytes":args.len()}),
            );
            let projection = project_args(tool, args, s.redactor.as_ref());
            s.write(
                "tool_call_requested",
                json!({"call_id":id,"tool":tool,"args":projection}),
            );
        }
        DispatchAction::Proceed
    }
    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(tool) = event.tool_name() {
            let mut s = self.0.lock().unwrap();
            let id = call_key(event.call_id, event.id);
            let tool = tool_name(tool);
            let elapsed = s.tools.remove(&id).map(|start| start.elapsed().as_millis());
            let result = event.tool_result();
            let succeeded = event.outcome.is_ok() && result.is_some_and(|r| !r.is_error());
            // A failed call's model-visible text may be generic; the structured
            // error message carries the typed code.
            let rendered = result.map(|r| {
                let mut text = r.output().render();
                if let Some(e) = r.error().or(r.refusal()) {
                    text.push(' ');
                    text.push_str(e.message());
                }
                text
            });
            let mut detail = Map::new();
            let mut code = if succeeded { "ok" } else { "tool_error" };
            let mut exposed = Vec::new();
            if SOURCE_TOOL_NAMES.contains(&tool) {
                if succeeded {
                    if let Some(response) = rendered
                        .as_deref()
                        .and_then(|t| serde_json::from_str::<Value>(t).ok())
                    {
                        if let Some(c) = response
                            .get("code")
                            .and_then(Value::as_str)
                            .and_then(ToolErrorCode::find_in)
                        {
                            code = c.as_str();
                        }
                        (detail, exposed) = project_outcome(tool, &response);
                    }
                    if let Some(body) = rendered.as_deref() {
                        // A digest of the rendered tool result lets a replay
                        // verify generated outline/search text and numbered
                        // read output without persisting any source payload.
                        detail.insert("response_sha256".into(), json!(sha(body)));
                        detail.insert("response_bytes".into(), json!(body.len()));
                    }
                } else if let Some(c) = result
                    .and_then(|r| r.error())
                    .and_then(|e| e.code())
                    .and_then(ToolErrorCode::find_in)
                    .or_else(|| rendered.as_deref().and_then(ToolErrorCode::find_in))
                {
                    code = c.as_str();
                }
            }
            if invalid_sql_probe_failure(tool, succeeded, rendered.as_deref()) {
                code = "invalid_sql";
            }
            s.record_exposure(&exposed);
            detail.insert("call_id".into(), json!(id));
            detail.insert("tool".into(), json!(tool));
            detail.insert("elapsed_ms".into(), json!(elapsed));
            detail.insert("succeeded".into(), json!(succeeded));
            detail.insert("code".into(), json!(code));
            detail.insert("exposed".into(), json!(exposed));
            s.write("tool_call_finished", Value::Object(detail));
        }
        OutcomeAction::Proceed
    }
}

fn invalid_sql_probe_failure(tool: &str, succeeded: bool, rendered: Option<&str>) -> bool {
    tool == "probe" && !succeeded && rendered.is_some_and(|text| text.contains("invalid_sql:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_sql_probe_code_is_scoped_and_value_free() {
        assert!(invalid_sql_probe_failure(
            "probe",
            false,
            Some("invalid_sql: use fixed grammar")
        ));
        assert!(!invalid_sql_probe_failure(
            "probe",
            true,
            Some("invalid_sql: example")
        ));
        assert!(!invalid_sql_probe_failure(
            "read_lines",
            false,
            Some("invalid_sql: example")
        ));
        assert!(!invalid_sql_probe_failure(
            "probe",
            false,
            Some("repair tool denied request")
        ));
    }
    use crate::source_tools::{
        LineRange, OutlineTool, ReadLinesTool, SearchTextTool, SourceLimits, SourceSession,
    };
    use rig::{
        AgentBuilder,
        test_utils::{MockCompletionModel, MockTurn},
        tool::PortableTool,
    };
    #[tokio::test]
    async fn model_trace_records_finish_reason_and_cap_without_provider_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-events.jsonl");
        let trace = AgentTrace::open(&path, "investigation").unwrap();
        let agent = AgentBuilder::new(MockCompletionModel::from_turns([MockTurn::text(
            "partial answer",
        )
        .with_finish_reason(FinishReason::Length)]))
        .max_tokens(16_384)
        .add_hook(trace.clone())
        .build();
        agent.prompt("private prompt").await.unwrap();
        assert!(trace.finish("completed", None, 0));
        let events: Vec<Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let turn = events
            .iter()
            .find(|event| event["name"] == "model_call_finished")
            .unwrap();
        assert_eq!(turn["detail"]["finish_reason"], "length");
        assert_eq!(turn["detail"]["max_tokens"], 16_384);
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("private prompt")
        );
    }

    #[derive(Clone)]
    struct WaitingTool;
    impl PortableTool for WaitingTool {
        const NAME: &'static str = "probe";
        type Args = Value;
        type Output = Value;
        type Error = std::io::Error;
        fn description(&self) -> String {
            "Wait for test cancellation".into()
        }
        fn parameters(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _: Value) -> Result<Value, std::io::Error> {
            std::future::pending().await
        }
    }
    #[derive(Clone)]
    struct WaitingCompletion;
    impl AgentHook for WaitingCompletion {
        async fn on_completion_call(
            &self,
            _: &HookContext,
            _: CompletionCallEvent<'_>,
        ) -> CompletionCallAction {
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn timeout_preserves_model_start_before_terminal_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-events.jsonl");
        let trace = AgentTrace::open(&path, "investigation").unwrap();
        let agent = AgentBuilder::new(MockCompletionModel::text("unused private response"))
            .add_hook(trace.clone())
            .add_hook(WaitingCompletion)
            .build();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                agent.prompt("private prompt")
            )
            .await
            .is_err()
        );
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.contains("model_call_started"));
        assert!(!before.contains("model_call_finished"));
        assert!(trace.finish("session_timeout", Some(120), 0));
        let after = std::fs::read_to_string(path).unwrap();
        assert!(after.contains("model_call_interrupted"));
        assert!(after.contains("agent_finished"));
        assert!(!after.contains("private"));
    }

    #[tokio::test]
    async fn timeout_preserves_started_tool_and_records_interruption_without_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-events.jsonl");
        let trace = AgentTrace::open(&path, "review").unwrap();
        let model = MockCompletionModel::from_turns([MockTurn::tool_call(
            "call-1",
            "probe",
            json!({"secret":"private value"}),
        )]);
        let agent = AgentBuilder::new(model)
            .add_hook(trace.clone())
            .tool(WaitingTool)
            .build();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                agent.prompt("private prompt")
            )
            .await
            .is_err()
        );
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.contains("tool_call_started"));
        assert!(!before.contains("agent_finished"));
        assert!(trace.finish("session_timeout", Some(120), 1));
        let after = std::fs::read_to_string(path).unwrap();
        assert!(!after.contains("private"));
        assert!(after.contains("tool_call_interrupted"));
        assert!(after.contains("session_timeout"));
    }

    const SCOPE: &str = "crates/baho-core/src/";

    /// A generated Rust file of a few hundred functions plus a marker line the
    /// trace must never echo.
    fn ledger_source() -> String {
        let mut s = String::from("use std::fmt;\n\npub enum Op {\n    Equal,\n}\n\n");
        for i in 0..120 {
            s.push_str(&format!(
                "pub fn generated_{i}(x: u32) -> u32 {{\n    // BODY_MARKER_{i}\n    x + {i}\n}}\n\n"
            ));
        }
        s.push_str("pub fn operator_symbol(s: &str) -> bool {\n    s == \"zebra_literal\"\n}\n");
        s
    }

    fn fixture() -> (tempfile::TempDir, SourceSession, String) {
        let dir = tempfile::tempdir().unwrap();
        let rel = format!("{SCOPE}ledger.rs");
        std::fs::create_dir_all(dir.path().join(SCOPE)).unwrap();
        std::fs::write(dir.path().join(&rel), ledger_source()).unwrap();
        let session = SourceSession::new(
            dir.path().to_path_buf(),
            &[SCOPE],
            SourceLimits {
                max_calls: 50,
                read_bytes: 1024,
                outline_bytes: 6 * 1024,
                search_matches: 40,
                stage_bytes: 48 * 1024,
            },
        );
        (dir, session, rel)
    }

    #[test]
    fn search_trace_keeps_every_excerpt_and_item_label_exposure() {
        let (_dir, session, _rel) = fixture();
        let response = serde_json::to_value(session.search_text("BODY_MARKER").unwrap()).unwrap();
        assert_eq!(response["matches"].as_array().unwrap().len(), 40);
        let (projected, exposed) = project_outcome("search_text", &response);
        assert_eq!(projected["match_count"], 40);
        assert_eq!(exposed.len(), 80);
        assert_eq!(
            exposed.iter().map(|e| e.bytes).sum::<usize>(),
            response["bytes"].as_u64().unwrap() as usize
        );
        assert_eq!(serde_json::to_value(&exposed).unwrap(), response["exposed"]);
    }

    fn events(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| read_event(l, TRACE_SCHEMA_VERSION).unwrap())
            .collect()
    }

    fn named<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
        events.iter().filter(|e| e["name"] == name).collect()
    }

    /// First object anywhere in `v` that has a `ranges` key (a read_lines result).
    fn find_ranges(v: &Value) -> Option<&Value> {
        match v {
            Value::Object(o) if o.contains_key("ranges") && o.contains_key("file_sha256") => {
                Some(v)
            }
            Value::Object(o) => o.values().find_map(find_ranges),
            Value::Array(a) => a.iter().find_map(find_ranges),
            _ => None,
        }
    }

    fn find_response<'a>(v: &'a Value, marker: &str) -> Option<&'a Value> {
        match v {
            Value::Object(o) if o.contains_key(marker) && o.contains_key("exposed") => Some(v),
            Value::Object(o) => o.values().find_map(|value| find_response(value, marker)),
            Value::Array(a) => a.iter().find_map(|value| find_response(value, marker)),
            _ => None,
        }
    }

    #[tokio::test]
    async fn source_ledger_replays_every_exposure_from_protected_tar() {
        use std::process::Command;

        let (dir, session, rel) = fixture();
        let protected = dir.path().join("protected");
        std::fs::create_dir(&protected).unwrap();
        let archive = protected.join("source.tar");
        assert!(
            Command::new("tar")
                .args(["-cf"])
                .arg(&archive)
                .args(["-C"])
                .arg(dir.path())
                .arg("crates")
                .status()
                .unwrap()
                .success()
        );

        let trace_path = dir.path().join("agent-events.jsonl");
        let trace = AgentTrace::open(&trace_path, "investigation")
            .unwrap()
            .with_redactor(Redactor::new(["private_literal"]).unwrap());
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("outline", "outline", json!({"path":rel})),
            MockTurn::tool_call("search", "search_text", json!({"query":"operator_symbol"})),
            MockTurn::tool_call(
                "read",
                "read_lines",
                json!({"path":rel,"ranges":[{"start":8,"end":12},{"start":10,"end":14}]}),
            ),
            MockTurn::text("done"),
        ]);
        let received = model.clone();
        let agent = AgentBuilder::new(model)
            .add_hook(trace.clone())
            .tool(OutlineTool(session.clone()))
            .tool(ReadLinesTool(session.clone()))
            .tool(SearchTextTool(session))
            .build();
        agent.prompt("go").max_turns(6).await.unwrap();
        assert!(trace.finish("completed", None, 3));

        let checkout = dir.path().join("replay");
        std::fs::create_dir(&checkout).unwrap();
        assert!(
            Command::new("tar")
                .arg("-xf")
                .arg(&archive)
                .arg("-C")
                .arg(&checkout)
                .status()
                .unwrap()
                .success()
        );
        let replay = fixture_session(&checkout);
        let expected = [
            (
                "outline",
                serde_json::to_value(replay.outline(&rel, None).unwrap()).unwrap(),
            ),
            (
                "search",
                serde_json::to_value(replay.search_text("operator_symbol").unwrap()).unwrap(),
            ),
            (
                "read",
                serde_json::to_value(
                    replay
                        .read_lines(
                            &rel,
                            &[
                                LineRange { start: 8, end: 12 },
                                LineRange { start: 10, end: 14 },
                            ],
                        )
                        .unwrap(),
                )
                .unwrap(),
            ),
        ];
        let history =
            serde_json::to_value(&received.requests().last().unwrap().chat_history).unwrap();
        let events = events(&trace_path);
        let finished = named(&events, "tool_call_finished");
        for (id, output) in expected {
            let recorded = &finished
                .iter()
                .find(|event| event["detail"]["call_id"] == id)
                .unwrap()["detail"];
            let marker = match id {
                "outline" => "outline_kind",
                "search" => "matches",
                _ => "ranges",
            };
            assert_eq!(
                find_response(&history, marker).unwrap(),
                &output,
                "mock received a different {id} result"
            );
            let rendered = output.to_string();
            assert_eq!(recorded["response_sha256"], sha(&rendered));
            assert_eq!(recorded["response_bytes"], rendered.len());
            assert_eq!(recorded["exposed"], output["exposed"]);
            for span in recorded["exposed"].as_array().unwrap() {
                let member = span["path"].as_str().unwrap();
                let tar_bytes = Command::new("tar")
                    .arg("-xOf")
                    .arg(&archive)
                    .arg(member)
                    .output()
                    .unwrap();
                assert!(tar_bytes.status.success());
                assert_eq!(
                    span["file_sha256"],
                    format!("{:x}", Sha256::digest(&tar_bytes.stdout))
                );
            }
        }
    }

    fn fixture_session(root: &Path) -> SourceSession {
        SourceSession::new(
            root.to_path_buf(),
            &[SCOPE],
            SourceLimits {
                max_calls: 50,
                read_bytes: 1024,
                outline_bytes: 6 * 1024,
                search_matches: 40,
                stage_bytes: 48 * 1024,
            },
        )
    }

    #[tokio::test]
    async fn ledger_records_requests_and_exposure_without_source_text() {
        let (dir, session, rel) = fixture();
        let trace_path = dir.path().join("agent-events.jsonl");
        let redactor = Redactor::new(["zebra_literal"]).unwrap();
        let trace = AgentTrace::open(&trace_path, "investigation")
            .unwrap()
            .with_redactor(redactor);
        let model = MockCompletionModel::from_turns([
            MockTurn::tool_call("c-outline", "outline", json!({"path":rel})),
            MockTurn::tool_call(
                "c-search-safe",
                "search_text",
                json!({"query":"operator_symbol"}),
            ),
            MockTurn::tool_call(
                "c-search-lit",
                "search_text",
                json!({"query":"zebra_literal"}),
            ),
            MockTurn::tool_call(
                "c-read",
                "read_lines",
                json!({"path":rel,"ranges":[{"start":8,"end":12},{"start":10,"end":14}]}),
            ),
            MockTurn::tool_call(
                "c-denied",
                "read_lines",
                json!({"path":"../escape.rs","ranges":[{"start":1,"end":2}]}),
            ),
            MockTurn::tool_call("c-bad", "read_lines", json!({"path":rel,"range":"x"})),
            MockTurn::text("done"),
        ]);
        let received = model.clone();
        let agent = AgentBuilder::new(model)
            .add_hook(trace.clone())
            .tool(OutlineTool(session.clone()))
            .tool(ReadLinesTool(session.clone()))
            .tool(SearchTextTool(session.clone()))
            .build();
        agent.prompt("go").max_turns(10).await.unwrap();
        assert!(trace.finish("completed", None, 6));

        let raw = std::fs::read_to_string(&trace_path).unwrap();
        for forbidden in [
            "BODY_MARKER",
            "zebra_literal",
            "generated_5(",
            "pub enum Op",
        ] {
            assert!(!raw.contains(forbidden), "{forbidden} leaked into trace");
        }
        let ev = events(&trace_path);
        let requested = named(&ev, "tool_call_requested");
        assert_eq!(requested.len(), 6);
        let by_id = |id: &str| -> &Value {
            requested
                .iter()
                .find(|e| e["detail"]["call_id"] == id)
                .unwrap()
        };
        assert_eq!(by_id("c-outline")["detail"]["args"]["path"], rel);
        assert_eq!(
            by_id("c-search-safe")["detail"]["args"]["query"],
            "operator_symbol"
        );
        let lit = &by_id("c-search-lit")["detail"]["args"];
        assert!(lit.get("query").is_none());
        assert_eq!(lit["query_chars"], 13);
        assert_eq!(lit["query_sha256"].as_str().unwrap().len(), 64);
        assert_eq!(lit["withheld"], json!(["source_literal"]));
        assert_eq!(
            by_id("c-bad")["detail"]["args"],
            json!({"args_invalid":true,"argument_bytes":json!({"path":rel,"range":"x"}).to_string().len()})
        );

        let finished = named(&ev, "tool_call_finished");
        let fin = |id: &str| -> &Value {
            finished
                .iter()
                .find(|e| e["detail"]["call_id"] == id)
                .unwrap()
        };
        assert_eq!(fin("c-outline")["detail"]["code"], "ok");
        assert_eq!(fin("c-outline")["detail"]["outline_kind"], "syn");
        assert_eq!(fin("c-search-safe")["detail"]["match_count"], 1);
        assert_eq!(fin("c-denied")["detail"]["code"], "path_not_allowed");
        assert_eq!(fin("c-denied")["detail"]["exposed"], json!([]));
        assert_eq!(fin("c-bad")["detail"]["succeeded"], false);

        // Replay: the recorded spans, read back from the source, are exactly
        // the bytes the mock model received from read_lines.
        let source = std::fs::read(dir.path().join(&rel)).unwrap();
        let file_sha = format!("{:x}", Sha256::digest(&source));
        let text = String::from_utf8(source).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let exposed = fin("c-read")["detail"]["exposed"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(exposed.len(), 2);
        let mut replayed = Vec::new();
        for e in &exposed {
            assert_eq!(e["file_sha256"], file_sha.as_str());
            assert_eq!(e["path"], rel);
            let (a, b) = (
                e["start_line"].as_u64().unwrap() as usize,
                e["end_line"].as_u64().unwrap() as usize,
            );
            let span: Vec<&str> = lines[a - 1..b].to_vec();
            replayed.extend(span.iter().map(|l| format!("{l}\n")));
        }
        let history: Vec<Value> = received
            .requests()
            .iter()
            .map(|r| serde_json::to_value(&r.chat_history).unwrap())
            .collect();
        let response = history
            .iter()
            .find_map(find_ranges)
            .expect("read_lines result reached the model");
        let mut got = Vec::new();
        for (r, e) in response["ranges"].as_array().unwrap().iter().zip(&exposed) {
            assert_eq!(e["bytes"], r["text"].as_str().unwrap().len());
            for l in r["text"].as_str().unwrap().lines() {
                let body = l.split_once('|').unwrap().1;
                got.push(format!("{}\n", body.strip_prefix(' ').unwrap_or(body)));
            }
        }
        assert_eq!(got, replayed);

        let last = ev.last().unwrap();
        assert_eq!(last["name"], "agent_finished");
        let exposed_bytes: u64 = finished
            .iter()
            .flat_map(|e| e["detail"]["exposed"].as_array().unwrap())
            .map(|e| e["bytes"].as_u64().unwrap())
            .sum();
        assert!(exposed_bytes > 0);
        assert_eq!(last["detail"]["source_bytes_exposed"], exposed_bytes);
        let file = &last["detail"]["source_files_exposed"][0];
        assert_eq!(file["file_sha256"], file_sha.as_str());
        // Overlapping reads and outline/search spans merge into disjoint spans.
        let spans = file["spans"].as_array().unwrap();
        let ranges: Vec<(u64, u64)> = spans
            .iter()
            .map(|s| (s[0].as_u64().unwrap(), s[1].as_u64().unwrap()))
            .collect();
        assert!(ranges.iter().any(|&(a, b)| a <= 8 && b >= 14), "{ranges:?}");
        assert!(ranges.windows(2).all(|w| w[0].1 + 1 < w[1].0), "{ranges:?}");
        assert_eq!(session.exposed_bytes() as u64, exposed_bytes);
    }

    #[test]
    fn probe_arguments_are_hashes_and_sizes_only() {
        let args =
            json!({"build_id":"b1","csv":"a,b\n1,2","prompt":"sum a","sql":"select 1"}).to_string();
        let p = project_args("probe", &args, Some(&Redactor::new(["zzz"]).unwrap()));
        assert_eq!(p["csv"]["bytes"], 7);
        assert_eq!(p["csv"]["sha256"].as_str().unwrap().len(), 64);
        assert_eq!(p["sql"]["bytes"], 8);
        let text = p.to_string();
        assert!(!text.contains("a,b") && !text.contains("sum a") && !text.contains("select"));
        let checks = project_args(
            "run_checks",
            &json!({"build_id":"b1","suite":"owning_crate"}).to_string(),
            None,
        );
        assert_eq!(checks["suite"], "owning_crate");
        // No redactor: model-chosen strings are withheld, not trusted.
        let q = project_args("search_text", &json!({"query":"abc"}).to_string(), None);
        assert!(q.get("query").is_none());
        assert_eq!(q["withheld"], json!(["no_redactor"]));
    }

    #[test]
    fn trace_reader_requires_matching_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-events.jsonl");
        let trace = AgentTrace::open(&path, "review").unwrap();
        assert!(trace.finish("completed", None, 0));
        let line = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(
            read_event(&line, 1),
            Err(TraceReadError::UnsupportedSchema {
                found: 3,
                supported: 1
            })
        );
        assert_eq!(
            read_event(&line, 2),
            Err(TraceReadError::UnsupportedSchema {
                found: 3,
                supported: 2
            })
        );
        assert!(read_event(&line, 3).is_ok());
        assert!(read_event(r#"{"schema_version":2,"name":"agent_finished"}"#, 2).is_ok());
        assert_eq!(read_event("not json", 3), Err(TraceReadError::Malformed));
    }
}
