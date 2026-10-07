//! Developer-only, bounded generator. The provider never receives source paths or records.
use crate::{
    dataset::LoadedDataset,
    sql::{QueryResult, SqlRunner, parse_role_query},
};
use futures::StreamExt;
use rig::agent::{
    AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent,
    HookContext, ModelTurnAction, ModelTurnFinished, OutcomeAction, OutcomeEvent,
};
use rig::{
    AgentBuilder, DynModel,
    operation::Completion,
    providers::openai::OpenAIConfig,
    tool::{PortableTool, ToolExecutionError},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const PROVIDER: &str = "Neuralwatt";
pub const BASE_URL: &str = "https://api.neuralwatt.com/v1";
pub const MODEL: &str = "deepseek-v4.1-flash-flex";
pub const PROMPT_VERSION: &str = "role-equality-v1";
const SYSTEM: &str = include_str!("../prompts/generator-system.md");

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct GenerateError(pub String);

pub fn credential(value: Option<String>) -> Result<String, GenerateError> {
    value
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| GenerateError("NEURALWATT_API_KEY is missing or empty".into()))
}

pub fn neuralwatt_model(key: String) -> impl Into<DynModel<Completion>> {
    OpenAIConfig::new(key)
        .with_base_url(BASE_URL)
        .client()
        .chat(MODEL)
}

