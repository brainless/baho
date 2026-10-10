//! Payload-free, incremental repair-agent activity. This sidecar has its own
//! schema (v1); existing attempt manifests/contracts remain schema v1 unchanged.
use rig::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
    HookContext, ModelTurnAction, ModelTurnFinished, OutcomeAction, OutcomeEvent,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const MAX_ACTIVITY_EVENTS: u32 = 2000;

#[derive(Clone)]
pub(crate) struct AgentTrace(Arc<Mutex<State>>);
struct State {
    file: File,
    stage: &'static str,
    start: Instant,
    model_calls: u32,
    active_model: Option<Instant>,
    tools: BTreeMap<String, Instant>,
    io_failed: bool,
    events: u32,
    dropped_events: u32,
}
impl AgentTrace {
    pub fn open(path: &Path, stage: &'static str) -> std::io::Result<Self> {
        Ok(Self(Arc::new(Mutex::new(State {
            file: OpenOptions::new().create(true).append(true).open(path)?,
            stage,
            start: Instant::now(),
            model_calls: 0,
            active_model: None,
            tools: BTreeMap::new(),
            io_failed: false,
            events: 0,
            dropped_events: 0,
        }))))
    }
    pub fn event(&self, name: &'static str, detail: Value) {
        self.0.lock().unwrap().write(name, detail);
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
        s.write(
            "agent_finished",
            json!({"cause":cause,"limit":limit,"elapsed_ms":elapsed,"tool_calls_charged":calls,"dropped_events":dropped_events}),
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
        let event = json!({"schema_version":1,"timestamp_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),"stage":self.stage,"name":name,"model_call":self.model_calls,"detail":detail});
        let result = (|| -> std::io::Result<()> {
            serde_json::to_writer(&mut self.file, &event)?;
            self.file.write_all(b"\n")?;
            self.file.flush()?;
            Ok(())
        })();
        self.io_failed |= result.is_err();
    }
}
fn tool_name(name: &str) -> &str {
    match name {
        "read_file" | "search_text" | "probe" | "run_checks" => name,
        _ => "unknown",
    }
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
        s.write("model_call_finished", json!({"elapsed_ms":elapsed,"input_tokens":event.usage.input_tokens,"output_tokens":event.usage.output_tokens,"total_tokens":event.usage.total_tokens,"cached_input_tokens":event.usage.cached_input_tokens}));
        ModelTurnAction::Continue
    }
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if let Some(tool) = event.tool_name() {
            let mut s = self.0.lock().unwrap();
            let id = format!("{:?}", event.id);
            if s.tools.len() < MAX_ACTIVITY_EVENTS as usize {
                s.tools.insert(id.clone(), Instant::now());
            }
            s.write("tool_call_started", json!({"call_id":id,"tool":tool_name(tool),"argument_bytes":event.tool_args().map_or(0,str::len)}));
        }
        DispatchAction::Proceed
    }
    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(tool) = event.tool_name() {
            let mut s = self.0.lock().unwrap();
            let id = format!("{:?}", event.id);
            let elapsed = s.tools.remove(&id).map(|start| start.elapsed().as_millis());
            let succeeded =
                event.outcome.is_ok() && event.tool_result().is_some_and(|r| !r.is_error());
            s.write("tool_call_finished", json!({"call_id":id,"tool":tool_name(tool),"elapsed_ms":elapsed,"succeeded":succeeded}));
        }
        OutcomeAction::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::{
        AgentBuilder,
        test_utils::{MockCompletionModel, MockTurn},
        tool::PortableTool,
    };
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
            "private-id",
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
}
