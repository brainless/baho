//! Sanitized read-only tools for the epic planner (Epic 016, T5).
//!
//! The agent never holds a filesystem handle. Every path is normalized, checked against an
//! allowlist and a denylist, canonicalized (so symlinks cannot leave the allowed roots), and
//! rechecked after resolution. Every result is sanitized with the redaction contract,
//! bounded by per-call and whole-run budgets, and scanned before it leaves the harness.

use crate::{
    contracts::EvaluationReport,
    redaction::{RedactionError, Redactor},
};
use rig::tool::{PortableTool, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const PLANNER_PROMPT_VERSION: &str = "epic-planner-v2";
pub const PLANNER_PROMPT: &str = include_str!("../prompts/planner-system.md");
pub const GOALS: &str = include_str!("../goals/goals.md");

pub fn planner_prompt_hash() -> String {
    hex_hash(PLANNER_PROMPT)
}

pub fn goals_hash() -> String {
    hex_hash(GOALS)
}

fn hex_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const MAX_FILE_BYTES: u64 = 1 << 20;
const MAX_READ_LINES: usize = 400;
const DEFAULT_READ_LINES: usize = 200;
const MAX_LINE_CHARS: usize = 500;
const MAX_LIST_ENTRIES: usize = 400;
const MAX_WALK_DEPTH: usize = 12;
const MAX_SEARCH_MATCHES: usize = 40;
const MAX_SNIPPET_CHARS: usize = 200;
const MAX_CASES_PER_PAGE: usize = 25;
const MAX_TARGET_CHARS: usize = 64;

#[derive(Clone, Copy)]
enum Seg {
    Lit(&'static str),
    Any,
    /// One or more remaining components; only valid last.
    Deep,
}
use Seg::{Any, Deep, Lit};

const ALLOWED: &[&[Seg]] = &[
    &[Lit("crates"), Any, Lit("src"), Deep],
    &[Lit("crates"), Any, Lit("tests"), Deep],
    &[Lit("crates"), Any, Lit("Cargo.toml")],
    &[Lit("apps"), Any, Lit("src"), Deep],
    &[Lit("epics"), Deep],
    &[Lit("PRD.md")],
    &[Lit("AGENTS.md")],
    &[Lit("tools"), Lit("synth"), Lit("features"), Deep],
    &[Lit("tools"), Lit("synth"), Lit("prompts"), Deep],
    &[Lit("tools"), Lit("synth"), Lit("goals"), Deep],
];
const DENIED_NAMES: &[&str] = &[".baho", ".datasets", "target", ".git"];

pub(crate) fn is_denied(comps: &[String]) -> bool {
    comps
        .iter()
        .any(|c| c.starts_with(".env") || DENIED_NAMES.contains(&c.as_str()))
        || (comps.len() >= 2 && comps[0] == "synthetic" && comps[1] == "out")
}

fn pattern_matches_file(pattern: &[Seg], comps: &[String]) -> bool {
    let mut i = 0;
    for seg in pattern {
        match seg {
            Lit(l) => {
                if comps.get(i).map(String::as_str) != Some(*l) {
                    return false;
                }
                i += 1;
            }
            Any => {
                if comps.get(i).is_none() {
                    return false;
                }
                i += 1;
            }
            Deep => return comps.len() > i,
        }
    }
    comps.len() == i
}

/// True when `comps` is a directory that is, or lies inside, an allowed region.
fn pattern_may_contain(pattern: &[Seg], comps: &[String]) -> bool {
    for (i, seg) in pattern.iter().enumerate() {
        match seg {
            Deep => return true,
            _ if i >= comps.len() => return true,
            Lit(l) if comps[i] != *l => return false,
            _ => {}
        }
    }
    false
}

pub(crate) fn allowed_file(comps: &[String]) -> bool {
    !is_denied(comps) && ALLOWED.iter().any(|p| pattern_matches_file(p, comps))
}

fn allowed_dir(comps: &[String]) -> bool {
    !is_denied(comps) && ALLOWED.iter().any(|p| pattern_may_contain(p, comps))
}

/// Rejects traversal, absolute, and platform-specific forms before touching the filesystem.
pub(crate) fn normalize(raw: &str) -> Result<Vec<String>, &'static str> {
    let bad = raw.len() > 256
        || raw.contains(['\0', '\\'])
        || raw.starts_with(['/', '~'])
        || raw.as_bytes().get(1) == Some(&b':');
    if bad {
        return Err("path not allowed");
    }
    let mut out = Vec::new();
    for c in raw.split('/') {
        match c {
            "" | "." => {}
            ".." => return Err("path not allowed"),
            c => out.push(c.to_owned()),
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ToolLimits {
    pub max_calls: usize,
    pub per_call_bytes: usize,
    pub total_bytes: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_calls: 60,
            per_call_bytes: 24 * 1024,
            total_bytes: 384 * 1024,
        }
    }
}

/// One bounded transcript row: no file contents, only the sanitized target and sizes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolTranscript {
    pub tool: String,
    pub target: String,
    pub bytes: usize,
    pub outcome: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

#[derive(Debug, thiserror::Error)]
pub enum ToolboxError {
    #[error("repository root is not usable")]
    Root,
    #[error("{0}")]
    Redaction(#[from] RedactionError),
}

#[derive(Default)]
struct Usage {
    calls: usize,
    bytes: usize,
    transcript: Vec<ToolTranscript>,
}

struct Inner {
    root: PathBuf,
    redactor: Redactor,
    report: EvaluationReport,
    limits: ToolLimits,
    usage: Mutex<Usage>,
}

/// Shared state for the four read tools: one budget and one transcript per run.
#[derive(Clone)]
pub struct Toolbox(Arc<Inner>);

impl Toolbox {
    /// `report` is sanitized again with `redactor`, so an unsanitized report cannot leak.
    pub fn new(
        repo_root: &Path,
        report: &EvaluationReport,
        redactor: Redactor,
        limits: ToolLimits,
    ) -> Result<Self, ToolboxError> {
        let root = repo_root.canonicalize().map_err(|_| ToolboxError::Root)?;
        let report = redactor.sanitize_report(report)?;
        Ok(Self(Arc::new(Inner {
            root,
            redactor,
            report,
            limits,
            usage: Mutex::new(Usage::default()),
        })))
    }

    pub fn transcript(&self) -> Vec<ToolTranscript> {
        self.0.usage.lock().unwrap().transcript.clone()
    }

    pub fn used_bytes(&self) -> usize {
        self.0.usage.lock().unwrap().bytes
    }

    fn run(
        &self,
        tool: &'static str,
        raw_target: &str,
        f: impl FnOnce(&Inner) -> Result<Value, String>,
    ) -> Result<Value, ToolError> {
        let inner = &*self.0;
        let target = inner.target_label(raw_target);
        let record = |bytes: usize, outcome: &str| {
            inner.usage.lock().unwrap().transcript.push(ToolTranscript {
                tool: tool.into(),
                target: target.clone(),
                bytes,
                outcome: outcome.into(),
            });
        };
        {
            let mut usage = inner.usage.lock().unwrap();
            if usage.calls >= inner.limits.max_calls {
                drop(usage);
                record(0, "call_budget_exhausted");
                return Err(ToolError("tool call budget exhausted".into()));
            }
            usage.calls += 1;
        }
        let fail = |outcome: &str, message: &str| {
            record(0, outcome);
            Err(ToolError(inner.redactor.sanitize(message)))
        };
        let value = match f(inner) {
            Ok(v) => v,
            Err(m) => return fail("rejected", &m),
        };
        let mut leaves = Vec::new();
        collect_strings(&value, &mut leaves);
        if leaves.iter().any(|s| !inner.redactor.scan(s).is_empty()) {
            return fail(
                "withheld",
                "response withheld: it could not be sanitized safely",
            );
        }
        let len = serde_json::to_string(&value).map_or(usize::MAX, |s| s.len());
        if len > inner.limits.per_call_bytes {
            return fail(
                "per_call_budget",
                "result exceeds the per-call byte budget; narrow the request",
            );
        }
        {
            let mut usage = inner.usage.lock().unwrap();
            if usage.bytes.saturating_add(len) > inner.limits.total_bytes {
                drop(usage);
                return fail("byte_budget_exhausted", "run byte budget exhausted");
            }
            usage.bytes += len;
        }
        record(len, "ok");
        Ok(value)
    }
}

fn collect_strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(m) => m.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_owned(),
    }
}

impl Inner {
    /// A transcript label that can never carry a source literal or secret.
    fn target_label(&self, raw: &str) -> String {
        let clean = self.redactor.sanitize(raw);
        if !self.redactor.scan(&clean).is_empty() {
            return "[withheld]".into();
        }
        truncate_chars(&clean, MAX_TARGET_CHARS)
    }

    fn clean(&self, text: &str) -> String {
        self.redactor.sanitize(text)
    }

    /// Resolves a repository-relative path to a canonical one that is still allowlisted.
    fn resolve(&self, raw: &str, kind: Kind) -> Result<(PathBuf, Vec<String>), &'static str> {
        let comps = normalize(raw)?;
        let ok = |c: &[String]| match kind {
            Kind::File => allowed_file(c),
            Kind::Dir => c.is_empty() || allowed_dir(c),
        };
        if !ok(&comps) {
            return Err("path not allowed");
        }
        let full =
            self.canonical_checked(&self.root.join(comps.iter().collect::<PathBuf>()), kind)?;
        Ok(full)
    }

    /// Canonicalizes and rechecks policy on the result, which defeats symlink escapes.
    fn canonical_checked(
        &self,
        joined: &Path,
        kind: Kind,
    ) -> Result<(PathBuf, Vec<String>), &'static str> {
        let canon = joined.canonicalize().map_err(|_| "path not found")?;
        let rel = canon
            .strip_prefix(&self.root)
            .map_err(|_| "path not allowed")?;
        let comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let ok = match kind {
            Kind::File => allowed_file(&comps),
            Kind::Dir => comps.is_empty() || allowed_dir(&comps),
        };
        let meta = fs::metadata(&canon).map_err(|_| "path not found")?;
        if !ok || (kind == Kind::File) != meta.is_file() {
            return Err("path not allowed");
        }
        Ok((canon, comps))
    }

    /// Allowed files under `dir` (canonical), deterministic order. Symlinked directories are
    /// never followed, and symlinked files are not listed (they stay readable by direct path).
    fn walk(&self, dir: &Path, depth: usize, cap: usize, out: &mut Vec<(PathBuf, String)>) {
        if depth > MAX_WALK_DEPTH || out.len() >= cap {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if out.len() >= cap {
                return;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                let Ok(rel) = path.strip_prefix(&self.root) else {
                    continue;
                };
                let comps: Vec<String> = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                if allowed_dir(&comps) {
                    self.walk(&path, depth + 1, cap, out);
                }
            } else if ft.is_file()
                && let Ok((canon, comps)) = self.canonical_checked(&path, Kind::File)
            {
                out.push((canon, comps.join("/")));
            }
        }
    }

    fn read_text(&self, canon: &Path) -> Result<String, &'static str> {
        let meta = fs::metadata(canon).map_err(|_| "path not found")?;
        if meta.len() > MAX_FILE_BYTES {
            return Err("file too large");
        }
        let bytes = fs::read(canon).map_err(|_| "path not found")?;
        String::from_utf8(bytes).map_err(|_| "file is not UTF-8 text")
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn harness_error(error: ToolError) -> ToolExecutionError {
    ToolExecutionError::other(error.to_string()).with_code("harness_rejection")
}