#[derive(Clone, Debug, Serialize)]
pub struct Settings {
    pub count: usize,
    pub max_turns: usize,
    pub per_call_seconds: u64,
    pub whole_run_seconds: u64,
    pub variation: String,
}
impl Settings {
    pub fn new(count: usize) -> Result<Self, GenerateError> {
        if !(1..=100).contains(&count) {
            return Err(GenerateError("count must be between 1 and 100".into()));
        }
        Ok(Self {
            count,
            max_turns: count * 4 + 4,
            per_call_seconds: 900,
            whole_run_seconds: 3600,
            variation: "Vary polite and direct requests, quoting, word order, and letter case."
                .into(),
        })
    }
    pub fn validate(&self) -> Result<(), GenerateError> {
        if !(1..=100).contains(&self.count)
            || !(1..=404).contains(&self.max_turns)
            || !(1..=7200).contains(&self.per_call_seconds)
            || !(1..=86400).contains(&self.whole_run_seconds)
            || self.variation.len() > 1024
        {
            return Err(GenerateError(
                "generation settings exceed supported bounds".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub schema_version: u32,
    pub case_id: String,
    pub dataset_id: String,
    pub dataset_hash: String,
    pub feature_tag: String,
    pub prompt: String,
    pub sql: String,
    pub expected_rows: Vec<u64>,
    pub statistics: Statistics,
    pub generation_record_id: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Statistics {
    pub count: usize,
    pub selectivity: f64,
    pub is_empty: bool,
    pub is_all_rows: bool,
}
impl From<&QueryResult> for Statistics {
    fn from(v: &QueryResult) -> Self {
        Self {
            count: v.count,
            selectivity: v.selectivity,
            is_empty: v.is_empty,
            is_all_rows: v.is_all_rows,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct CallRecord {
    pub turn: usize,
    pub elapsed_ms: u128,
    pub requested_tier: &'static str,
    pub served_tier: String,
    pub standard_tier: bool,
    pub interrupted: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct ToolRecord {
    pub turn: usize,
    pub effect_id: String,
    pub tool: String,
    pub event: &'static str,
    pub succeeded: Option<bool>,
    pub argument_bytes: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct GenerationRecord {
    pub schema_version: u32,
    pub id: String,
    pub provider: &'static str,
    pub model: &'static str,
    pub route: &'static str,
    pub rig_version: &'static str,
    pub prompt_version: &'static str,
    pub feature_hash: String,
    pub dataset_hash: String,
    pub settings: Settings,
    pub outcome: String,
    pub calls: Vec<CallRecord>,
    pub tools: Vec<ToolRecord>,
    pub rejected_submissions: Vec<String>,
    pub accepted: usize,
}
struct State {
    dataset: Arc<LoadedDataset>,
    settings: Settings,
    id: String,
    candidates: Vec<Candidate>,
    rejected: Vec<String>,
    tools: Vec<ToolRecord>,
    calls: Vec<CallRecord>,
    active_call: Option<(usize, Instant)>,
}
type Shared = Arc<Mutex<State>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyArgs {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlArgs {
    pub sql: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitArgs {
    pub prompt: String,
    pub sql: String,
    pub feature_tag: String,
    pub rationale: Option<String>,
}
#[derive(Clone)]
struct Describe(Shared);
impl PortableTool for Describe {
    const NAME: &'static str = "describe_dataset";
    type Args = EmptyArgs;
    type Output = Value;
    type Error = GenerateError;
    fn description(&self) -> String {
        "Read the restricted Role profile; no other source fields are available.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn call(&self, _: EmptyArgs) -> Result<Value, GenerateError> {
        Ok(json!(self.0.lock().unwrap().dataset.profile))
    }
}
#[derive(Clone)]
struct RunSql(Shared);
async fn execute(state: &Shared, sql: String) -> Result<QueryResult, GenerateError> {
    let dataset = state.lock().unwrap().dataset.clone();
    tokio::task::spawn_blocking(move || {
        SqlRunner::new(
            dataset.database_path(),
            &dataset.profile,
            Duration::from_secs(5),
        )
        .run(&sql)
    })
    .await
    .map_err(|_| GenerateError("SQL worker failed".into()))?
    .map_err(|e| {
        GenerateError(match e {
            crate::sql::SqlError::Grammar | crate::sql::SqlError::UnknownRole => e.to_string(),
            _ => "SQL execution failed".into(),
        })
    })
}
impl PortableTool for RunSql {
    const NAME: &'static str = "run_sql";
    type Args = SqlArgs;
    type Output = Value;
    type Error = GenerateError;
    fn map_error(&self, error: GenerateError) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string()).with_code("harness_rejection")
    }
    fn description(&self) -> String {
        "Check one canonical Role equality SQL query; return count and up to ten source row IDs."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"sql":{"type":"string","maxLength":512}},"required":["sql"],"additionalProperties":false})
    }
    async fn call(&self, args: SqlArgs) -> Result<Value, GenerateError> {
        let result = execute(&self.0, args.sql).await?;
        Ok(
            json!({"count":result.count,"sample_rows":result.rows.into_iter().take(10).collect::<Vec<_>>()}),
        )
    }
}
#[derive(Clone)]
struct Submit(Shared);
fn validate_submission(args: &SubmitArgs, state: &State) -> Result<(), GenerateError> {
    if args.prompt.is_empty()
        || args.prompt.len() > 1024
        || args.rationale.as_ref().is_some_and(|r| r.len() > 512)
        || args.feature_tag != "role_equality"
    {
        return Err(GenerateError("invalid feature tag or field bounds".into()));
    }
    let query = parse_role_query(&args.sql, &state.dataset.profile)
        .map_err(|e| GenerateError(e.to_string()))?;
    let lower = args.prompt.to_ascii_lowercase();
    if !lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word == "role")
        || !lower.contains(&query.literal.to_ascii_lowercase())
    {
        return Err(GenerateError(
            "prompt must explicitly name Role and the SQL literal".into(),
        ));
    }
    if state.candidates.len() >= state.settings.count {
        return Err(GenerateError("accepted submission limit reached".into()));
    }
    Ok(())
}
impl PortableTool for Submit {
    const NAME: &'static str = "submit_case";
    type Args = SubmitArgs;
    type Output = Value;
    type Error = GenerateError;
    fn map_error(&self, error: GenerateError) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string()).with_code("harness_rejection")
    }
    fn description(&self) -> String {
        "Submit a prompt and canonical SQL as an untrusted role_equality candidate. The harness computes expected rows.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"prompt":{"type":"string","maxLength":1024},"sql":{"type":"string","maxLength":512},"feature_tag":{"type":"string","enum":["role_equality"]},"rationale":{"type":"string","maxLength":512}},"required":["prompt","sql","feature_tag"],"additionalProperties":false})
    }
    async fn call(&self, args: SubmitArgs) -> Result<Value, GenerateError> {
        let validation = {
            let state = self.0.lock().unwrap();
            validate_submission(&args, &state)
        };
        validation?;
        let result = execute(&self.0, args.sql.clone()).await?;
        let mut state = self.0.lock().unwrap();
        // Parallel tool calls can finish together; enforce the cap again at insertion.
        validate_submission(&args, &state)?;
        let case_id = format!("{}-{:04}", state.id, state.candidates.len() + 1);
        let hash = state.dataset.hash.clone();
        let id = state.id.clone();
        state.candidates.push(Candidate {
            schema_version: 1,
            case_id: case_id.clone(),
            dataset_id: format!("dataset-{}", &hash[..16]),
            dataset_hash: hash,
            feature_tag: args.feature_tag,
            prompt: args.prompt,
            sql: args.sql,
            expected_rows: result.rows.clone(),
            statistics: Statistics::from(&result),
            generation_record_id: id,
        });
        Ok(
            json!({"accepted":true,"case_id":case_id,"accepted_count":state.candidates.len(),"limit":state.settings.count}),
        )
    }
}

#[derive(Clone)]
struct Transcript(Shared);
pub fn served_tier(raw: &Value) -> String {
    raw.pointer("/additional_params/service_tier")
        .or_else(|| raw.get("service_tier"))
        .and_then(Value::as_str)
        .filter(|v| matches!(*v, "flex" | "standard" | "default" | "priority" | "auto"))
        .unwrap_or("unknown")
        .into()
}
impl AgentHook for Transcript {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        let mut state = self.0.lock().unwrap();
        state.active_call = Some((event.turn, Instant::now()));
        CompletionCallAction::Continue
    }
    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        let mut state = self.0.lock().unwrap();
        let elapsed_ms = state
            .active_call
            .take()
            .map(|(_, start)| start.elapsed().as_millis())
            .unwrap_or(0);
        let tier = served_tier(event.raw);
        state.calls.push(CallRecord {
            turn: event.turn,
            elapsed_ms,
            requested_tier: "flex",
            standard_tier: matches!(tier.as_str(), "standard" | "default"),
            served_tier: tier,
            interrupted: false,
        });
        ModelTurnAction::Continue
    }
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if let Some(tool) = event.tool_name() {
            let mut state = self.0.lock().unwrap();
            if state.tools.len() < 4000 {
                state.tools.push(ToolRecord {
                    turn: event.turn,
                    effect_id: format!("{:?}", event.id),
                    tool: known_tool(tool).into(),
                    event: "dispatch",
                    succeeded: None,
                    argument_bytes: event.tool_args().map(str::len).unwrap_or(0),
                });
            }
        }
        DispatchAction::Proceed
    }
    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(tool) = event.tool_name() {
            let mut state = self.0.lock().unwrap();
            let succeeded =
                event.outcome.is_ok() && event.tool_result().is_some_and(|r| !r.is_error());
            if tool == "submit_case" && !succeeded && state.rejected.len() < 1000 {
                let reason = event
                    .tool_result()
                    .and_then(|r| r.error())
                    .filter(|e| e.code() == Some("harness_rejection"))
                    .map(|e| e.message().to_owned())
                    .unwrap_or_else(|| {
                        "malformed typed submission or rejected tool invocation".into()
                    });
                state.rejected.push(reason);
            }
            if state.tools.len() < 4000 {
                state.tools.push(ToolRecord {
                    turn: event.turn,
                    effect_id: format!("{:?}", event.id),
                    tool: known_tool(tool).into(),
                    event: "outcome",
                    succeeded: Some(succeeded),
                    argument_bytes: event.tool_args().map(str::len).unwrap_or(0),
                });
            }
        }
        OutcomeAction::Proceed
    }
}
fn known_tool(name: &str) -> &str {
    match name {
        "describe_dataset" | "run_sql" | "submit_case" => name,
        _ => "unknown",
    }
}

