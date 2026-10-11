//! Bounded structural source navigation for repair agents (Epic 019).
//!
//! `outline` gives a compact map with line spans, `read_lines` serves small
//! line ranges, and `search_text` finds lines with their enclosing item. No
//! response exceeds its cap regardless of file size, and every byte of
//! source-derived text counts against a per-stage budget.
//!
//! `syn` spans never leave [`parse_items`]: proc-macro2 keeps a thread-local
//! source map, so parsing runs on a short-lived scoped thread and returns plain
//! line data only.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rig::tool::{PortableTool, ToolErrorKind, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use syn::spanned::Spanned;

use crate::repair::AttemptSettings;

const MAX_RANGES: usize = 4;
const MAX_QUERY_BYTES: usize = 100;
const MAX_OUTLINE_LINE_CHARS: usize = 120;
const MAX_EXCERPT_CHARS: usize = 160;
const MAX_PARSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SEARCH_FILES: usize = 5_000;
const MAX_SEARCH_LINE_BYTES: u64 = 64 * 1024;
const MAX_PATH_BYTES: usize = 256;

/// Stable error codes returned to the model and recorded in the trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorCode {
    PathNotAllowed,
    NotFound,
    NotAFile,
    InvalidRange,
    QueryInvalid,
    /// The stage's tool-call budget is spent.
    BudgetExhausted,
    /// The stage's exposed-source byte budget is spent.
    SessionBytesExhausted,
    /// `syn` could not parse the file. Outline falls back instead of failing,
    /// so this is reported in the response and the trace, not as an `Err`.
    ParseFailed,
}

impl ToolErrorCode {
    pub const ALL: [Self; 8] = [
        Self::PathNotAllowed,
        Self::NotFound,
        Self::NotAFile,
        Self::InvalidRange,
        Self::QueryInvalid,
        Self::BudgetExhausted,
        Self::SessionBytesExhausted,
        Self::ParseFailed,
    ];