#[derive(Clone)]
pub struct ReadEvaluation(pub Toolbox);
#[derive(Clone)]
pub struct ListFiles(pub Toolbox);
#[derive(Clone)]
pub struct ReadFile(pub Toolbox);
#[derive(Clone)]
pub struct SearchText(pub Toolbox);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadEvaluationArgs {
    #[serde(default)]
    pub section: EvaluationSection,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationSection {
    #[default]
    Summary,
    Cases,
}

impl PortableTool for ReadEvaluation {
    const NAME: &'static str = "read_evaluation";
    type Args = ReadEvaluationArgs;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, error: ToolError) -> ToolExecutionError {
        harness_error(error)
    }
    fn description(&self) -> String {
        "Read the sanitized evaluation: section 'summary' (IDs, aggregates, limitations, notes) \
         or 'cases' (a page of per-case results with stable case IDs)."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "section":{"type":"string","enum":["summary","cases"]},
            "offset":{"type":"integer","minimum":0},
            "limit":{"type":"integer","minimum":1,"maximum":MAX_CASES_PER_PAGE}
        },"additionalProperties":false})
    }
    async fn call(&self, args: ReadEvaluationArgs) -> Result<Value, ToolError> {
        let label = match args.section {
            EvaluationSection::Summary => "summary".to_owned(),
            EvaluationSection::Cases => format!("cases@{}", args.offset),
        };
        self.0.run(Self::NAME, &label, |inner| {
            let r = &inner.report;
            match args.section {
                EvaluationSection::Summary => Ok(json!({
                    "evaluation_id": r.evaluation_id,
                    "generation_id": r.generation_id,
                    "generation_hash": r.generation_hash,
                    "candidates_hash": r.candidates_hash,
                    "dataset_hash": r.dataset_hash,
                    "provenance": {
                        "repo_revision": r.provenance.repo_revision,
                        "repo_dirty": r.provenance.repo_dirty,
                        "binary_version": r.provenance.binary_version,
                    },
                    "aggregates": r.aggregates,
                    "limitations": r.limitations,
                    "notes": r.notes,
                    "case_count": r.cases.len(),
                })),
                EvaluationSection::Cases => {
                    let limit = args.limit.unwrap_or(10).clamp(1, MAX_CASES_PER_PAGE);
                    let page: Vec<&_> = r.cases.iter().skip(args.offset).take(limit).collect();
                    let next = args.offset + page.len();
                    Ok(json!({
                        "evaluation_id": r.evaluation_id,
                        "total": r.cases.len(),
                        "offset": args.offset,
                        "next_offset": (next < r.cases.len()).then_some(next),
                        "cases": page,
                    }))
                }
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListFilesArgs {
    pub dir: Option<String>,
}

impl PortableTool for ListFiles {
    const NAME: &'static str = "list_files";
    type Args = ListFilesArgs;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, error: ToolError) -> ToolExecutionError {
        harness_error(error)
    }
    fn description(&self) -> String {
        "List readable repository files under an optional repository-relative directory.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"dir":{"type":"string","maxLength":256}},"additionalProperties":false})
    }
    async fn call(&self, args: ListFilesArgs) -> Result<Value, ToolError> {
        let raw = args.dir.unwrap_or_default();
        self.0.run(Self::NAME, &raw, |inner| {
            let (canon, _) = inner.resolve(&raw, Kind::Dir).map_err(String::from)?;
            let mut files = Vec::new();
            inner.walk(&canon, 0, MAX_LIST_ENTRIES + 1, &mut files);
            let truncated = files.len() > MAX_LIST_ENTRIES;
            files.truncate(MAX_LIST_ENTRIES);
            let paths: Vec<String> = files.iter().map(|(_, rel)| inner.clean(rel)).collect();
            Ok(json!({"files": paths, "truncated": truncated}))
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileArgs {
    pub path: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

impl PortableTool for ReadFile {
    const NAME: &'static str = "read_file";
    type Args = ReadFileArgs;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, error: ToolError) -> ToolExecutionError {
        harness_error(error)
    }
    fn description(&self) -> String {
        format!(
            "Read lines of one allowlisted repository file (1-based, inclusive, at most \
             {MAX_READ_LINES} lines per call). Lines keep their original numbers."
        )
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "path":{"type":"string","maxLength":256},
            "start_line":{"type":"integer","minimum":1},
            "end_line":{"type":"integer","minimum":1}
        },"required":["path"],"additionalProperties":false})
    }
    async fn call(&self, args: ReadFileArgs) -> Result<Value, ToolError> {
        let budget = self.0.0.limits.per_call_bytes;
        self.0.run(Self::NAME, &args.path, |inner| {
            let (canon, comps) = inner
                .resolve(&args.path, Kind::File)
                .map_err(String::from)?;
            let text = inner.read_text(&canon).map_err(String::from)?;
            let all: Vec<&str> = text.lines().collect();
            let start = args.start_line.unwrap_or(1).max(1);
            if start > all.len().max(1) {
                return Err("start_line is beyond the end of the file".into());
            }
            let wanted_end = args.end_line.unwrap_or(start + DEFAULT_READ_LINES - 1);
            let end = wanted_end
                .min(start + MAX_READ_LINES - 1)
                .min(all.len())
                .max(start.min(all.len()));
            let mut lines = Vec::new();
            let mut used = 0;
            let mut last = start.saturating_sub(1);
            for n in start..=end {
                let line = truncate_chars(&inner.clean(all[n - 1]), MAX_LINE_CHARS);
                let rendered = format!("{n}: {line}");
                used += rendered.len() + 4;
                // Leave headroom for the envelope so the call budget is not tripped by us.
                if used + 512 > budget {
                    break;
                }
                lines.push(rendered);
                last = n;
            }
            Ok(json!({
                "path": inner.clean(&comps.join("/")),
                "start_line": start,
                "end_line": last,
                "total_lines": all.len(),
                "truncated": last < wanted_end.min(all.len()),
                "lines": lines,
            }))
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchTextArgs {
    pub literal: String,
    pub scope: Option<String>,
}

impl PortableTool for SearchText {
    const NAME: &'static str = "search_text";
    type Args = SearchTextArgs;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, error: ToolError) -> ToolExecutionError {
        harness_error(error)
    }
    fn description(&self) -> String {
        format!(
            "Case-sensitive literal search over the sanitized view of allowlisted files, \
             optionally within a repository-relative file or directory scope. Returns at most \
             {MAX_SEARCH_MATCHES} matches with path, line number, and a short snippet."
        )
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "literal":{"type":"string","minLength":2,"maxLength":128},
            "scope":{"type":"string","maxLength":256}
        },"required":["literal"],"additionalProperties":false})
    }
    async fn call(&self, args: SearchTextArgs) -> Result<Value, ToolError> {
        self.0.run(Self::NAME, &args.literal, |inner| {
            let needle = args.literal.as_str();
            let len = needle.chars().count();
            if !(2..=128).contains(&len) || needle.chars().any(char::is_control) {
                return Err("search literal must be 2 to 128 printable characters".into());
            }
            if !inner.redactor.scan(needle).is_empty() {
                return Err("search literal rejected".into());
            }
            let scope = args.scope.as_deref().unwrap_or("");
            let mut files = Vec::new();
            match inner.resolve(scope, Kind::File) {
                Ok((canon, comps)) => files.push((canon, comps.join("/"))),
                Err(_) => {
                    let (dir, _) = inner.resolve(scope, Kind::Dir).map_err(String::from)?;
                    inner.walk(&dir, 0, 2000, &mut files);
                }
            }
            let mut matches = Vec::new();
            let mut truncated = false;
            'files: for (canon, rel) in &files {
                let Ok(text) = inner.read_text(canon) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    let clean = inner.clean(line);
                    if clean.contains(needle) {
                        if matches.len() >= MAX_SEARCH_MATCHES {
                            truncated = true;
                            break 'files;
                        }
                        matches.push(json!({
                            "path": inner.clean(rel),
                            "line": i + 1,
                            "snippet": truncate_chars(clean.trim(), MAX_SNIPPET_CHARS),
                        }));
                    }
                }
            }
            Ok(json!({"matches": matches, "truncated": truncated}))
        })
    }
}

pub const MAX_EXPLANATION_BYTES: usize = 1000;
pub const MAX_EXCERPT_BYTES: usize = 300;

/// Sanitized text capped at `max` bytes on a character boundary. Text that still trips the
/// redactor after sanitizing is withheld rather than partially shown.
pub fn bounded_text(redactor: &Redactor, text: &str, max: usize) -> String {
    let clean = redactor.sanitize(text.trim());
    if !redactor.scan(&clean).is_empty() {
        return "[withheld: the text could not be sanitized safely]".into();
    }
    let mut end = clean.len().min(max);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    clean[..end].to_owned()
}

/// True when model text looks like a tool call written as prose (DeepSeek DSML, XML-style
/// `<tool_call>`, `invoke name=`, `function_call`). Deliberately small: a false positive only
/// changes the outcome label from `no_terminal_action` to `malformed_tool_call`.
pub fn looks_like_tool_call_markup(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "dsml",
        "invoke name=",
        "<tool_call",
        "<function_call",
        "function_call",
    ];
    let lower = text.to_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// `finish_no_epic(explanation)`: the only deliberate way to end a run without an epic.
#[derive(Clone)]
pub struct FinishNoEpic {
    redactor: Redactor,
    explanation: Arc<Mutex<Option<String>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishNoEpicArgs {
    pub explanation: String,
}

impl FinishNoEpic {
    pub fn new(redactor: Redactor) -> Self {
        Self {
            redactor,
            explanation: Arc::default(),
        }
    }

    /// The sanitized, bounded explanation once the agent has finished deliberately.
    pub fn explanation(&self) -> Option<String> {
        self.explanation.lock().unwrap().clone()
    }
}

impl PortableTool for FinishNoEpic {
    const NAME: &'static str = "finish_no_epic";
    type Args = FinishNoEpicArgs;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, error: ToolError) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string())
    }
    fn description(&self) -> String {
        "Finish the run without an epic because the evidence is insufficient. Give a brief \
         explanation (at most 1000 bytes). Call this instead of writing a final answer."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "explanation":{"type":"string","maxLength":MAX_EXPLANATION_BYTES}
        },"required":["explanation"],"additionalProperties":false})
    }
    async fn call(&self, args: FinishNoEpicArgs) -> Result<Value, ToolError> {
        let mut clean = bounded_text(&self.redactor, &args.explanation, MAX_EXPLANATION_BYTES);
        if clean.is_empty() {
            clean = "[no explanation given]".into();
        }
        let mut slot = self.explanation.lock().unwrap();
        slot.get_or_insert(clean);
        Ok(json!({"finished": true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    const CRED: &str = "sk-abcdefghijklmnopqrstuvwxyz0123456789";

    #[test]
    fn detects_tool_call_markup_only() {
        for yes in [
            "x <｜DSML｜ invoke name=\"read_file\">",
            "<tool_call>{}",
            "Let me call INVOKE NAME=\"a\"",
            "{\"function_call\": 1}",
        ] {
            assert!(looks_like_tool_call_markup(yes), "{yes}");
        }
        for no in ["Evidence is thin.", "I will read the file next.", ""] {
            assert!(!looks_like_tool_call_markup(no), "{no}");
        }
    }

    #[test]
    fn bounded_text_caps_bytes_on_char_boundary_and_redacts() {
        let r = redactor();
        let out = bounded_text(&r, &format!("Senior Analyst {}", "é".repeat(400)), 300);
        assert!(out.len() <= 300);
        assert!(!out.contains("Senior Analyst"));
    }

    fn redactor() -> Redactor {
        Redactor::new(["Senior Analyst"]).unwrap()
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn report(n: usize) -> EvaluationReport {
        let case = |i: usize| CaseReport {
            case_id: format!("generation-1791425654630970000-3920-{i:04}"),
            candidate_hash: "ab".repeat(32),
            prompt: "Keep Senior Analyst".into(),
            review: ReviewStatus::Confirmed,
            review_rationale: None,
            observed: ObservedResult::Refused,
            plan_comparison: PlanComparison::Unavailable,
            diagnostic_codes: vec!["intent.column_not_found".into()],
            row_counts: None,
            evidence: CaseEvidence::default(),
        };
        EvaluationReport {
            schema_version: REPORT_SCHEMA_VERSION,
            evaluation_id: "evaluation-1791425654630970000-3920-0".into(),
            generation_id: "generation-1791425654630970000-3920".into(),
            generation_hash: "g".into(),
            candidates_hash: "c".into(),
            metadata_hash: "m".into(),
            dataset_hash: "d".into(),
            provenance: ToolProvenance::default(),
            aggregates: Aggregates::default(),
            limitations: vec![],
            notes: vec![],
            cases: (0..n).map(case).collect(),
        }
    }

    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        outside: PathBuf,
        tools: Toolbox,
    }

    fn fixture_with(limits: ToolLimits) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "outside secret\n").unwrap();
        write(
            &root,
            "crates/a/src/lib.rs",
            &format!(
                "fn one() {{}}\n// senior analyst role\nlet k = \"{CRED}\";\nlet p = \"/Users/me/x\";\nfn four() {{}}\n"
            ),
        );
        write(&root, "crates/a/tests/t.rs", "// test\n");
        write(&root, "crates/a/Cargo.toml", "[package]\n");
        write(&root, "crates/a/other.rs", "not allowed\n");
        write(&root, "epics/001-x.md", "# Epic\nhello\n");
        write(&root, "PRD.md", "prd\n");
        write(&root, "Cargo.toml", "[workspace]\n");
        write(&root, ".env", "KEY=1\n");
        write(&root, ".git/config", "x\n");
        write(&root, "target/debug/x.rs", "x\n");
        write(&root, ".datasets/d.csv", "Senior Analyst\n");
        write(&root, ".baho/runs/1/manifest.json", "{}\n");
        write(&root, "synthetic/out/x.txt", "x\n");
        write(&root, "tools/synth/goals/goals.md", "goals\n");
        write(&root, "crates/a/src/.env.local", "S=1\n");
        symlink(&outside, root.join("crates/a/src/escape.rs")).unwrap();
        symlink(root.join(".env"), root.join("crates/a/src/env_link.rs")).unwrap();
        symlink(root.join("PRD.md"), root.join("crates/a/src/prd_link.rs")).unwrap();
        symlink(dir.path(), root.join("epics/dirlink")).unwrap();
        let tools = Toolbox::new(&root, &report(30), redactor(), limits).unwrap();
        Fixture {
            _dir: dir,
            root,
            outside,
            tools,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(ToolLimits::default())
    }

    async fn read(
        f: &Fixture,
        path: &str,
        s: Option<usize>,
        e: Option<usize>,
    ) -> Result<Value, ToolError> {
        ReadFile(f.tools.clone())
            .call(ReadFileArgs {
                path: path.into(),
                start_line: s,
                end_line: e,
            })
            .await
    }

    #[tokio::test]
    async fn reads_allowlisted_files_with_original_line_numbers() {
        let f = fixture();
        let v = read(&f, "crates/a/src/lib.rs", Some(4), Some(5))
            .await
            .unwrap();
        assert_eq!(v["start_line"], 4);
        assert_eq!(v["end_line"], 5);
        assert_eq!(v["total_lines"], 5);
        assert!(v["lines"][1].as_str().unwrap().starts_with("5: fn four"));
        for ok in [
            "crates/a/tests/t.rs",
            "crates/a/Cargo.toml",
            "epics/001-x.md",
            "PRD.md",
        ] {
            read(&f, ok, None, None)
                .await
                .unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
    }

    #[tokio::test]
    async fn rejects_denied_unlisted_and_traversal_paths() {
        let f = fixture();
        let abs = f.outside.to_string_lossy().into_owned();
        for bad in [
            ".env",
            "crates/a/src/.env.local",
            ".git/config",
            "target/debug/x.rs",
            ".datasets/d.csv",
            ".baho/runs/1/manifest.json",
            "synthetic/out/x.txt",
            "Cargo.toml",
            "crates/a/other.rs",
            "../outside.txt",
            "crates/a/src/../../../outside.txt",
            "epics/../.env",
            "/etc/passwd",
            "~/x",
            "C:/x",
            "crates\\a\\src\\lib.rs",
            "crates/a/src",
            "",
            &abs,
        ] {
            assert!(
                read(&f, bad, None, None).await.is_err(),
                "{bad:?} was readable"
            );
        }
    }

    #[tokio::test]
    async fn symlinks_cannot_escape_or_reach_denied_files() {
        let f = fixture();
        for link in [
            "crates/a/src/escape.rs",
            "crates/a/src/env_link.rs",
            "epics/dirlink/outside.txt",
        ] {
            let err = read(&f, link, None, None).await.unwrap_err().to_string();
            assert!(!err.contains("secret"), "{err}");
            assert!(read(&f, link, None, None).await.is_err(), "{link}");
        }
        // A symlink to another allowed file resolves to its canonical path.
        let v = read(&f, "crates/a/src/prd_link.rs", None, None)
            .await
            .unwrap();
        assert_eq!(v["path"], "PRD.md");
        let listed = ListFiles(f.tools.clone())
            .call(ListFilesArgs {
                dir: Some("epics".into()),
            })
            .await
            .unwrap();
        assert_eq!(listed["files"], json!(["epics/001-x.md"]));
    }

    #[tokio::test]
    async fn file_content_is_sanitized_but_numbering_is_preserved() {
        let f = fixture();
        let v = read(&f, "crates/a/src/lib.rs", None, None).await.unwrap();
        let text = v["lines"].to_string().to_lowercase();
        assert!(!text.contains("senior analyst"), "{text}");
        assert!(!text.contains("sk-abc"), "{text}");
        assert!(!text.contains("/users/me"), "{text}");
        assert!(text.contains("[secret]") && text.contains("[path]"));
        assert!(v["lines"][1].as_str().unwrap().starts_with("2: // "), "{v}");
    }

    #[tokio::test]
    async fn list_files_shows_only_allowed_files() {
        let f = fixture();
        let v = ListFiles(f.tools.clone())
            .call(ListFilesArgs { dir: None })
            .await
            .unwrap();
        let files: Vec<&str> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(
            files,
            [
                "AGENTS.md",
                "PRD.md",
                "crates/a/Cargo.toml",
                "crates/a/src/lib.rs",
                "crates/a/tests/t.rs",
                "epics/001-x.md",
                "tools/synth/goals/goals.md",
            ]
            .iter()
            .filter(|p| **p != "AGENTS.md")
            .copied()
            .collect::<Vec<_>>()
        );
        for denied in [
            ".git",
            "target",
            ".datasets",
            ".baho",
            "synthetic/out",
            "crates/a",
        ] {
            let r = ListFiles(f.tools.clone())
                .call(ListFilesArgs {
                    dir: Some(denied.into()),
                })
                .await;
            if denied == "crates/a" {
                let files = r.unwrap()["files"].to_string();
                assert!(!files.contains("other.rs"), "{files}");
            } else {
                assert!(r.is_err(), "{denied}");
            }
        }
    }

    #[tokio::test]
    async fn search_runs_over_the_sanitized_view_only() {
        let f = fixture();
        let tools = f.tools.clone();
        let search = |lit: &str, scope: Option<&str>| {
            let tool = SearchText(tools.clone());
            let args = SearchTextArgs {
                literal: lit.into(),
                scope: scope.map(Into::into),
            };
            async move { tool.call(args).await }
        };
        let hit = search("fn four", None).await.unwrap();
        assert_eq!(hit["matches"][0]["line"], 5);
        assert_eq!(hit["matches"][0]["path"], "crates/a/src/lib.rs");
        // The real literal is refused rather than searched; its stand-in is searchable.
        assert!(search("senior analyst", None).await.is_err());
        let stand_in = redactor().sanitize("senior analyst");
        let hit = search(&stand_in, Some("crates/a/src")).await.unwrap();
        assert_eq!(hit["matches"][0]["line"], 2);
        assert!(!hit.to_string().to_lowercase().contains("senior analyst"));
        assert!(search("x", None).await.is_err());
        assert!(
            search("outside", None).await.unwrap()["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(search("fn four", Some("../")).await.is_err());
        assert!(search("fn four", Some(".git")).await.is_err());
    }

    #[tokio::test]
    async fn read_evaluation_pages_cases_and_exempts_harness_ids() {
        let f = fixture();
        let tool = ReadEvaluation(f.tools.clone());
        let s = tool
            .call(ReadEvaluationArgs {
                section: EvaluationSection::Summary,
                offset: 0,
                limit: None,
            })
            .await
            .unwrap();
        assert_eq!(s["case_count"], 30);
        assert_eq!(s["generation_id"], "generation-1791425654630970000-3920");
        let p = tool
            .call(ReadEvaluationArgs {
                section: EvaluationSection::Cases,
                offset: 10,
                limit: Some(5),
            })
            .await
            .unwrap();
        assert_eq!(p["cases"].as_array().unwrap().len(), 5);
        assert_eq!(p["next_offset"], 15);
        assert_eq!(
            p["cases"][0]["case_id"],
            "generation-1791425654630970000-3920-0010"
        );
        assert!(!p.to_string().to_lowercase().contains("senior analyst"));
        // Oversized pages fail the per-call budget instead of being cut off mid-record.
        let big = fixture_with(ToolLimits {
            per_call_bytes: 600,
            ..ToolLimits::default()
        });
        assert!(
            ReadEvaluation(big.tools.clone())
                .call(ReadEvaluationArgs {
                    section: EvaluationSection::Cases,
                    offset: 0,
                    limit: Some(25)
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn credential_in_unsanitized_report_cannot_reach_the_model() {
        let f = fixture();
        let mut r = report(1);
        r.notes.push(format!("token={CRED}"));
        let t = Toolbox::new(&f.root, &r, redactor(), ToolLimits::default()).unwrap();
        let s = ReadEvaluation(t)
            .call(ReadEvaluationArgs {
                section: EvaluationSection::Summary,
                offset: 0,
                limit: None,
            })
            .await
            .unwrap();
        assert!(!s.to_string().contains("sk-abc"));
    }

    #[tokio::test]
    async fn call_and_byte_budgets_hold_and_transcript_has_no_contents() {
        let f = fixture_with(ToolLimits {
            max_calls: 3,
            ..ToolLimits::default()
        });
        for _ in 0..3 {
            read(&f, "PRD.md", None, None).await.unwrap();
        }
        let err = read(&f, "PRD.md", None, None).await.unwrap_err();
        assert!(err.to_string().contains("budget"));

        let f = fixture_with(ToolLimits {
            total_bytes: 300,
            ..ToolLimits::default()
        });
        let mut failed = false;
        for _ in 0..5 {
            failed |= read(&f, "PRD.md", None, None).await.is_err();
        }
        assert!(failed && f.tools.used_bytes() <= 300);
        let t = f.tools.transcript();
        assert_eq!(t[0].outcome, "ok");
        assert_eq!(t.last().unwrap().outcome, "byte_budget_exhausted");
        assert!(!serde_json::to_string(&t).unwrap().contains("prd\\n"));
    }

    #[tokio::test]
    async fn long_reads_stop_inside_the_per_call_budget() {
        let f = fixture_with(ToolLimits {
            per_call_bytes: 2048,
            ..ToolLimits::default()
        });
        let body: String = (0..300)
            .map(|i| format!("line {i} {}\n", "x".repeat(60)))
            .collect();
        write(&f.root, "epics/002-long.md", &body);
        let v = read(&f, "epics/002-long.md", Some(1), Some(300))
            .await
            .unwrap();
        assert_eq!(v["truncated"], true);
        assert!(v["end_line"].as_u64().unwrap() < 300);
        assert_eq!(v["lines"][0].as_str().unwrap().split(':').next(), Some("1"));
    }

    #[test]
    fn goals_and_prompt_ship_with_the_tooling() {
        assert!(GOALS.contains("Parse → Plan → Execute"));
        assert!(GOALS.contains("Product invariants hold"));
        assert!(PLANNER_PROMPT.contains("exactly one"));
        assert_eq!(planner_prompt_hash().len(), 64);
        assert_ne!(planner_prompt_hash(), goals_hash());
    }
}