pub struct Generated {
    pub candidates: Vec<Candidate>,
    pub record: GenerationRecord,
}
pub async fn generate(
    model: impl Into<DynModel<Completion>>,
    dataset: Arc<LoadedDataset>,
    feature: &str,
    settings: Settings,
) -> Result<Generated, GenerateError> {
    settings.validate()?;
    if feature.len() > 16 * 1024 {
        return Err(GenerateError("feature file exceeds 16 KiB".into()));
    }
    let id = format!(
        "generation-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id()
    );
    let hash = dataset.hash.clone();
    if dataset.profile.roles.iter().any(|r| r.value.len() > 256)
        || serde_json::to_vec(&dataset.profile).unwrap().len() > 16 * 1024
    {
        return Err(GenerateError(
            "restricted dataset profile exceeds generation bounds".into(),
        ));
    }
    let prompt = format!(
        "Feature description:\n{feature}\nRestricted dataset profile:\n{}\nRequested count: {}\nVariation hint: {}",
        serde_json::to_string(&dataset.profile).unwrap(),
        settings.count,
        settings.variation
    );
    let state = Arc::new(Mutex::new(State {
        dataset,
        settings: settings.clone(),
        id: id.clone(),
        candidates: vec![],
        rejected: vec![],
        tools: vec![],
        calls: vec![],
        active_call: None,
    }));
    let agent = AgentBuilder::new(model)
        .preamble(SYSTEM)
        .temperature(0.8)
        .max_tokens(4096)
        .add_hook(Transcript(state.clone()))
        .tool(Describe(state.clone()))
        .tool(RunSql(state.clone()))
        .tool(Submit(state.clone()))
        .build();
    let mut stream = agent.prompt(prompt).max_turns(settings.max_turns).stream();
    let whole_deadline = Instant::now() + Duration::from_secs(settings.whole_run_seconds);
    let outcome = loop {
        let call_deadline = state
            .lock()
            .unwrap()
            .active_call
            .map(|(_, t)| t + Duration::from_secs(settings.per_call_seconds))
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(settings.per_call_seconds));
        let deadline = whole_deadline.min(call_deadline);
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), stream.next()).await
        {
            Err(_) => {
                break if Instant::now() >= whole_deadline {
                    "whole_run_timeout"
                } else {
                    "per_call_timeout"
                };
            }
            Ok(None) => break "completed",
            Ok(Some(Err(rig::agent::StreamingError::Prompt(
                rig::prelude::PromptError::MaxTurnsError { .. },
            )))) => break "max_turns",
            Ok(Some(Err(_))) => break "provider_or_agent_error",
            Ok(Some(Ok(_))) => {}
        }
    }
    .to_owned();
    drop(stream);
    let mut state = state.lock().unwrap();
    if let Some((turn, start)) = state.active_call.take() {
        state.calls.push(CallRecord {
            turn,
            elapsed_ms: start.elapsed().as_millis(),
            requested_tier: "flex",
            served_tier: "unknown".into(),
            standard_tier: false,
            interrupted: true,
        });
    }
    let record = GenerationRecord {
        schema_version: 1,
        id,
        provider: PROVIDER,
        model: MODEL,
        route: "https://api.neuralwatt.com/v1/chat/completions",
        rig_version: "0.43.0",
        prompt_version: PROMPT_VERSION,
        feature_hash: format!("{:x}", Sha256::digest(feature.as_bytes())),
        dataset_hash: hash,
        settings,
        outcome,
        calls: state.calls.clone(),
        tools: state.tools.clone(),
        rejected_submissions: state.rejected.clone(),
        accepted: state.candidates.len(),
    };
    Ok(Generated {
        candidates: state.candidates.clone(),
        record,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::Usage;
    use rig::test_utils::{MockCompletionModel, MockStreamEvent, mock_final};

    fn dataset() -> Arc<LoadedDataset> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "Private,Role\nsecret-one,Analyst\nsecret-two,analyst\nsecret-three,Lead"
        )
        .unwrap();
        Arc::new(LoadedDataset::load(file.path()).unwrap())
    }
    fn sql() -> &'static str {
        "SELECT _row FROM data WHERE lower(\"Role\") = lower('Analyst') ORDER BY _row"
    }
    fn turn(name: &str, args: Value) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::tool_call("call", name, args),
            MockStreamEvent::FinalResponse(mock_final(Usage::default())),
        ]
    }
    fn final_turn() -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::FinalResponse(mock_final(Usage::default())),
        ]
    }
    #[test]
    fn credentials_and_fixed_configuration() {
        assert!(
            credential(None)
                .unwrap_err()
                .to_string()
                .contains("NEURALWATT_API_KEY")
        );
        assert!(credential(Some(" ".into())).is_err());
        assert_eq!(credential(Some("key".into())).unwrap(), "key");
        assert_eq!(BASE_URL, "https://api.neuralwatt.com/v1");
        assert_eq!(MODEL, "deepseek-v4.1-flash-flex");
        assert_eq!(
            served_tier(&json!({"additional_params":{"service_tier":"flex"}})),
            "flex"
        );
        assert_eq!(served_tier(&json!({})), "unknown");
        assert_eq!(served_tier(&json!({"service_tier":"standard"})), "standard");
    }
    #[tokio::test]
    async fn streamed_mock_rejections_submission_and_redaction() {
        let model = MockCompletionModel::from_stream_turns([
            turn("describe_dataset", json!({})),
            turn(
                "run_sql",
                json!({"sql":"SELECT * FROM read_csv('/etc/passwd')"}),
            ),
            turn(
                "submit_case",
                json!({"prompt":"Show matching rows","sql":sql(),"feature_tag":"role_equality"}),
            ),
            turn(
                "submit_case",
                json!({"prompt":"Show Role Analyst rows","sql":sql(),"feature_tag":"role_equality"}),
            ),
            final_turn(),
        ]);
        let capture = model.clone();
        let result = generate(
            model,
            dataset(),
            include_str!("../features/role-equality.md"),
            Settings::new(1).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(result.record.outcome, "completed");
        assert_eq!(result.record.accepted, 1);
        assert_eq!(result.candidates[0].expected_rows, [1, 2]);
        assert_eq!(result.record.rejected_submissions.len(), 1);
        assert_eq!(result.record.calls.len(), 5);
        assert_eq!(result.record.tools.len(), 8);
        let requests = format!("{:?}", capture.requests());
        for private in [
            "secret-one",
            "secret-two",
            "secret-three",
            "Private",
            ".duckdb",
        ] {
            assert!(!requests.contains(private));
        }
        let serialized = serde_json::to_string(&result.record).unwrap();
        for private in ["secret-one", "/etc/passwd", "authorization", "api_key"] {
            assert!(!serialized.contains(private));
        }
        let directory = tempfile::tempdir().unwrap();
        result.write(directory.path()).unwrap();
        let bytes = fs::read_to_string(
            directory
                .path()
                .join(&result.record.id)
                .join("candidates.jsonl"),
        )
        .unwrap();
        assert_eq!(bytes.lines().count(), 1);
        let case: Value = serde_json::from_str(bytes.trim()).unwrap();
        assert_eq!(case["expected_rows"], json!([1, 2]));
        assert!(result.write(directory.path()).is_err());
    }
    #[tokio::test]
    async fn max_turns_retains_accepted_cases_and_cap_rejects_extra() {
        let model = MockCompletionModel::from_stream_turns([
            turn(
                "submit_case",
                json!({"prompt":"Show Role Analyst rows","sql":sql(),"feature_tag":"role_equality"}),
            ),
            turn(
                "submit_case",
                json!({"prompt":"Select Role Analyst records","sql":sql(),"feature_tag":"role_equality"}),
            ),
        ]);
        let mut settings = Settings::new(1).unwrap();
        settings.max_turns = 2;
        let result = generate(model, dataset(), "feature", settings)
            .await
            .unwrap();
        assert_eq!(result.record.outcome, "max_turns");
        assert_eq!(result.candidates.len(), 1);
        assert!(result.record.rejected_submissions[0].contains("limit"));
    }
    #[tokio::test]
    async fn malformed_typed_submission_does_not_execute() {
        let model = MockCompletionModel::from_stream_turns([
            turn(
                "submit_case",
                json!({"prompt":"Role Analyst","sql":sql(),"feature_tag":"role_equality","expected_rows":[999]}),
            ),
            final_turn(),
        ]);
        let result = generate(model, dataset(), "feature", Settings::new(1).unwrap())
            .await
            .unwrap();
        assert!(result.candidates.is_empty());
        assert_eq!(result.record.outcome, "completed");
    }
    async fn http_fixture(timeout: Option<&str>) -> Generated {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let delay_second = timeout.is_some();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                let header_end = loop {
                    let n = connection.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                assert!(headers.starts_with("POST /v1/chat/completions "));
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                while bytes.len() < header_end + length {
                    let n = connection.read(&mut buffer).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let request: Value =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                assert_eq!(request["stream"], true);
                assert_eq!(request["model"], MODEL);
                let delta = if index == 0 {
                    json!({"role":"assistant","tool_calls":[{"index":0,"id":"submit1","type":"function","function":{"name":"submit_case","arguments":json!({"prompt":"Show Role Analyst rows","sql":sql(),"feature_tag":"role_equality"}).to_string()}}]})
                } else {
                    json!({"role":"assistant","content":"done"})
                };
                let tier = if index == 0 { "flex" } else { "standard" };
                let chunk = json!({"id":format!("chat-{index}"),"object":"chat.completion.chunk","created":1,"model":MODEL,"service_tier":tier,"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                let terminal = json!({"id":format!("chat-{index}"),"object":"chat.completion.chunk","created":1,"model":MODEL,"service_tier":tier,"choices":[{"index":0,"delta":{},"finish_reason":if index==0 {"tool_calls"} else {"stop"}}]});
                let keepalive = ": waiting for flex admission\n\n";
                let body = format!("data: {chunk}\n\ndata: {terminal}\n\ndata: [DONE]\n\n");
                write!(connection,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{keepalive}",keepalive.len()+body.len()).unwrap();
                connection.flush().unwrap();
                if index == 1 && delay_second {
                    requests.push(request);
                    thread::sleep(Duration::from_millis(1500));
                    break;
                }
                thread::sleep(Duration::from_millis(30));
                connection.write_all(body.as_bytes()).unwrap();
                connection.flush().unwrap();
                requests.push(request);
            }
            requests
        });
        let model = OpenAIConfig::new("offline-fixture-secret")
            .with_base_url(format!("http://{address}/v1"))
            .client()
            .chat(MODEL);
        let mut settings = Settings::new(1).unwrap();
        if timeout == Some("per_call_timeout") {
            settings.per_call_seconds = 1;
        }
        if timeout == Some("whole_run_timeout") {
            settings.whole_run_seconds = 1;
        }
        let result = generate(model, dataset(), "feature", settings)
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        result
    }
    #[tokio::test]
    async fn http_chat_stream_keepalive_tool_and_served_tiers() {
        let result = http_fixture(None).await;
        assert_eq!(result.record.outcome, "completed");
        assert_eq!(result.candidates[0].expected_rows, [1, 2]);
        assert_eq!(result.record.calls[0].served_tier, "flex");
        assert_eq!(result.record.calls[1].served_tier, "standard");
        assert!(result.record.calls[1].standard_tier);
        assert!(
            !serde_json::to_string(&result.record)
                .unwrap()
                .contains("offline-fixture-secret")
        );
    }
    #[tokio::test]
    async fn timeouts_keep_submissions_and_record_interrupted_unknown_tier() {
        for outcome in ["per_call_timeout", "whole_run_timeout"] {
            let result = http_fixture(Some(outcome)).await;
            assert_eq!(result.record.outcome, outcome);
            assert_eq!(result.candidates[0].expected_rows, [1, 2]);
            assert_eq!(result.record.calls.len(), 2);
            assert!(result.record.calls[1].interrupted);
            assert_eq!(result.record.calls[1].served_tier, "unknown");
        }
    }
}
impl Generated {
    pub fn write(&self, directory: &Path) -> Result<(), GenerateError> {
        fs::create_dir_all(directory)
            .map_err(|_| GenerateError("cannot create output directory".into()))?;
        let run = directory.join(&self.record.id);
        fs::create_dir(&run)
            .map_err(|_| GenerateError("cannot reserve generation directory".into()))?;
        let mut candidates = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(run.join("candidates.jsonl"))
            .map_err(|_| GenerateError("cannot write candidates".into()))?;
        for case in &self.candidates {
            serde_json::to_writer(&mut candidates, case)
                .map_err(|_| GenerateError("cannot serialize candidate".into()))?;
            writeln!(candidates).map_err(|_| GenerateError("cannot write candidate".into()))?;
        }
        fs::write(
            run.join("generation.json"),
            serde_json::to_vec_pretty(&self.record).unwrap(),
        )
        .map_err(|_| GenerateError("cannot write generation record".into()))?;
        Ok(())
    }
}