    /// The code a tool error text starts with, as rendered by
    /// [`SourceToolError`]'s `Display`; trace hooks only see that text.
    pub fn find_in(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .filter_map(|c| text.find(c.as_str()).map(|at| (at, c)))
            .min_by_key(|(at, _)| *at)
            .map(|(_, c)| c)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PathNotAllowed => "path_not_allowed",
            Self::NotFound => "not_found",
            Self::NotAFile => "not_a_file",
            Self::InvalidRange => "invalid_range",
            Self::QueryInvalid => "query_invalid",
            Self::BudgetExhausted => "budget_exhausted",
            Self::SessionBytesExhausted => "session_bytes_exhausted",
            Self::ParseFailed => "parse_failed",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{}: {message}", code.as_str())]
pub struct SourceToolError {
    pub code: ToolErrorCode,
    pub message: String,
}

fn err(code: ToolErrorCode, message: impl Into<String>) -> SourceToolError {
    SourceToolError {
        code,
        message: message.into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLimits {
    pub max_calls: u32,
    pub read_bytes: usize,
    pub outline_bytes: usize,
    pub search_matches: usize,
    pub stage_bytes: usize,
}

impl SourceLimits {
    pub fn from_settings(settings: &AttemptSettings) -> Self {
        Self {
            max_calls: settings.max_tool_calls,
            read_bytes: settings.max_read_bytes,
            outline_bytes: settings.max_outline_bytes,
            search_matches: settings.max_search_matches,
            stage_bytes: settings.max_exposed_bytes,
        }
    }
}

/// A span of file text that reached the model, pinned to the file's hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exposed {
    pub path: String,
    pub file_sha256: String,
    pub start_line: usize,
    pub end_line: usize,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutlineKind {
    Syn,
    Fallback,
}

#[derive(Debug, Serialize)]
pub struct OutlineResponse {
    pub path: String,
    pub file_sha256: String,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub outline_kind: OutlineKind,
    /// `parse_failed` when the fallback outline was used.
    pub code: Option<ToolErrorCode>,
    pub text: String,
    pub truncated: bool,
    pub next_line: Option<usize>,
    pub bytes: usize,
    pub exposed: Vec<Exposed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeStatus {
    Served,
    Partial,
    NotServed,
}

#[derive(Debug, Serialize)]
pub struct RangeResult {
    pub start: usize,
    /// Requested end clamped to the file length.
    pub end: usize,
    pub status: RangeStatus,
    pub text: String,
    pub served_end: Option<usize>,
    pub next_line: Option<usize>,
    pub line_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadLinesResponse {
    pub path: String,
    pub file_sha256: String,
    pub total_lines: usize,
    pub ranges: Vec<RangeResult>,
    pub truncated: bool,
    pub bytes: usize,
    pub exposed: Vec<Exposed>,
}

#[derive(Debug, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub excerpt: String,
    /// Innermost enclosing outline item, e.g. `fn name`, with its 1-based span.
    pub item: Option<String>,
    pub item_start: Option<usize>,
    pub item_end: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub matches: Vec<SearchMatch>,
    pub files_searched: usize,
    pub files_skipped: usize,
    pub truncated: bool,
    pub bytes: usize,
    pub exposed: Vec<Exposed>,
}

#[derive(Clone, Debug)]
struct Item {
    depth: usize,
    start: usize,
    end: usize,
    label: String,
    text: String,
}

struct Parsed {
    sha256: String,
    total_lines: usize,
    total_bytes: usize,
    kind: OutlineKind,
    items: Vec<Item>,
    /// Line start byte offsets into `source`.
    line_starts: Vec<usize>,
    source: String,
}

impl Parsed {
    fn line(&self, number: usize) -> &str {
        let start = self.line_starts[number - 1];
        let end = self
            .line_starts
            .get(number)
            .copied()
            .unwrap_or(self.source.len());
        self.source[start..end]
            .trim_end_matches('\n')
            .trim_end_matches('\r')
    }
}

#[derive(Default)]
struct State {
    calls: u32,
    /// Calls refused because the call budget was already spent.
    denied_calls: u32,
    exposed_bytes: usize,
    parsed: HashMap<String, Arc<Parsed>>,
}

/// One stage's view of the checkout. Cheap to clone; clones share budgets and
/// the parse cache.
#[derive(Clone)]
pub struct SourceSession {
    root: PathBuf,
    scopes: Arc<Vec<String>>,
    limits: SourceLimits,
    state: Arc<Mutex<State>>,
}

impl SourceSession {
    /// `scopes` are root-relative directory prefixes ending in `/`.
    pub fn new(root: PathBuf, scopes: &[&str], limits: SourceLimits) -> Self {
        Self {
            root,
            scopes: Arc::new(scopes.iter().map(|s| (*s).to_owned()).collect()),
            limits,
            state: Arc::default(),
        }
    }

    pub fn exposed_bytes(&self) -> usize {
        self.lock().exposed_bytes
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn charge_call_locked(&self, st: &mut State) -> Result<(), SourceToolError> {
        if st.calls >= self.limits.max_calls {
            st.denied_calls = st.denied_calls.saturating_add(1);
            return Err(err(
                ToolErrorCode::BudgetExhausted,
                "tool-call budget is spent; answer from what you have already read",
            ));
        }
        st.calls += 1;
        Ok(())
    }

    /// Charges one call against the shared tool-call budget without exposing
    /// source; used by non-source tools so every tool draws on one budget.
    pub fn charge_call(&self) -> Result<(), SourceToolError> {
        let mut st = self.lock();
        self.charge_call_locked(&mut st)
    }

    pub fn calls_charged(&self) -> u32 {
        self.lock().calls
    }

    /// Whether any call was refused for lack of call budget.
    pub fn budget_denied(&self) -> bool {
        self.lock().denied_calls > 0
    }

    /// Charges one call and returns the exposure bytes still available.
    fn begin(&self) -> Result<usize, SourceToolError> {
        let mut st = self.lock();
        self.charge_call_locked(&mut st)?;
        let left = self.limits.stage_bytes.saturating_sub(st.exposed_bytes);
        if left == 0 {
            return Err(err(
                ToolErrorCode::SessionBytesExhausted,
                "source exposure budget for this stage is spent; answer from what you have already read",
            ));
        }
        Ok(left)
    }

    fn spend(&self, bytes: usize) {
        self.lock().exposed_bytes += bytes;
    }

    fn in_scope(&self, rel: &str) -> bool {
        self.scopes.iter().any(|s| rel.starts_with(s.as_str()))
    }

    /// Lexical check, then canonical containment, so `..`, absolute paths, and
    /// symlinks leaving the checkout or scope all report `path_not_allowed`
    /// without revealing whether the target exists.
    fn resolve(&self, raw: &str) -> Result<(String, PathBuf), SourceToolError> {
        let denied = || {
            err(
                ToolErrorCode::PathNotAllowed,
                "path is outside the allowed source scope; use a root-relative path from search_text or outline results",
            )
        };
        let path = Path::new(raw);
        if raw.is_empty()
            || raw.len() > MAX_PATH_BYTES
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            || !self.in_scope(raw)
        {
            return Err(denied());
        }
        let full = self.root.join(path);
        let canon = match fs::canonicalize(&full) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink could point anywhere; treat it as denied.
                if fs::symlink_metadata(&full).is_ok() {
                    return Err(denied());
                }
                return Err(err(
                    ToolErrorCode::NotFound,
                    "no such file; use search_text to locate it",
                ));
            }
            Err(_) => return Err(denied()),
        };
        let root = fs::canonicalize(&self.root).map_err(|_| denied())?;
        let rel = canon.strip_prefix(&root).map_err(|_| denied())?;
        let rel_text = rel.to_string_lossy().replace('\\', "/");
        if !self.in_scope(&rel_text) {
            return Err(denied());
        }
        if !fs::metadata(&canon).map(|m| m.is_file()).unwrap_or(false) {
            return Err(err(
                ToolErrorCode::NotAFile,
                "path is not a regular file; use search_text to locate source files",
            ));
        }
        Ok((raw.to_owned(), canon))
    }

    fn load(&self, raw: &str) -> Result<(String, Arc<Parsed>), SourceToolError> {
        let (name, canon) = self.resolve(raw)?;
        self.load_resolved(name, &canon)
    }

    fn load_resolved(
        &self,
        name: String,
        canon: &Path,
    ) -> Result<(String, Arc<Parsed>), SourceToolError> {
        if let Some(hit) = self.lock().parsed.get(&name) {
            return Ok((name, hit.clone()));
        }
        let bytes = fs::read(canon).map_err(|_| {
            err(
                ToolErrorCode::NotFound,
                "file could not be read; use search_text to locate another file",
            )
        })?;
        let parsed = Arc::new(parse_file(&bytes));
        self.lock().parsed.insert(name.clone(), parsed.clone());
        Ok((name, parsed))
    }

    /// Page of the structural outline starting at the first item whose span
    /// begins on or after `start_line`.
    pub fn outline(
        &self,
        path: &str,
        start_line: Option<usize>,
    ) -> Result<OutlineResponse, SourceToolError> {
        let left = self.begin()?;
        let (name, file) = self.load(path)?;
        let cap = self.limits.outline_bytes.min(left);
        let from = start_line.unwrap_or(1).max(1);
        let kind = file.kind;
        let header = format!(
            "{name}  {} lines  {} bytes  sha256:{}  outline:{}\n",
            file.total_lines,
            file.total_bytes,
            &file.sha256[..12],
            if kind == OutlineKind::Syn {
                "syn"
            } else {
                "fallback"
            }
        );
        let mut text = header;
        let mut truncated = false;
        let mut next_line = None;
        let (mut first, mut last) = (None, None);
        for item in file.items.iter().filter(|i| i.start >= from) {
            let line = format!(
                "{:>6}-{:>6}  {}{}\n",
                item.start,
                item.end,
                "  ".repeat(item.depth),
                clip(&item.text, MAX_OUTLINE_LINE_CHARS)
            );
            if text.len() + line.len() > cap {
                truncated = true;
                next_line = Some(item.start);
                break;
            }
            text.push_str(&line);
            first.get_or_insert(item.start);
            last = Some(item.end);
        }
        if text.len() > cap {
            // Even the header does not fit the remaining stage budget.
            text.truncate(floor_boundary(&text, cap));
            truncated = true;
        }
        let bytes = text.len();
        self.spend(bytes);
        let exposed = first
            .zip(last)
            .map(|(s, e)| Exposed {
                path: name.clone(),
                file_sha256: file.sha256.clone(),
                start_line: s,
                end_line: e,
                bytes,
            })
            .into_iter()
            .collect();
        Ok(OutlineResponse {
            path: name,
            file_sha256: file.sha256.clone(),
            total_lines: file.total_lines,
            total_bytes: file.total_bytes,
            outline_kind: kind,
            code: (kind == OutlineKind::Fallback).then_some(ToolErrorCode::ParseFailed),
            text,
            truncated,
            next_line,
            bytes,
            exposed,
        })
    }

    pub fn read_lines(
        &self,
        path: &str,
        ranges: &[LineRange],
    ) -> Result<ReadLinesResponse, SourceToolError> {
        let left = self.begin()?;
        if ranges.is_empty() || ranges.len() > MAX_RANGES {
            return Err(err(
                ToolErrorCode::InvalidRange,
                format!("give between 1 and {MAX_RANGES} ranges"),
            ));
        }
        let (name, file) = self.load(path)?;
        for r in ranges {
            if r.start == 0 || r.end < r.start || r.start > file.total_lines {
                return Err(err(
                    ToolErrorCode::InvalidRange,
                    format!(
                        "ranges are 1-based inclusive with start <= end <= {}; use outline for line numbers",
                        file.total_lines
                    ),
                ));
            }
        }
        let cap = self.limits.read_bytes.min(left);
        let mut used = 0usize;
        let mut out = Vec::new();
        let mut exposed = Vec::new();
        let mut any_cut = false;
        let mut stopped = false;
        for r in ranges {
            let end = r.end.min(file.total_lines);
            let mut result = RangeResult {
                start: r.start,
                end,
                status: RangeStatus::NotServed,
                text: String::new(),
                served_end: None,
                next_line: None,
                line_truncated: false,
            };
            if stopped {
                result.next_line = Some(r.start);
                any_cut = true;
                out.push(result);
                continue;
            }
            let mut line_no = r.start;
            while line_no <= end {
                let line = file.line(line_no);
                let room = cap - used;
                let prefix = format!("{line_no:>6}| ");
                let overhead = prefix.len() + 1; // Number and trailing newline.
                if room < overhead {
                    break;
                }
                let available = room - overhead;
                let (shown, cut) = if line.len() <= available {
                    (line, false)
                } else if used == 0 {
                    let boundary = floor_boundary(line, available);
                    if boundary == 0 {
                        break;
                    }
                    (&line[..boundary], true)
                } else {
                    break;
                };
                result.text.push_str(&prefix);
                result.text.push_str(shown);
                result.text.push('\n');
                let line_bytes = overhead + shown.len();
                used += line_bytes;
                result.served_end = Some(line_no);
                result.line_truncated |= cut;
                line_no += 1;
                if cut || used >= cap {
                    break;
                }
            }
            if result.served_end.is_some() {
                exposed.push(Exposed {
                    path: name.clone(),
                    file_sha256: file.sha256.clone(),
                    start_line: r.start,
                    end_line: result.served_end.unwrap_or(r.start),
                    bytes: result.text.len(),
                });
            }
            if line_no > end && !result.line_truncated {
                result.status = RangeStatus::Served;
            } else {
                result.status = if result.served_end.is_some() {
                    RangeStatus::Partial
                } else {
                    RangeStatus::NotServed
                };
                result.next_line = Some(line_no);
                any_cut = true;
                stopped = true;
            }
            out.push(result);
        }
        self.spend(used);
        Ok(ReadLinesResponse {
            path: name,
            file_sha256: file.sha256.clone(),
            total_lines: file.total_lines,
            ranges: out,
            truncated: any_cut,
            bytes: used,
            exposed,
        })
    }

    pub fn search_text(&self, query: &str) -> Result<SearchResponse, SourceToolError> {
        let left = self.begin()?;
        if query.is_empty() || query.len() > MAX_QUERY_BYTES || query.contains(['\n', '\r']) {
            return Err(err(
                ToolErrorCode::QueryInvalid,
                format!("query must be 1 to {MAX_QUERY_BYTES} bytes on a single line"),
            ));
        }
        let mut files = Vec::new();
        let mut skipped = 0usize;
        for scope in self.scopes.iter() {
            collect_files(
                &self.root,
                Path::new(scope.trim_end_matches('/')),
                &mut files,
                &mut skipped,
            );
        }
        files.sort();
        files.dedup();
        files.truncate(MAX_SEARCH_FILES);

        let mut matches = Vec::new();
        let mut exposed = Vec::new();
        let mut bytes = 0usize;
        let mut searched = 0usize;
        let mut truncated = false;
        let budget = left;
        'files: for rel in &files {
            let Ok((name, canon)) = self.resolve(rel) else {
                skipped += 1;
                continue;
            };
            let Ok(hits) = scan_lines(&canon, query) else {
                skipped += 1;
                continue;
            };
            searched += 1;
            if hits.is_empty() {
                continue;
            }
            let Ok((_, file)) = self.load_resolved(name.clone(), &canon) else {
                skipped += 1;
                continue;
            };
            for (line_no, text) in hits {
                let excerpt = clip(text.trim(), MAX_EXCERPT_CHARS);
                let inner = file
                    .items
                    .iter()
                    .rfind(|i| i.start <= line_no && line_no <= i.end);
                let item = inner.map(|i| clip(&i.label, MAX_OUTLINE_LINE_CHARS));
                let match_bytes = excerpt.len() + item.as_ref().map_or(0, String::len);
                if matches.len() >= self.limits.search_matches || match_bytes > budget - bytes {
                    truncated = true;
                    break 'files;
                }
                bytes += match_bytes;
                exposed.push(Exposed {
                    path: name.clone(),
                    file_sha256: file.sha256.clone(),
                    start_line: line_no,
                    end_line: line_no,
                    bytes: excerpt.len(),
                });
                if let (Some(item), Some(label)) = (inner, item.as_ref()) {
                    exposed.push(Exposed {
                        path: name.clone(),
                        file_sha256: file.sha256.clone(),
                        start_line: item.start,
                        end_line: item.end,
                        bytes: label.len(),
                    });
                }
                matches.push(SearchMatch {
                    path: name.clone(),
                    line: line_no,
                    excerpt,
                    item,
                    item_start: inner.map(|i| i.start),
                    item_end: inner.map(|i| i.end),
                });
            }
        }
        self.spend(bytes);
        Ok(SearchResponse {
            matches,
            files_searched: searched,
            files_skipped: skipped,
            truncated,
            bytes,
            exposed,
        })
    }
}

fn collect_files(root: &Path, rel: &Path, out: &mut Vec<String>, skipped: &mut usize) {
    let Ok(entries) = fs::read_dir(root.join(rel)) else {
        *skipped += 1;
        return;
    };
    let mut names: Vec<_> = entries.filter_map(Result::ok).collect();
    names.sort_by_key(|e| e.file_name());
    for entry in names {
        let child = rel.join(entry.file_name());
        let Ok(kind) = entry.file_type() else {
            *skipped += 1;
            continue;
        };
        if kind.is_dir() {
            collect_files(root, &child, out, skipped);
        } else if kind.is_file() || kind.is_symlink() {
            // Symlinks are listed so `resolve` can accept in-checkout ones and
            // count escaping ones as skipped.
            out.push(child.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Streams a file by line, keeping only matching lines. Overlong lines are
/// examined only up to a bound so a minified file cannot exhaust memory.
fn scan_lines(path: &Path, query: &str) -> std::io::Result<Vec<(usize, String)>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hits = Vec::new();
    let mut buf = Vec::new();
    let mut number = 0usize;
    loop {
        buf.clear();
        let n = reader
            .by_ref()
            .take(MAX_SEARCH_LINE_BYTES)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Ok(hits);
        }
        if buf.last() != Some(&b'\n') && n as u64 == MAX_SEARCH_LINE_BYTES {
            // Discard the remainder of the overlong line.
            let mut sink = Vec::new();
            reader.read_until(b'\n', &mut sink)?;
        }
        number += 1;
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.contains(query) {
            hits.push((number, line.to_owned()));
        }
    }
}

fn floor_boundary(s: &str, mut at: usize) -> usize {
    at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_file(bytes: &[u8]) -> Parsed {
    let source = String::from_utf8_lossy(bytes).into_owned();
    let mut line_starts = vec![0usize];
    line_starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
    if line_starts.len() > 1 && *line_starts.last().unwrap_or(&0) == source.len() {
        line_starts.pop();
    }
    if source.is_empty() {
        line_starts.clear();
    }
    let total_lines = line_starts.len();
    let parsed = if bytes.len() <= MAX_PARSE_BYTES {
        parse_items(&source)
    } else {
        None
    };
    let (kind, items) = match parsed {
        Some(items) => (OutlineKind::Syn, items),
        None => (OutlineKind::Fallback, fallback_items(&source)),
    };
    Parsed {
        sha256: format!("{:x}", Sha256::digest(bytes)),
        total_lines,
        total_bytes: bytes.len(),
        kind,
        items,
        line_starts,
        source,
    }
}

fn fallback_items(source: &str) -> Vec<Item> {
    const KEYWORDS: [&str; 7] = ["fn", "struct", "enum", "impl", "mod", "use", "trait"];
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        let mut rest = trimmed;
        if let Some(r) = rest.strip_prefix("pub") {
            rest = r.trim_start();
            if rest.starts_with('(')
                && let Some(close) = rest.find(')')
            {
                rest = rest[close + 1..].trim_start();
            }
        }
        for q in ["async ", "const ", "unsafe "] {
            rest = rest.strip_prefix(q).unwrap_or(rest);
        }
        let keyword = rest.split([' ', '<', '{', '(']).next().unwrap_or("");
        if KEYWORDS.contains(&keyword) {
            let text = collapse(trimmed.trim_end_matches(['{', ' ']));
            out.push(Item {
                depth: (line.len() - trimmed.len()) / 4,
                start: index + 1,
                end: index + 1,
                label: text.clone(),
                text,
            });
        }
    }
    out
}

/// Parses on a scoped thread (fresh thread-local span map, generous stack) and
/// returns plain data. `None` on any parse failure or panic.
fn parse_items(source: &str) -> Option<Vec<Item>> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(scope, || {
                let file = syn::parse_file(source).ok()?;
                let mut out = Vec::new();
                walk_items(source, &file.items, 0, &mut out);
                Some(out)
            })
            .ok()?
            .join()
            .ok()
            .flatten()
    })
}

fn bytes_of(span: proc_macro2::Span) -> (usize, usize) {
    let r = span.byte_range();
    (r.start, r.end)
}

/// Start of an item's own text, after outer attributes and doc comments.
fn head_start(src: &str, span_start: usize, attrs: &[syn::Attribute]) -> usize {
    let after_attrs = attrs
        .iter()
        .filter(|a| matches!(a.style, syn::AttrStyle::Outer))
        .map(|a| bytes_of(a.span()).1)
        .max()
        .unwrap_or(span_start);
    let rest = src.get(after_attrs..).unwrap_or("");
    after_attrs + (rest.len() - rest.trim_start().len())
}

fn slice(src: &str, start: usize, end: usize) -> String {
    src.get(start..end.max(start))
        .map(collapse)
        .unwrap_or_default()
}

fn cut_where(text: String) -> String {
    match text.find(" where ") {
        Some(i) => text[..i].to_owned(),
        None => text.trim_end_matches(" where").to_owned(),
    }
}

fn has_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("test"))
}

fn push(out: &mut Vec<Item>, depth: usize, span: proc_macro2::Span, label: String, text: String) {
    out.push(Item {
        depth,
        start: span.start().line,
        end: span.end().line,
        label,
        text,
    });
}

fn walk_items(src: &str, items: &[syn::Item], depth: usize, out: &mut Vec<Item>) {
    let mut index = 0;
    while index < items.len() {
        if matches!(items[index], syn::Item::Use(_)) {
            let mut end = index;
            while end + 1 < items.len() && matches!(items[end + 1], syn::Item::Use(_)) {
                end += 1;
            }
            let group: Vec<_> = items[index..=end]
                .iter()
                .filter_map(|i| match i {
                    syn::Item::Use(u) => Some(u),
                    _ => None,
                })
                .collect();
            let paths: Vec<String> = group
                .iter()
                .map(|u| {
                    let (s, e) = bytes_of(u.tree.span());
                    let text = slice(src, s, e);
                    match text.find('{') {
                        Some(i) => format!("{}{{…}}", &text[..i]),
                        None => text,
                    }
                })
                .collect();
            let start = items[index].span().start().line;
            let finish = items[end].span().end().line;
            out.push(Item {
                depth,
                start,
                end: finish,
                label: "use".into(),
                text: format!("use  {}", paths.join("; ")),
            });
            index = end + 1;
            continue;
        }
        walk_item(src, &items[index], depth, out);
        index += 1;
    }
}

fn fields_text(fields: &syn::Fields) -> String {
    match fields {
        syn::Fields::Named(n) => {
            let names: Vec<String> = n
                .named
                .iter()
                .filter_map(|f| f.ident.as_ref().map(ToString::to_string))
                .collect();
            format!(" {{ {} }}", names.join(", "))
        }
        syn::Fields::Unnamed(u) => format!("({} fields)", u.unnamed.len()),
        syn::Fields::Unit => String::new(),
    }
}

fn walk_item(src: &str, item: &syn::Item, depth: usize, out: &mut Vec<Item>) {
    let span = item.span();
    let (sb, _) = bytes_of(span);
    // `prefix` is the collapsed text from the item's own start up to its name.
    let prefix = |attrs: &[syn::Attribute], ident: &syn::Ident| {
        slice(src, head_start(src, sb, attrs), bytes_of(ident.span()).0)
    };
    match item {
        syn::Item::Mod(m) => {
            let p = prefix(&m.attrs, &m.ident);
            push(
                out,
                depth,
                span,
                format!("mod {}", m.ident),
                format!("{p} {}", m.ident),
            );
            if let Some((_, inner)) = &m.content {
                walk_items(src, inner, depth + 1, out);
            }
        }
        syn::Item::Struct(s) => {
            let p = prefix(&s.attrs, &s.ident);
            let text = format!("{p} {}{}", s.ident, fields_text(&s.fields));
            push(out, depth, span, format!("struct {}", s.ident), text);
        }
        syn::Item::Enum(e) => {
            let p = prefix(&e.attrs, &e.ident);
            let names: Vec<String> = e.variants.iter().map(|v| v.ident.to_string()).collect();
            let text = format!("{p} {} {{ {} }}", e.ident, names.join(", "));
            push(out, depth, span, format!("enum {}", e.ident), text);
        }
        syn::Item::Union(u) => {
            let p = prefix(&u.attrs, &u.ident);
            push(
                out,
                depth,
                span,
                format!("union {}", u.ident),
                format!("{p} {}", u.ident),
            );
        }
        syn::Item::Trait(t) => {
            let p = prefix(&t.attrs, &t.ident);
            push(
                out,
                depth,
                span,
                format!("trait {}", t.ident),
                format!("{p} {}", t.ident),
            );
            walk_trait(src, &t.items, depth + 1, out);
        }
        syn::Item::Impl(i) => {
            let start = head_start(src, sb, &i.attrs);
            let text = cut_where(slice(src, start, bytes_of(i.brace_token.span.open()).0));
            push(out, depth, span, text.clone(), text);
            walk_impl(src, &i.items, depth + 1, out);
        }
        syn::Item::Fn(f) => {
            let start = head_start(src, sb, &f.attrs);
            let sig = cut_where(slice(src, start, bytes_of(f.sig.span()).1));
            let tag = if has_test(&f.attrs) { "#[test] " } else { "" };
            push(
                out,
                depth,
                span,
                format!("fn {}", f.sig.ident),
                format!("{tag}{sig}"),
            );
        }
        syn::Item::Const(c) => {
            let p = prefix(&c.attrs, &c.ident);
            let ty = slice(src, bytes_of(c.ty.span()).0, bytes_of(c.ty.span()).1);
            push(
                out,
                depth,
                span,
                format!("const {}", c.ident),
                format!("{p} {}: {ty}", c.ident),
            );
        }
        syn::Item::Static(c) => {
            let p = prefix(&c.attrs, &c.ident);
            let ty = slice(src, bytes_of(c.ty.span()).0, bytes_of(c.ty.span()).1);
            push(
                out,
                depth,
                span,
                format!("static {}", c.ident),
                format!("{p} {}: {ty}", c.ident),
            );
        }
        syn::Item::Type(t) => {
            let p = prefix(&t.attrs, &t.ident);
            push(
                out,
                depth,
                span,
                format!("type {}", t.ident),
                format!("{p} {}", t.ident),
            );
        }
        syn::Item::Macro(m) => {
            if m.mac.path.is_ident("macro_rules")
                && let Some(ident) = &m.ident
            {
                push(
                    out,
                    depth,
                    span,
                    format!("macro_rules! {ident}"),
                    format!("macro_rules! {ident}"),
                );
            }
        }
        _ => {}
    }
}

fn walk_impl(src: &str, items: &[syn::ImplItem], depth: usize, out: &mut Vec<Item>) {
    for item in items {
        let span = item.span();
        let (sb, _) = bytes_of(span);
        match item {
            syn::ImplItem::Fn(f) => {
                let start = head_start(src, sb, &f.attrs);
                let sig = cut_where(slice(src, start, bytes_of(f.sig.span()).1));
                let tag = if has_test(&f.attrs) { "#[test] " } else { "" };
                push(
                    out,
                    depth,
                    span,
                    format!("fn {}", f.sig.ident),
                    format!("{tag}{sig}"),
                );
            }
            syn::ImplItem::Const(c) => {
                let start = head_start(src, sb, &c.attrs);
                let p = slice(src, start, bytes_of(c.ident.span()).0);
                push(
                    out,
                    depth,
                    span,
                    format!("const {}", c.ident),
                    format!("{p} {}", c.ident),
                );
            }
            syn::ImplItem::Type(t) => {
                let start = head_start(src, sb, &t.attrs);
                let p = slice(src, start, bytes_of(t.ident.span()).0);
                push(
                    out,
                    depth,
                    span,
                    format!("type {}", t.ident),
                    format!("{p} {}", t.ident),
                );
            }
            _ => {}
        }
    }
}

fn walk_trait(src: &str, items: &[syn::TraitItem], depth: usize, out: &mut Vec<Item>) {
    for item in items {
        let span = item.span();
        let (sb, _) = bytes_of(span);
        match item {
            syn::TraitItem::Fn(f) => {
                let start = head_start(src, sb, &f.attrs);
                let sig = cut_where(slice(src, start, bytes_of(f.sig.span()).1));
                push(out, depth, span, format!("fn {}", f.sig.ident), sig);
            }
            syn::TraitItem::Const(c) => {
                let start = head_start(src, sb, &c.attrs);
                let p = slice(src, start, bytes_of(c.ident.span()).0);
                push(
                    out,
                    depth,
                    span,
                    format!("const {}", c.ident),
                    format!("{p} {}", c.ident),
                );
            }
            syn::TraitItem::Type(t) => {
                let start = head_start(src, sb, &t.attrs);
                let p = slice(src, start, bytes_of(t.ident.span()).0);
                push(
                    out,
                    depth,
                    span,
                    format!("type {}", t.ident),
                    format!("{p} {}", t.ident),
                );
            }
            _ => {}
        }
    }
}

/// Typed arguments, shared by the tools and the trace's argument projection.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutlineArgs {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadLinesArgs {
    pub path: String,
    pub ranges: Vec<LineRange>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchTextArgs {
    pub query: String,
}

/// Tool names whose results carry an `exposed` ledger the trace projects.
pub const SOURCE_TOOL_NAMES: [&str; 3] = ["outline", "read_lines", "search_text"];

fn to_output<T: Serialize>(response: &T) -> Result<Value, SourceToolError> {
    serde_json::to_value(response)
        .map_err(|_| err(ToolErrorCode::ParseFailed, "response encoding failed"))
}

/// Typed failures reach the model as tool output carrying the stable code and
/// the next-step hint, and the trace reads the same code back from the result.
fn execution_error(error: SourceToolError) -> ToolExecutionError {
    let kind = match error.code {
        ToolErrorCode::NotFound => ToolErrorKind::NotFound,
        ToolErrorCode::PathNotAllowed => ToolErrorKind::PermissionDenied,
        ToolErrorCode::InvalidRange | ToolErrorCode::QueryInvalid => ToolErrorKind::InvalidArgs,
        _ => ToolErrorKind::Other,
    };
    let text = error.to_string();
    ToolExecutionError::new(kind, text.clone())
        .with_code(error.code.as_str())
        .with_model_feedback(text)
}

#[derive(Clone)]
pub struct OutlineTool(pub SourceSession);
impl PortableTool for OutlineTool {
    const NAME: &'static str = "outline";
    type Args = OutlineArgs;
    type Output = Value;
    type Error = SourceToolError;
    fn map_error(&self, error: SourceToolError) -> ToolExecutionError {
        execution_error(error)
    }
    fn description(&self) -> String {
        "Compact outline of one Rust source file with 1-based line spans; page with start_line."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"start_line":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false})
    }
    async fn call(&self, args: OutlineArgs) -> Result<Value, SourceToolError> {
        to_output(&self.0.outline(&args.path, args.start_line)?)
    }
}

#[derive(Clone)]
pub struct ReadLinesTool(pub SourceSession);
impl PortableTool for ReadLinesTool {
    const NAME: &'static str = "read_lines";
    type Args = ReadLinesArgs;
    type Output = Value;
    type Error = SourceToolError;
    fn map_error(&self, error: SourceToolError) -> ToolExecutionError {
        execution_error(error)
    }
    fn description(&self) -> String {
        "Read up to 4 inclusive 1-based line ranges of one source file; responses are small and truncated at whole lines.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"path":{"type":"string"},"ranges":{"type":"array","minItems":1,"maxItems":MAX_RANGES,"items":{"type":"object","properties":{"start":{"type":"integer","minimum":1},"end":{"type":"integer","minimum":1}},"required":["start","end"],"additionalProperties":false}}},"required":["path","ranges"],"additionalProperties":false})
    }
    async fn call(&self, args: ReadLinesArgs) -> Result<Value, SourceToolError> {
        to_output(&self.0.read_lines(&args.path, &args.ranges)?)
    }
}

#[derive(Clone)]
pub struct SearchTextTool(pub SourceSession);
impl PortableTool for SearchTextTool {
    const NAME: &'static str = "search_text";
    type Args = SearchTextArgs;
    type Output = Value;
    type Error = SourceToolError;
    fn map_error(&self, error: SourceToolError) -> ToolExecutionError {
        execution_error(error)
    }
    fn description(&self) -> String {
        "Search allowed source files for a single-line text; matches name their enclosing item and span.".into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"query":{"type":"string","maxLength":MAX_QUERY_BYTES}},"required":["query"],"additionalProperties":false})
    }
    async fn call(&self, args: SearchTextArgs) -> Result<Value, SourceToolError> {
        to_output(&self.0.search_text(&args.query)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    const SCOPE: &str = "crates/baho-core/src/";

    fn limits() -> SourceLimits {
        SourceLimits {
            max_calls: 100,
            read_bytes: 1024,
            outline_bytes: 6 * 1024,
            search_matches: 40,
            stage_bytes: 48 * 1024,
        }
    }

    fn session_with(limits: SourceLimits) -> (tempfile::TempDir, SourceSession) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(SCOPE)).unwrap();
        let s = SourceSession::new(dir.path().to_path_buf(), &[SCOPE], limits);
        (dir, s)
    }

    fn write(dir: &tempfile::TempDir, name: &str, text: &str) {
        let p = dir.path().join(SCOPE).join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn rel(name: &str) -> String {
        format!("{SCOPE}{name}")
    }

    /// Over 64 KiB: a use block, an enum, an impl with methods, many generated
    /// functions, and a test module.
    fn big_source() -> String {
        let mut s = String::from("use std::fmt;\nuse std::collections::{BTreeMap, BTreeSet};\n\n");
        s.push_str("/// Operators.\n#[derive(Debug)]\npub enum Op {\n    Equal,\n    Less,\n}\n\n");
        s.push_str("pub struct Parser {\n    pos: usize,\n    text: String,\n}\n\n");
        s.push_str("impl Parser {\n    /// Parse one.\n    pub fn parse(&mut self, pos: usize) -> Option<Op>\n    where\n        Self: Sized,\n    {\n        let _ = pos;\n        Some(Op::Equal)\n    }\n}\n\n");
        for i in 0..1500 {
            writeln!(
                s,
                "fn generated_{i}(x: u32) -> u32 {{\n    x + {i} // marker_{i}\n}}\n"
            )
            .unwrap();
        }
        s.push_str("#[cfg(test)]\nmod tests {\n    #[test]\n    fn equals_word() {\n        assert!(true);\n    }\n}\n");
        assert!(s.len() > 64 * 1024);
        s
    }

    #[test]
    fn outline_of_large_file_has_correct_spans_and_stays_in_cap() {
        let mut l = limits();
        l.stage_bytes = 1024 * 1024;
        let (dir, s) = session_with(l);
        let src = big_source();
        write(&dir, "big.rs", &src);
        let lines: Vec<&str> = src.lines().collect();
        let first = s.outline(&rel("big.rs"), None).unwrap();
        assert_eq!(first.outline_kind, OutlineKind::Syn);
        assert!(first.code.is_none());
        assert!(first.text.len() <= 6 * 1024);
        assert!(first.truncated);
        assert_eq!(first.total_lines, lines.len());
        let t = &first.text;
        assert!(
            t.contains("1-     2  use  std; std::collections::{…}")
                || t.contains("use  std::fmt; std::collections::{…}"),
            "{t}"
        );
        assert!(t.contains("enum Op { Equal, Less }"), "{t}");
        assert!(t.contains("struct Parser { pos, text }"));
        // Span includes doc + derive (lines 5-8 of the file).
        let enum_line = t.lines().find(|l| l.contains("enum Op")).unwrap();
        assert!(enum_line.trim_start().starts_with("4-"), "{enum_line}");
        // Signature without body or where clause, nested under the impl.
        let parse = t.lines().find(|l| l.contains("fn parse")).unwrap();
        assert!(parse.contains("pub fn parse(&mut self, pos: usize) -> Option<Op>"));
        assert!(!parse.contains("where"));
        assert!(parse.contains("    pub fn"), "indented: {parse}");
        // Paging reaches the tests module and every generated fn.
        let mut all = String::new();
        let mut next = None;
        let mut pages = 0;
        loop {
            let page = s.outline(&rel("big.rs"), next).unwrap();
            assert!(page.text.len() <= 6 * 1024);
            all.push_str(&page.text);
            pages += 1;
            match page.next_line {
                Some(n) => next = Some(n),
                None => break,
            }
            if pages > 20 {
                panic!("paging did not finish");
            }
        }
        assert!(pages > 1);
        assert!(all.contains("fn generated_1499"));
        assert!(all.contains("mod tests"));
        assert!(all.contains("#[test] fn equals_word"));
        let g = all.lines().find(|l| l.contains("fn generated_7(")).unwrap();
        let start: usize = g.split('-').next().unwrap().trim().parse().unwrap();
        assert!(lines[start - 1].starts_with("fn generated_7("));
    }

    #[test]
    fn read_lines_serves_any_range_of_large_file_within_cap() {
        let (dir, s) = session_with(limits());
        let src = big_source();
        write(&dir, "big.rs", &src);
        let lines: Vec<&str> = src.lines().collect();
        let r = s
            .read_lines(&rel("big.rs"), &[LineRange { start: 7, end: 9 }])
            .unwrap();
        assert_eq!(r.ranges[0].status, RangeStatus::Served);
        assert!(r.ranges[0].text.contains(&format!("     7| {}", lines[6])));
        assert!(r.bytes <= 1024);
        // A far range, end beyond file length is clamped.
        let n = lines.len();
        let r = s
            .read_lines(
                &rel("big.rs"),
                &[LineRange {
                    start: n - 1,
                    end: n + 50,
                }],
            )
            .unwrap();
        assert_eq!(r.ranges[0].end, n);
        assert_eq!(r.ranges[0].served_end, Some(n));
        assert_eq!(r.exposed[0].file_sha256.len(), 64);
    }

    #[test]
    fn read_lines_cap_cuts_on_whole_lines_across_ranges() {
        let (dir, s) = session_with(limits());
        let body: String = (1..=200).map(|i| format!("line{i:03}\n")).collect(); // 7 bytes each
        write(&dir, "a.rs", &body);
        let r = s
            .read_lines(
                &rel("a.rs"),
                &[
                    LineRange { start: 1, end: 180 },
                    LineRange {
                        start: 150,
                        end: 160,
                    },
                    LineRange {
                        start: 170,
                        end: 171,
                    },
                ],
            )
            .unwrap();
        assert!(r.truncated);
        assert!(r.bytes <= 1024);
        assert_eq!(
            r.bytes,
            r.ranges.iter().map(|range| range.text.len()).sum::<usize>()
        );
        let first = &r.ranges[0];
        assert_eq!(first.status, RangeStatus::Partial);
        assert_eq!(first.served_end, Some(64)); // 64 * (9-byte numbering + 7-byte content)
        assert_eq!(first.next_line, Some(65));
        assert_eq!(r.ranges[1].status, RangeStatus::NotServed);
        assert_eq!(r.ranges[1].next_line, Some(150));
        assert_eq!(r.ranges[2].status, RangeStatus::NotServed);
        assert_eq!(r.exposed.len(), 1);
        assert_eq!((r.exposed[0].start_line, r.exposed[0].end_line), (1, 64));
        assert_eq!(r.exposed[0].bytes, 1024);
        // Continuation starts exactly at next_line.
        let r2 = s
            .read_lines(&rel("a.rs"), &[LineRange { start: 65, end: 66 }])
            .unwrap();
        assert!(r2.ranges[0].text.starts_with("    65| line065"));
    }

    #[test]
    fn blank_lines_count_toward_read_response_cap() {
        let (dir, s) = session_with(limits());
        write(&dir, "blank.txt", &"\n".repeat(200));
        let r = s
            .read_lines(&rel("blank.txt"), &[LineRange { start: 1, end: 200 }])
            .unwrap();
        let range = &r.ranges[0];
        assert_eq!(range.status, RangeStatus::Partial);
        assert_eq!(range.served_end, Some(113)); // 113 numbered blank lines * 9 bytes
        assert_eq!(range.next_line, Some(114));
        assert!(!range.line_truncated);
        assert!(r.truncated);
        assert_eq!(r.bytes, range.text.len());
        assert_eq!(r.bytes, r.exposed[0].bytes);
        assert!(r.bytes <= 1024);
        assert_eq!(range.text.lines().count(), 113);
    }

    #[test]
    fn clipped_final_requested_line_is_reported_as_partial() {
        let (dir, s) = session_with(limits());
        write(&dir, "long.txt", &format!("{}\n", "a".repeat(1100)));
        let r = s
            .read_lines(&rel("long.txt"), &[LineRange { start: 1, end: 1 }])
            .unwrap();
        let range = &r.ranges[0];
        assert_eq!(range.status, RangeStatus::Partial);
        assert_eq!(range.served_end, Some(1));
        assert_eq!(range.next_line, Some(2));
        assert!(range.line_truncated);
        assert!(r.truncated);
        assert_eq!(r.bytes, 1024);
        assert_eq!(r.bytes, range.text.len());
    }

    #[test]
    fn overlong_multibyte_line_is_cut_on_char_boundary() {
        let (dir, s) = session_with(limits());
        let long = "é".repeat(1500); // 3000 bytes
        write(&dir, "u.rs", &format!("{long}\nshort\n"));
        let r = s
            .read_lines(&rel("u.rs"), &[LineRange { start: 1, end: 2 }])
            .unwrap();
        let range = &r.ranges[0];
        assert!(range.line_truncated);
        assert_eq!(r.bytes, 1023); // 507 two-byte chars and 9 bytes of numbering
        assert_eq!(range.status, RangeStatus::Partial);
        assert_eq!(range.served_end, Some(1));
        assert_eq!(range.next_line, Some(2));
        assert!(range.text.is_char_boundary(range.text.len()));
    }

    #[test]
    fn invalid_ranges_are_typed() {
        let (dir, s) = session_with(limits());
        write(&dir, "a.rs", "a\nb\n");
        let p = rel("a.rs");
        let code = |r: &[LineRange]| s.read_lines(&p, r).unwrap_err().code;
        assert_eq!(code(&[]), ToolErrorCode::InvalidRange);
        assert_eq!(
            code(&[LineRange { start: 0, end: 1 }]),
            ToolErrorCode::InvalidRange
        );
        assert_eq!(
            code(&[LineRange { start: 2, end: 1 }]),
            ToolErrorCode::InvalidRange
        );
        assert_eq!(
            code(&[LineRange { start: 3, end: 3 }]),
            ToolErrorCode::InvalidRange
        );
        let five = [LineRange { start: 1, end: 1 }; 5];
        assert_eq!(code(&five), ToolErrorCode::InvalidRange);
    }

    #[test]
    fn search_is_sorted_has_enclosing_items_and_skips_unreadable_files() {
        let (dir, s) = session_with(limits());
        write(&dir, "z.rs", "fn zed() {\n    marker\n}\n");
        write(&dir, "big.rs", &big_source());
        write(
            &dir,
            "sub/a.rs",
            "impl Foo {\n    fn bar() {\n        marker_x\n    }\n}\n// marker top\nconst TOP: u8 = 0;\n",
        );
        #[cfg(unix)]
        let unreadable = {
            use std::os::unix::fs::PermissionsExt;
            write(&dir, "locked.rs", "marker\n");
            let p = dir.path().join(SCOPE).join("locked.rs");
            fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
            fs::File::open(&p).is_err()
        };
        let r = s.search_text("marker").unwrap();
        let order: Vec<_> = r.matches.iter().map(|m| (m.path.clone(), m.line)).collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
        assert!(r.matches.len() <= 40);
        assert!(r.truncated); // big.rs has 1500 hits
        assert_eq!(r.matches[0].path, rel("big.rs"));
        assert_eq!(r.matches[0].item.as_deref(), Some("fn generated_0"));
        assert_eq!(r.matches[0].item_start, Some(27));
        assert!(r.matches.iter().all(|m| m.excerpt.chars().count() <= 160));
        let r = s.search_text("marker_x").unwrap();
        #[cfg(unix)]
        if unreadable {
            assert_eq!(r.files_skipped, 1);
        }
        let m = &r.matches[0];
        assert_eq!(
            (m.path.as_str(), m.line),
            ("crates/baho-core/src/sub/a.rs", 3)
        );
        assert_eq!(m.item.as_deref(), Some("fn bar"));
        assert_eq!((m.item_start, m.item_end), (Some(2), Some(4)));
        let r = s.search_text("marker top").unwrap();
        assert_eq!(r.matches[0].item, None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join(SCOPE).join("locked.rs");
            fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    #[test]
    fn search_query_validation() {
        let (_dir, s) = session_with(limits());
        for q in ["", &"x".repeat(101), "a\nb"] {
            assert_eq!(
                s.search_text(q).unwrap_err().code,
                ToolErrorCode::QueryInvalid
            );
        }
    }

    #[test]
    fn search_charges_and_bounds_enclosing_item_labels() {
        let mut l = limits();
        l.stage_bytes = 256;
        let (dir, s) = session_with(l);
        let long_header = format!("impl {} {{\n", "VeryLongTypeName".repeat(80));
        write(
            &dir,
            "long.rs",
            &format!("{long_header}    // marker\n}}\n"),
        );

        let r = s.search_text("marker").unwrap();
        assert_eq!(r.matches.len(), 1);
        let m = &r.matches[0];
        assert!(m.item.as_ref().unwrap().chars().count() <= MAX_OUTLINE_LINE_CHARS);
        assert_eq!(r.bytes, m.excerpt.len() + m.item.as_ref().unwrap().len());
        assert_eq!(r.bytes, r.exposed.iter().map(|e| e.bytes).sum::<usize>());
        assert_eq!(s.exposed_bytes(), r.bytes);
        assert!(r.bytes <= 256);
        assert!(
            r.exposed
                .iter()
                .any(|e| e.start_line == 1 && e.end_line == 3)
        );

        let again = s.search_text("marker").unwrap();
        assert!(again.matches.is_empty());
        assert!(again.truncated);
        assert_eq!(again.bytes, 0);
        assert_eq!(s.exposed_bytes(), r.bytes);
    }

    #[test]
    fn search_bounds_fallback_item_label() {
        let mut l = limits();
        l.stage_bytes = 320;
        let (dir, s) = session_with(l);
        write(
            &dir,
            "bad.rs",
            &format!("pub fn {} marker {{\nlet = ;\n", "name".repeat(300)),
        );

        let r = s.search_text("marker").unwrap();
        let m = &r.matches[0];
        assert!(m.item.as_ref().unwrap().chars().count() <= MAX_OUTLINE_LINE_CHARS);
        assert_eq!(r.bytes, m.excerpt.len() + m.item.as_ref().unwrap().len());
        assert_eq!(r.bytes, r.exposed.iter().map(|e| e.bytes).sum::<usize>());
        assert!(r.bytes <= 320);
    }

    #[test]
    fn unparseable_file_gets_fallback_outline_and_still_reads() {
        let (dir, s) = session_with(limits());
        write(
            &dir,
            "bad.rs",
            "use a::b;\npub fn ok(x: u32) {\n    let = ;\nstruct S {\n",
        );
        let o = s.outline(&rel("bad.rs"), None).unwrap();
        assert_eq!(o.outline_kind, OutlineKind::Fallback);
        assert_eq!(o.code, Some(ToolErrorCode::ParseFailed));
        assert!(o.text.contains("outline:fallback"));
        assert!(
            o.text.contains("2-     2  pub fn ok(x: u32) {")
                || o.text.contains("pub fn ok(x: u32)")
        );
        assert!(o.text.contains("struct S"));
        let r = s
            .read_lines(&rel("bad.rs"), &[LineRange { start: 3, end: 3 }])
            .unwrap();
        assert!(r.ranges[0].text.contains("let = ;"));
    }

    #[test]
    fn paths_outside_scope_or_checkout_are_denied_and_expose_nothing() {
        let (dir, s) = session_with(limits());
        write(&dir, "a.rs", "ok\n");
        fs::write(dir.path().join("secret.txt"), "secret\n").unwrap();
        fs::create_dir_all(dir.path().join("crates/other")).unwrap();
        fs::write(dir.path().join("crates/other/x.rs"), "x\n").unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("o.rs"), "outside\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.path().join("o.rs"),
                dir.path().join(SCOPE).join("link.rs"),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                dir.path().join("secret.txt"),
                dir.path().join(SCOPE).join("inroot.rs"),
            )
            .unwrap();
            std::os::unix::fs::symlink("nowhere", dir.path().join(SCOPE).join("dangling.rs"))
                .unwrap();
        }
        let mut bad = vec![
            "../secret.txt".to_owned(),
            format!("{SCOPE}../../../secret.txt"),
            "/etc/passwd".to_owned(),
            "secret.txt".to_owned(),
            "crates/other/x.rs".to_owned(),
            String::new(),
            "x".repeat(300),
        ];
        #[cfg(unix)]
        bad.extend([rel("link.rs"), rel("inroot.rs"), rel("dangling.rs")]);
        for path in &bad {
            let e = s.outline(path, None).unwrap_err();
            assert_eq!(e.code, ToolErrorCode::PathNotAllowed, "{path}");
            let e = s
                .read_lines(path, &[LineRange { start: 1, end: 1 }])
                .unwrap_err();
            assert_eq!(e.code, ToolErrorCode::PathNotAllowed, "{path}");
        }
        assert_eq!(s.exposed_bytes(), 0);
        // Escaping symlinks are never searched or exposed.
        let r = s.search_text("outside").unwrap();
        assert!(r.matches.is_empty());
        let r = s.search_text("secret").unwrap();
        assert!(r.matches.is_empty());
        #[cfg(unix)]
        assert!(r.files_skipped >= 3);
    }

    #[test]
    fn not_found_and_not_a_file() {
        let (dir, s) = session_with(limits());
        fs::create_dir_all(dir.path().join(SCOPE).join("d")).unwrap();
        assert_eq!(
            s.outline(&rel("missing.rs"), None).unwrap_err().code,
            ToolErrorCode::NotFound
        );
        assert_eq!(
            s.outline(&rel("d"), None).unwrap_err().code,
            ToolErrorCode::NotAFile
        );
    }

    #[test]
    fn call_and_byte_budgets_are_enforced_with_typed_errors() {
        let mut l = limits();
        l.max_calls = 3;
        l.stage_bytes = 48;
        let (dir, s) = session_with(l);
        write(&dir, "a.rs", &"0123456789\n".repeat(10));
        let p = rel("a.rs");
        // The stage budget counts numbering and newlines as well as content.
        let r = s
            .read_lines(&p, &[LineRange { start: 1, end: 10 }])
            .unwrap();
        assert_eq!(r.bytes, 38); // two numbered 10-byte lines
        assert!(r.truncated);
        let r2 = s
            .read_lines(&p, &[LineRange { start: 1, end: 10 }])
            .unwrap();
        assert_eq!(r2.bytes, 10); // 10 bytes remain: one character fits after numbering
        assert!(r2.ranges[0].line_truncated);
        // Budget now fully spent.
        let e = s
            .read_lines(&p, &[LineRange { start: 1, end: 1 }])
            .unwrap_err();
        assert_eq!(e.code, ToolErrorCode::SessionBytesExhausted);
        // Fourth call exceeds max_calls.
        let e = s.search_text("0").unwrap_err();
        assert_eq!(e.code, ToolErrorCode::BudgetExhausted);
    }

    #[test]
    fn error_codes_have_stable_names() {
        use ToolErrorCode::*;
        let names: Vec<_> = [
            PathNotAllowed,
            NotFound,
            NotAFile,
            InvalidRange,
            QueryInvalid,
            BudgetExhausted,
            SessionBytesExhausted,
            ParseFailed,
        ]
        .iter()
        .map(|c| c.as_str())
        .collect();
        assert_eq!(
            names,
            [
                "path_not_allowed",
                "not_found",
                "not_a_file",
                "invalid_range",
                "query_invalid",
                "budget_exhausted",
                "session_bytes_exhausted",
                "parse_failed"
            ]
        );
        assert_eq!(
            serde_json::to_string(&ParseFailed).unwrap(),
            "\"parse_failed\""
        );
        let e = err(NotFound, "x");
        assert!(e.to_string().starts_with("not_found:"));
    }
}
