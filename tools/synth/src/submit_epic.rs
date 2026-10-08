//! `submit_epic`: the only write path of the epic planner (Epic 016, T6).
//!
//! The agent supplies a slug and Markdown. The harness validates the whole submission,
//! chooses the file name, allocates the number under a repository-scoped lock, and publishes
//! the complete file with no-overwrite semantics. Rejected submissions write nothing.

use crate::{
    contracts::{EvaluationReport, ReviewStatus},
    planner_tools::{ToolError, allowed_file, is_denied, normalize},
    redaction::{Redactor, ViolationKind},
};
use rig::tool::{PortableTool, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

pub const MAX_EPIC_BYTES: usize = 48 * 1024;
pub const MAX_SUBMISSIONS: usize = 8;
const GOALS: std::ops::RangeInclusive<u32> = 1..=8;
const PATH_EXTENSIONS: &[&str] = &[".rs", ".md", ".toml", ".json", ".csv", ".txt", ".feature"];
const LOCK_STALE: Duration = Duration::from_secs(60);
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Stable reason codes; the prose detail may improve without breaking consumers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    BadSlug,
    Empty,
    Oversized,
    BadHeader,
    HtmlComment,
    MissingSection,
    MissingGoal,
    BadGoalRow,
    UnknownEvidenceId,
    MissingEvidenceId,
    ChosenIncrement,
    DeferredContent,
    UnlabeledEvidence,
    PathInventory,
    PathUnlisted,
    PathDenied,
    PathMissing,
    PathNotOwned,
    PathAmbiguous,
    Forbidden,
    DatasetCopy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reason {
    pub code: ReasonCode,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RejectionRecord {
    pub codes: Vec<ReasonCode>,
    /// The sanitized, bounded reasons that were returned to the agent, in code order.
    /// Absent in records written before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<RecordedReason>,
}

/// What the agent was told about one violation; never the rejected text itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedReason {
    pub code: ReasonCode,
    pub message: String,
}

const MAX_RECORDED_REASON_CHARS: usize = 400;

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("submission rejected: {}", render(.0))]
    Rejected(Vec<Reason>),
    #[error("an epic was already accepted in this run")]
    AlreadyAccepted,
    #[error("submission attempt budget exhausted")]
    AttemptsExhausted,
    #[error("epic could not be published: {0}")]
    Publish(String),
}

fn render(reasons: &[Reason]) -> String {
    reasons
        .iter()
        .map(|r| format!("[{:?}] {}", r.code, r.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub number: u32,
    /// Repository-relative path, e.g. `epics/017-slug.md`.
    pub relative_path: String,
}

#[derive(Default)]
struct State {
    accepted: Option<Accepted>,
    attempts: usize,
    rejections: Vec<RejectionRecord>,
}

struct Ctx {
    root: PathBuf,
    redactor: Redactor,
    evaluation_id: String,
    generation_id: String,
    case_ids: BTreeSet<String>,
    has_unreviewed: bool,
    has_ambiguous: bool,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct EpicSubmitter(Arc<Ctx>);

impl EpicSubmitter {
    /// `repo_root` owns `epics/`. Tests pass a temporary directory.
    pub fn new(repo_root: &Path, report: &EvaluationReport, redactor: Redactor) -> Option<Self> {
        let root = repo_root.canonicalize().ok()?;
        let any = |s: ReviewStatus| report.cases.iter().any(|c| c.review == s);
        Some(Self(Arc::new(Ctx {
            root,
            redactor,
            evaluation_id: report.evaluation_id.clone(),
            generation_id: report.generation_id.clone(),
            case_ids: report.cases.iter().map(|c| c.case_id.clone()).collect(),
            has_unreviewed: any(ReviewStatus::Unreviewed),
            has_ambiguous: any(ReviewStatus::Ambiguous),
            state: Mutex::new(State::default()),
        })))
    }

    pub fn accepted(&self) -> Option<Accepted> {
        self.0.state.lock().unwrap().accepted.clone()
    }

    pub fn rejections(&self) -> Vec<RejectionRecord> {
        self.0.state.lock().unwrap().rejections.clone()
    }

    pub fn submit(&self, slug: &str, markdown: &str) -> Result<Accepted, SubmitError> {
        let ctx = &*self.0;
        // Held for the whole call: in-process submissions are serialized, and a second
        // acceptance is impossible.
        let mut state = ctx.state.lock().unwrap();
        if state.accepted.is_some() {
            return Err(SubmitError::AlreadyAccepted);
        }
        if state.attempts >= MAX_SUBMISSIONS {
            return Err(SubmitError::AttemptsExhausted);
        }
        state.attempts += 1;
        let title = match validate(ctx, slug, markdown) {
            Ok(t) => t,
            Err(mut reasons) => {
                reasons.sort_by_key(|r| r.code);
                for r in &mut reasons {
                    r.detail = ctx.redactor.sanitize_prose(&r.detail);
                }
                state.rejections.push(RejectionRecord {
                    codes: reasons.iter().map(|r| r.code).collect(),
                    reasons: reasons
                        .iter()
                        .map(|r| RecordedReason {
                            code: r.code,
                            message: recorded_message(&ctx.redactor, &r.detail),
                        })
                        .collect(),
                });
                return Err(SubmitError::Rejected(reasons));
            }
        };
        let accepted = publish(ctx, slug, &title, markdown).map_err(SubmitError::Publish)?;
        state.accepted = Some(accepted.clone());
        Ok(accepted)
    }
}

/// Details are already sanitized; a message that still scans dirty is withheld, and the rest
/// is capped so a long echoed ID cannot bloat the record.
fn recorded_message(redactor: &Redactor, detail: &str) -> String {
    if !redactor.scan_prose(detail).is_empty() {
        return "[withheld]".into();
    }
    detail.chars().take(MAX_RECORDED_REASON_CHARS).collect()
}

/// Describes the redaction violations by kind and 1-based line number, never by content.
fn forbidden_detail(redactor: &Redactor, md: &str, kinds: &[ViolationKind]) -> String {
    const MAX_LINES: usize = 12;
    let mut parts = Vec::new();
    for kind in kinds {
        let lines: Vec<usize> = md
            .lines()
            .enumerate()
            .filter(|(_, l)| redactor.scan_prose(l).contains(kind))
            .map(|(i, _)| i + 1)
            .collect();
        let (name, fix) = match kind {
            ViolationKind::SourceLiteral => (
                "a protected source value (whole word)",
                "use only the stand-in names the tools returned",
            ),
            ViolationKind::AbsolutePath => (
                "an absolute or home-relative path",
                "write repository-relative paths with no leading slash or tilde",
            ),
            ViolationKind::Credential => (
                "a credential-shaped token",
                "remove keys, tokens, or long encoded strings",
            ),
        };
        let where_ = if lines.is_empty() {
            "spanning lines".to_owned()
        } else {
            let shown: Vec<String> = lines
                .iter()
                .take(MAX_LINES)
                .map(|n| n.to_string())
                .collect();
            let more = if lines.len() > MAX_LINES { ", ..." } else { "" };
            format!("line {}{more}", shown.join(", "))
        };
        parts.push(format!("{name} at {where_}; {fix}"));
    }
    format!(
        "redaction scan found {} (contents withheld)",
        parts.join(" | ")
    )
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

struct Heading {
    level: usize,
    text: String,
    /// Line index of the heading.
    line: usize,
}

fn headings(lines: &[&str]) -> Vec<Heading> {
    let mut in_fence = false;
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence {
            continue;
        }
        let level = l.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&level) && l[level..].starts_with(' ') {
            out.push(Heading {
                level,
                text: l[level..].trim().to_lowercase(),
                line: i,
            });
        }
    }
    out
}

/// Line range of the body of heading `idx`, up to the next heading of the same or higher rank.
fn section_range(hs: &[Heading], idx: usize, total: usize) -> (usize, usize) {
    let end = hs[idx + 1..]
        .iter()
        .find(|h| h.level <= hs[idx].level)
        .map_or(total, |h| h.line);
    (hs[idx].line + 1, end)
}

fn reject(reasons: &mut Vec<Reason>, code: ReasonCode, detail: impl Into<String>) {
    reasons.push(Reason {
        code,
        detail: detail.into(),
    });
}

pub fn valid_slug(slug: &str) -> bool {
    (3..=48).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Returns the title text (without `#`) when the submission is acceptable.
fn validate(ctx: &Ctx, slug: &str, md: &str) -> Result<String, Vec<Reason>> {
    use ReasonCode::*;
    let mut r = Vec::new();
    if !valid_slug(slug) {
        reject(&mut r, BadSlug, "slug must match [a-z0-9-]{3,48}");
    }
    if md.trim().is_empty() {
        reject(&mut r, Empty, "markdown is empty");
        return Err(r);
    }
    if md.len() > MAX_EPIC_BYTES {
        reject(
            &mut r,
            Oversized,
            format!("markdown exceeds {MAX_EPIC_BYTES} bytes"),
        );
        return Err(r);
    }
    if md.contains('\0') {
        reject(&mut r, Forbidden, "NUL byte");
    }
    if md.contains("<!--") {
        reject(
            &mut r,
            HtmlComment,
            "HTML comments are added by the harness only",
        );
    }
    let lines: Vec<&str> = md.lines().collect();

    // Header: title, then `Status: Proposed`.
    let mut it = lines.iter().filter(|l| !l.trim().is_empty());
    let title = it
        .next()
        .and_then(|l| l.strip_prefix("# "))
        .map(|t| t.trim().to_owned())
        .unwrap_or_default();
    let status_ok = it.next().is_some_and(|l| l.trim() == "Status: Proposed");
    if title.is_empty() || !status_ok {
        reject(
            &mut r,
            BadHeader,
            "must begin with '# <title>' then 'Status: Proposed'",
        );
    } else if title.to_lowercase().starts_with("epic ") {
        reject(&mut r, BadHeader, "title must not carry an epic number");
    }

    let hs = headings(&lines);
    let has = |kw: &str| hs.iter().any(|h| h.level >= 2 && h.text.contains(kw));
    for kw in [
        "motivating evidence",
        "observed behavior",
        "problem",
        "out of scope",
        "architecture",
        "alternatives",
        "goal coverage",
        "acceptance criteria",
        "fixture",
        "implementation outline",
        "path inventory",
        "unresolved questions",
    ] {
        if !has(kw) {
            reject(&mut r, MissingSection, format!("missing section: {kw}"));
        }
    }

    check_goals(&lines, &hs, &mut r);
    check_evidence(ctx, md, &mut r);
    check_increment(&lines, &hs, &mut r);
    check_paths(ctx, &lines, &hs, &mut r);

    if ctx.has_unreviewed && !md.to_lowercase().contains("unreviewed") {
        reject(
            &mut r,
            UnlabeledEvidence,
            "evidence includes unreviewed cases; label them 'unreviewed'",
        );
    }
    if ctx.has_ambiguous && !md.to_lowercase().contains("ambiguous") {
        reject(
            &mut r,
            UnlabeledEvidence,
            "evidence includes ambiguous cases; label them 'ambiguous'",
        );
    }

    let kinds = ctx.redactor.scan_prose(md);
    if !kinds.is_empty() {
        reject(
            &mut r,
            Forbidden,
            forbidden_detail(&ctx.redactor, md, &kinds),
        );
    }
    for l in &lines {
        let low = l.to_lowercase();
        let copy = [
            "copy", "import", "commit", "fixture", "check in", "check-in",
        ];
        if low.contains(".datasets") && copy.iter().any(|w| low.contains(w)) {
            reject(
                &mut r,
                DatasetCopy,
                "do not propose using local dataset contents",
            );
            break;
        }
    }
    if r.is_empty() { Ok(title) } else { Err(r) }
}

fn check_goals(lines: &[&str], hs: &[Heading], r: &mut Vec<Reason>) {
    let Some(idx) = hs
        .iter()
        .position(|h| h.level >= 2 && h.text.contains("goal coverage"))
    else {
        return; // reported as a missing section
    };
    let (a, b) = section_range(hs, idx, lines.len());
    let mut seen = BTreeSet::new();
    for l in &lines[a..b] {
        let t = l.trim();
        if !t.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = t.trim_matches('|').split('|').map(str::trim).collect();
        let first = cells[0].replace('*', "");
        let first = first.trim();
        let num = first
            .strip_prefix("Goal ")
            .or_else(|| first.strip_prefix("goal "))
            .unwrap_or(first);
        let Ok(n) = num.trim().parse::<u32>() else {
            continue; // header or separator row
        };
        let filled =
            |c: Option<&&str>| c.is_some_and(|c| !c.is_empty() && !c.trim_matches('-').is_empty());
        if !GOALS.contains(&n) || cells.len() < 3 || !filled(cells.get(1)) || !filled(cells.get(2))
        {
            reject(
                r,
                ReasonCode::BadGoalRow,
                format!("goal table row for '{n}' needs a decision and a reason, goals 1 to 8"),
            );
        } else {
            seen.insert(n);
        }
    }
    for g in GOALS {
        if !seen.contains(&g) {
            reject(
                r,
                ReasonCode::MissingGoal,
                format!("goal {g} missing from the coverage table"),
            );
        }
    }
}

/// Tokens that look like `generation-...` or `evaluation-...` IDs.
fn id_tokens(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    for prefix in ["generation-", "evaluation-"] {
        for (i, _) in md.match_indices(prefix) {
            let before = md[..i].chars().next_back();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                continue;
            }
            let tail = &md[i..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(tail.len());
            out.push(tail[..end].trim_end_matches('-').to_owned());
        }
    }
    out
}

fn check_evidence(ctx: &Ctx, md: &str, r: &mut Vec<Reason>) {
    let tokens = id_tokens(md);
    let mut cases = 0;
    for t in &tokens {
        if *t == ctx.evaluation_id || *t == ctx.generation_id {
            continue;
        }
        if ctx.case_ids.contains(t) {
            cases += 1;
        } else {
            reject(
                r,
                ReasonCode::UnknownEvidenceId,
                format!("unknown evidence ID '{t}'"),
            );
        }
    }
    for (id, label) in [
        (&ctx.evaluation_id, "evaluation"),
        (&ctx.generation_id, "generation"),
    ] {
        if !tokens.contains(id) {
            reject(
                r,
                ReasonCode::MissingEvidenceId,
                format!("the {label} ID must be cited in full"),
            );
        }
    }
    if cases == 0 {
        reject(
            r,
            ReasonCode::MissingEvidenceId,
            "cite at least one existing case ID",
        );
    }
}

fn check_increment(lines: &[&str], hs: &[Heading], r: &mut Vec<Reason>) {
    let chosen = lines
        .iter()
        .filter(|l| {
            l.trim_start_matches(|c: char| "-*> ".contains(c))
                .to_lowercase()
                .starts_with("chosen increment:")
        })
        .count();
    if chosen != 1 {
        reject(
            r,
            ReasonCode::ChosenIncrement,
            format!("declare exactly one 'Chosen increment:' line, found {chosen}"),
        );
    }
    for (idx, h) in hs.iter().enumerate() {
        if !h.text.contains("deferred") {
            continue;
        }
        let (a, b) = section_range(hs, idx, lines.len());
        let body = lines[a..b].join("\n").to_lowercase();
        if [
            "acceptance criteria",
            "implementation outline",
            "implementation step",
        ]
        .iter()
        .any(|k| body.contains(k))
        {
            reject(
                r,
                ReasonCode::DeferredContent,
                "deferred ideas must not carry implementation steps or acceptance criteria",
            );
        }
    }
}

fn looks_like_path(tok: &str) -> bool {
    !tok.is_empty()
        && tok.len() <= 256
        && tok
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
        && (tok.contains('/')
            || tok.starts_with(".env")
            || PATH_EXTENSIONS.iter().any(|e| tok.ends_with(e)))
        && tok.chars().any(|c| c.is_ascii_alphabetic())
}

fn backticked(line: &str) -> Vec<&str> {
    line.split('`').skip(1).step_by(2).collect()
}

fn check_paths(ctx: &Ctx, lines: &[&str], hs: &[Heading], r: &mut Vec<Reason>) {
    use ReasonCode::*;
    let Some(idx) = hs
        .iter()
        .position(|h| h.level >= 2 && h.text.contains("path inventory"))
    else {
        return;
    };
    let (a, b) = section_range(hs, idx, lines.len());
    let mut existing = BTreeSet::new();
    let mut proposed = BTreeSet::new();
    #[derive(PartialEq)]
    enum Cat {
        None,
        Existing,
        Proposed,
    }
    let mut cat = Cat::None;
    for l in &lines[a..b] {
        let low = l.to_lowercase();
        if low.contains("existing evidence") {
            cat = Cat::Existing;
        } else if low.contains("proposed new") {
            cat = Cat::Proposed;
        }
        for tok in backticked(l) {
            if !looks_like_path(tok) {
                continue;
            }
            match cat {
                Cat::None => reject(
                    r,
                    PathInventory,
                    "list paths under 'Existing evidence files' or 'Proposed new files'",
                ),
                Cat::Existing => {
                    existing.insert(tok.to_owned());
                }
                Cat::Proposed => {
                    proposed.insert(tok.to_owned());
                }
            }
        }
    }
    if existing.is_empty() && proposed.is_empty() {
        reject(r, PathInventory, "the path inventory lists no paths");
    }
    for p in existing.intersection(&proposed) {
        reject(
            r,
            PathAmbiguous,
            format!("'{p}' is listed as both existing and proposed"),
        );
    }
    for p in &existing {
        match normalize(p) {
            Ok(c) if is_denied(&c) => reject(r, PathDenied, format!("'{p}' is in a denied root")),
            Ok(c) if allowed_file(&c) => {
                if !exists_in_repo(&ctx.root, &c) {
                    reject(r, PathMissing, format!("existing file '{p}' was not found"));
                }
            }
            _ => reject(r, PathDenied, format!("'{p}' is outside the allowed roots")),
        }
    }
    for p in &proposed {
        match normalize(p) {
            Ok(c) if is_denied(&c) => reject(r, PathDenied, format!("'{p}' is in a denied root")),
            Ok(c) if c.join("/") == *p && owned(&c) => {}
            _ => reject(
                r,
                PathNotOwned,
                format!("'{p}' must be a normalized path inside an owning crate or tools/synth"),
            ),
        }
    }
    // Every other mention anywhere in the document must be inventoried.
    for (i, l) in lines.iter().enumerate() {
        if (a..b).contains(&i) {
            continue;
        }
        for tok in backticked(l) {
            if !looks_like_path(tok) {
                continue;
            }
            if tok.starts_with('/') || tok.starts_with("..") || tok.contains("/../") {
                reject(
                    r,
                    PathDenied,
                    format!("'{tok}' is not a repository-relative path"),
                );
            } else if !existing.contains(tok) && !proposed.contains(tok) {
                reject(
                    r,
                    PathUnlisted,
                    format!("'{tok}' is not in the path inventory"),
                );
            }
        }
    }
}

fn owned(c: &[String]) -> bool {
    c.len() >= 3
        && (matches!(c[0].as_str(), "crates" | "apps")
            || (c[0] == "tools" && c[1] == "synth" && c.len() >= 3))
}

fn exists_in_repo(root: &Path, comps: &[String]) -> bool {
    let Ok(canon) = root.join(comps.iter().collect::<PathBuf>()).canonicalize() else {
        return false;
    };
    let Ok(rel) = canon.strip_prefix(root) else {
        return false;
    };
    let rc: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    allowed_file(&rc) && canon.is_file()
}

// ---------------------------------------------------------------------------
// Allocation and publication
// ---------------------------------------------------------------------------

struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Exclusive repository-scoped lock via no-overwrite creation, with stale-lock recovery.
fn acquire_lock(root: &Path) -> Result<LockGuard, String> {
    let dir = root.join(".baho");
    fs::create_dir_all(&dir).map_err(|e| format!("lock directory: {e}"))?;
    let path = dir.join("epic-submit.lock");
    let start = SystemTime::now();
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                return Ok(LockGuard(path));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let age = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| m.elapsed().ok());
                if age.is_some_and(|a| a > LOCK_STALE) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if start.elapsed().is_ok_and(|e| e > LOCK_WAIT) {
                    return Err("timed out waiting for the epic lock".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(format!("lock: {e}")),
        }
    }
}

fn next_number(epics: &Path) -> Result<u32, String> {
    let mut max = 0;
    for entry in fs::read_dir(epics).map_err(|e| format!("epics directory: {e}"))? {
        let name = entry.map_err(|e| e.to_string())?.file_name();
        let name = name.to_string_lossy();
        let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
        if digits.len() >= 3
            && name[digits.len()..].starts_with('-')
            && name.ends_with(".md")
            && let Ok(n) = digits.parse::<u32>()
        {
            max = max.max(n);
        }
    }
    max.checked_add(1)
        .ok_or_else(|| "epic numbers exhausted".into())
}

fn render_document(ctx: &Ctx, number: u32, title: &str, md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 256);
    let mut header_done = false;
    let mut title_done = false;
    for line in md.lines() {
        if !title_done && !line.trim().is_empty() {
            out.push_str(&format!("# Epic {number:03}: {title}\n"));
            title_done = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if title_done && !header_done && line.trim() == "Status: Proposed" {
            out.push_str(&format!(
                "\n<!-- Generated by Agent 2 (epic planner); evaluation: {}; review: unreviewed -->\n",
                ctx.evaluation_id
            ));
            header_done = true;
        }
    }
    out
}

fn publish(ctx: &Ctx, slug: &str, title: &str, md: &str) -> Result<Accepted, String> {
    let epics = ctx.root.join("epics");
    fs::create_dir_all(&epics).map_err(|e| format!("epics directory: {e}"))?;
    let _lock = acquire_lock(&ctx.root)?;
    // A collision with a writer outside the lock requires fresh allocation, never overwrite.
    for _ in 0..16 {
        let number = next_number(&epics)?;
        let name = format!("{number:03}-{slug}.md");
        let text = render_document(ctx, number, title, md);
        let tmp = epics.join(format!(".{name}.{}.tmp", std::process::id()));
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| format!("temporary file: {e}"))?;
            f.write_all(text.as_bytes())
                .and_then(|()| f.sync_all())
                .map_err(|e| format!("write: {e}"))?;
        }
        // hard_link fails when the destination exists, so publication is atomic and
        // no-overwrite; readers never see a partial file.
        let linked = fs::hard_link(&tmp, epics.join(&name));
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => {
                return Ok(Accepted {
                    number,
                    relative_path: format!("epics/{name}"),
                });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("publish: {e}")),
        }
    }
    Err("could not allocate an epic number".into())
}

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SubmitEpic(pub EpicSubmitter);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitEpicArgs {
    pub slug: String,
    pub markdown: String,
}

impl PortableTool for SubmitEpic {
    const NAME: &'static str = "submit_epic";
    type Args = SubmitEpicArgs;
    type Output = Value;
    type Error = ToolError;
    fn map_error(&self, error: ToolError) -> ToolExecutionError {
        ToolExecutionError::other(error.to_string()).with_code("harness_rejection")
    }
    fn description(&self) -> String {
        "Submit the one proposed epic: a slug ([a-z0-9-]{3,48}) and the Markdown body. The \
         harness validates it, assigns the number and file name, and writes it. Rejections list \
         reasons so you can fix and resubmit; stop after an accepted submission."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "slug":{"type":"string","pattern":"^[a-z0-9-]{3,48}$"},
            "markdown":{"type":"string","maxLength":MAX_EPIC_BYTES}
        },"required":["slug","markdown"],"additionalProperties":false})
    }
    async fn call(&self, args: SubmitEpicArgs) -> Result<Value, ToolError> {
        match self.0.submit(&args.slug, &args.markdown) {
            Ok(a) => Ok(json!({"accepted": true, "path": a.relative_path, "number": a.number})),
            Err(e) => Err(ToolError(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::*;
    use tempfile::TempDir;

    const GEN: &str = "generation-1791425654630970000-3920";
    const EVAL: &str = "evaluation-1791425654630970000-3920-0";

    fn report() -> EvaluationReport {
        let case = |i: usize| CaseReport {
            case_id: format!("{GEN}-{i:04}"),
            candidate_hash: "ab".repeat(32),
            prompt: "p".into(),
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

    fn repo() -> (TempDir, EpicSubmitter) {
        repo_with(&["Senior Analyst"])
    }

    fn repo_with(literals: &[&str]) -> (TempDir, EpicSubmitter) {
        let dir = tempfile::tempdir().unwrap();
        let w = |rel: &str, body: &str| {
            let p = dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        };
        w("epics/001-a.md", "# a\n");
        w("epics/003-b.md", "# b\n");
        w("epics/notes.md", "not numbered\n");
        w("crates/a/src/lib.rs", "fn x() {}\n");
        w(".env", "K=1\n");
        w(".datasets/d.csv", "x\n");
        let s =
            EpicSubmitter::new(dir.path(), &report(), Redactor::new(literals).unwrap()).unwrap();
        (dir, s)
    }

    fn doc(extra: &str) -> String {
        format!(
            "# Parse refusals for polite lead-ins\n\nStatus: Proposed\n\n\
## Motivating evidence and user intent\nRun `{EVAL}` from `{GEN}`; case {GEN}-0001 is unreviewed.\n\n\
## Observed behavior\nRefused.\n\n## Problem and desired behavior\nx\n\n## Out of scope\nx\n\n\
## Architecture and crate ownership\nx\n\n## Design and alternatives\nChosen increment: normalize lead-ins.\n\n\
## Goal coverage\n\n| Goal | Decision | Reason |\n|---|---|---|\n{}\n\
## Acceptance criteria\nx\n\n## Fixture and test strategy\nx\n\n## Implementation outline\nx\n\n\
## Path inventory\n\nExisting evidence files:\n- `crates/a/src/lib.rs`\n\nProposed new files:\n- `crates/a/tests/lead_in.rs`\n\n\
## Unresolved questions\nNone.\n{extra}",
            (1..=8)
                .map(|g| format!("| {g} | not affected | reason {g} |\n"))
                .collect::<String>()
        )
    }

    fn codes(r: Result<Accepted, SubmitError>) -> Vec<ReasonCode> {
        match r {
            Err(SubmitError::Rejected(v)) => v.into_iter().map(|r| r.code).collect(),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    fn epics_len(dir: &TempDir) -> usize {
        fs::read_dir(dir.path().join("epics")).unwrap().count()
    }

    #[test]
    fn valid_submission_is_numbered_titled_and_stamped() {
        let (dir, s) = repo();
        let a = s.submit("polite-lead-ins", &doc("")).unwrap();
        assert_eq!(a.relative_path, "epics/004-polite-lead-ins.md");
        let text = fs::read_to_string(dir.path().join(&a.relative_path)).unwrap();
        assert!(text.starts_with("# Epic 004: Parse refusals for polite lead-ins\n"));
        assert!(text.contains("Status: Proposed\n\n<!-- Generated by Agent 2"));
        assert!(text.contains(EVAL) && text.contains("review: unreviewed"));
        assert_eq!(s.accepted(), Some(a));
        assert!(
            !dir.path()
                .join("epics")
                .read_dir()
                .unwrap()
                .any(|e| { e.unwrap().file_name().to_string_lossy().ends_with(".tmp") })
        );
    }

    #[test]
    fn second_accept_is_refused_and_nothing_else_is_written() {
        let (dir, s) = repo();
        s.submit("first-one", &doc("")).unwrap();
        let n = epics_len(&dir);
        assert!(matches!(
            s.submit("second-one", &doc("")),
            Err(SubmitError::AlreadyAccepted)
        ));
        assert_eq!(epics_len(&dir), n);
    }

    #[test]
    fn rejection_writes_nothing_and_is_recorded_then_correction_succeeds() {
        let (dir, s) = repo();
        let n = epics_len(&dir);
        let bad = doc("").replace("## Out of scope", "## Elsewhere");
        assert!(codes(s.submit("good-slug", &bad)).contains(&ReasonCode::MissingSection));
        assert_eq!(epics_len(&dir), n);
        assert_eq!(s.rejections()[0].codes, vec![ReasonCode::MissingSection]);
        assert!(s.submit("good-slug", &doc("")).is_ok());
    }

    #[test]
    fn bad_slugs_and_oversized_bodies_are_rejected() {
        let (_d, s) = repo();
        for slug in ["ab", "Bad_Slug", "../x", &"a".repeat(49)] {
            assert!(
                codes(s.submit(slug, &doc(""))).contains(&ReasonCode::BadSlug),
                "{slug}"
            );
        }
        let big = doc(&"x".repeat(MAX_EPIC_BYTES));
        assert_eq!(
            codes(s.submit("good-slug", &big)),
            vec![ReasonCode::Oversized]
        );
    }

    #[test]
    fn header_and_comment_rules() {
        let (_d, s) = repo();
        let no_status = doc("").replace("Status: Proposed", "Status: Accepted");
        assert!(codes(s.submit("good-slug", &no_status)).contains(&ReasonCode::BadHeader));
        let numbered = doc("").replace("# Parse", "# Epic 9: Parse");
        assert!(codes(s.submit("good-slug", &numbered)).contains(&ReasonCode::BadHeader));
        assert!(
            codes(s.submit("good-slug", &doc("<!-- x -->"))).contains(&ReasonCode::HtmlComment)
        );
    }

    #[test]
    fn goal_table_must_cover_one_to_eight() {
        let (_d, s) = repo();
        let missing = doc("").replace("| 5 | not affected | reason 5 |\n", "");
        assert_eq!(
            codes(s.submit("good-slug", &missing)),
            vec![ReasonCode::MissingGoal]
        );
        let empty_reason = doc("").replace("reason 2", "");
        assert!(codes(s.submit("good-slug", &empty_reason)).contains(&ReasonCode::BadGoalRow));
        let nine = doc("").replace("| 8 |", "| 9 |");
        let c = codes(s.submit("good-slug", &nine));
        assert!(c.contains(&ReasonCode::BadGoalRow) && c.contains(&ReasonCode::MissingGoal));
    }

    #[test]
    fn evidence_ids_must_exist_and_be_cited() {
        let (_d, s) = repo();
        let unknown = doc(&format!("See {GEN}-0099 too.\n"));
        assert_eq!(
            codes(s.submit("good-slug", &unknown)),
            vec![ReasonCode::UnknownEvidenceId]
        );
        let no_case = doc("").replace(&format!("{GEN}-0001"), "a case");
        assert_eq!(
            codes(s.submit("good-slug", &no_case)),
            vec![ReasonCode::MissingEvidenceId]
        );
        let no_eval = doc("").replace(EVAL, "the evaluation");
        assert_eq!(
            codes(s.submit("good-slug", &no_eval)),
            vec![ReasonCode::MissingEvidenceId]
        );
    }

    #[test]
    fn exactly_one_increment_and_deferred_has_no_steps() {
        let (_d, s) = repo();
        let two = doc("Chosen increment: another.\n");
        assert_eq!(
            codes(s.submit("good-slug", &two)),
            vec![ReasonCode::ChosenIncrement]
        );
        let none = doc("").replace("Chosen increment:", "Increment:");
        assert_eq!(
            codes(s.submit("good-slug", &none)),
            vec![ReasonCode::ChosenIncrement]
        );
        let deferred = doc("\n## Deferred ideas\nSort by.\n### Acceptance criteria\n- it sorts\n");
        assert_eq!(
            codes(s.submit("good-slug", &deferred)),
            vec![ReasonCode::DeferredContent]
        );
        assert!(
            s.submit("good-slug", &doc("\n## Deferred ideas\nSort by.\n"))
                .is_ok()
        );
    }

    #[test]
    fn unreviewed_evidence_must_be_labeled() {
        let (_d, s) = repo();
        let unlabeled = doc("").replace("is unreviewed", "is fine");
        assert_eq!(
            codes(s.submit("good-slug", &unlabeled)),
            vec![ReasonCode::UnlabeledEvidence]
        );
    }

    #[test]
    fn path_inventory_rules() {
        let (_d, s) = repo();
        let unlisted = doc("See `crates/a/src/other.rs`.\n");
        assert_eq!(
            codes(s.submit("good-slug", &unlisted)),
            vec![ReasonCode::PathUnlisted]
        );

        let missing = doc("").replace(
            "Existing evidence files:\n",
            "Existing evidence files:\n- `crates/a/src/gone.rs`\n",
        );
        assert_eq!(
            codes(s.submit("good-slug", &missing)),
            vec![ReasonCode::PathMissing]
        );

        for denied in [
            ".env",
            ".datasets/d.csv",
            "../x/y.rs",
            "/etc/passwd.txt",
            "target/x.rs",
        ] {
            let d = doc("").replace(
                "Existing evidence files:\n",
                &format!("Existing evidence files:\n- `{denied}`\n"),
            );
            assert!(
                codes(repo().1.submit("good-slug", &d))
                    .iter()
                    .any(|c| matches!(c, ReasonCode::PathDenied | ReasonCode::Forbidden)),
                "{denied}"
            );
            let p = doc("").replace(
                "Proposed new files:\n",
                &format!("Proposed new files:\n- `{denied}`\n"),
            );
            assert!(
                codes(repo().1.submit("good-slug", &p))
                    .iter()
                    .any(|c| matches!(
                        c,
                        ReasonCode::PathDenied | ReasonCode::PathNotOwned | ReasonCode::Forbidden
                    )),
                "{denied}"
            );
        }

        let outside_owner = doc("").replace(
            "Proposed new files:\n",
            "Proposed new files:\n- `docs/new.md`\n",
        );
        assert_eq!(
            codes(s.submit("good-slug", &outside_owner)),
            vec![ReasonCode::PathNotOwned]
        );

        let both = doc("").replace(
            "Proposed new files:\n",
            "Proposed new files:\n- `crates/a/src/lib.rs`\n",
        );
        assert_eq!(
            codes(s.submit("good-slug", &both)),
            vec![ReasonCode::PathAmbiguous]
        );

        let uncategorized = doc("").replace("Existing evidence files:\n", "");
        assert!(codes(s.submit("good-slug", &uncategorized)).contains(&ReasonCode::PathInventory));
    }

    #[test]
    fn symlinked_existing_file_cannot_escape_roots() {
        let (dir, s) = repo();
        std::os::unix::fs::symlink(
            dir.path().join(".env"),
            dir.path().join("crates/a/src/link.rs"),
        )
        .unwrap();
        let d = doc("").replace(
            "Existing evidence files:\n",
            "Existing evidence files:\n- `crates/a/src/link.rs`\n",
        );
        assert_eq!(
            codes(s.submit("good-slug", &d)),
            vec![ReasonCode::PathMissing]
        );
    }

    #[test]
    fn forbidden_content_is_rejected_without_echoing_it() {
        let (_d, s) = repo();
        for extra in [
            "Role is Senior Analyst.\n",
            "Key sk-abcdefghijklmnopqrstuvwxyz0123456789.\n",
            "Look in /Users/me/project.\n",
        ] {
            let err = s.submit("good-slug", &doc(extra)).unwrap_err().to_string();
            assert!(err.contains("Forbidden"), "{extra}");
            assert!(
                !err.contains("Senior Analyst")
                    && !err.contains("sk-abc")
                    && !err.contains("/Users/me")
            );
        }
        let copy = doc("Copy rows from .datasets/ into a fixture.\n");
        assert!(codes(s.submit("good-slug", &copy)).contains(&ReasonCode::DatasetCopy));
    }

    #[test]
    fn recorded_reasons_name_kind_and_line_but_never_the_offending_text() {
        let (_d, s) = repo();
        let bad = doc(
            "Role is Senior Analyst.\nKey sk-abcdefghijklmnopqrstuvwxyz0123456789.\nSee /Users/me/project.\n",
        );
        let total = bad.lines().count();
        s.submit("good-slug", &bad).unwrap_err();
        let rec = &s.rejections()[0];
        assert_eq!(rec.codes, vec![ReasonCode::Forbidden]);
        assert_eq!(rec.reasons.len(), 1);
        let msg = &rec.reasons[0].message;
        for kind in ["protected source value", "absolute", "credential-shaped"] {
            assert!(msg.contains(kind), "{msg}");
        }
        // Lines are 1-based; the three offending lines are the last three of the document.
        for n in total - 2..=total {
            assert!(msg.contains(&n.to_string()), "{msg}");
        }
        let json = serde_json::to_string(rec).unwrap();
        for leaked in ["Senior Analyst", "sk-abc", "/Users/me"] {
            assert!(!json.contains(leaked) && !msg.contains(leaked));
        }
    }

    #[test]
    fn recorded_reasons_cover_every_returned_code_in_order() {
        let (_d, s) = repo();
        let bad = doc("").replace("## Out of scope", "## Elsewhere");
        s.submit("Bad_Slug", &bad).unwrap_err();
        let rec = &s.rejections()[0];
        assert_eq!(
            rec.codes,
            rec.reasons.iter().map(|r| r.code).collect::<Vec<_>>()
        );
        assert!(rec.reasons.iter().all(|r| !r.message.is_empty()));
        assert!(
            rec.reasons
                .iter()
                .all(|r| r.message.chars().count() <= MAX_RECORDED_REASON_CHARS)
        );
    }

    #[test]
    fn ordinary_words_containing_a_short_profiled_value_are_not_forbidden() {
        // Synthetic stand-in for a profiled value that is a common word fragment.
        let (_d, s) = repo_with(&["Admin"]);
        let prose = "Administrator rights and administration are discussed; mismanaged.\n";
        assert!(
            s.submit("good-slug", &doc(prose)).is_ok(),
            "{:?}",
            s.rejections()
        );
    }

    #[test]
    fn whole_word_profiled_values_are_still_forbidden() {
        for text in [
            "Rows for Admin.\n",
            "Rows for admins.\n",
            "Rows for (ADMIN).\n",
            "Rows for the_admin role.\n",
            "Rows for admin's team.\n",
            "Rows for admin-only.\n",
        ] {
            let (_d, s) = repo_with(&["Admin"]);
            assert!(
                codes(s.submit("good-slug", &doc(text))).contains(&ReasonCode::Forbidden),
                "{text}"
            );
        }
    }

    #[test]
    fn attempt_budget_is_bounded() {
        let (_d, s) = repo();
        for _ in 0..MAX_SUBMISSIONS {
            let _ = s.submit("bad", "");
        }
        assert!(matches!(
            s.submit("good-slug", &doc("")),
            Err(SubmitError::AttemptsExhausted)
        ));
    }

    #[test]
    fn concurrent_submitters_get_distinct_numbers_and_complete_files() {
        let (dir, first) = repo();
        let root = dir.path().to_owned();
        let mut handles = Vec::new();
        for i in 0..8 {
            let root = root.clone();
            handles.push(std::thread::spawn(move || {
                let s = EpicSubmitter::new(
                    &root,
                    &report(),
                    Redactor::new(["Senior Analyst"]).unwrap(),
                )
                .unwrap();
                s.submit(&format!("race-{i}"), &doc("")).unwrap()
            }));
        }
        let mut numbers: Vec<u32> = handles
            .into_iter()
            .map(|h| h.join().unwrap().number)
            .collect();
        numbers.sort();
        assert_eq!(numbers, (4..12).collect::<Vec<_>>());
        for entry in fs::read_dir(root.join("epics")).unwrap() {
            let p = entry.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"), "{name}");
            if name.starts_with("race-") || name[4..].starts_with("race-") {
                assert!(fs::read_to_string(&p).unwrap().ends_with("None.\n"));
            }
        }
        drop(first);
    }

    #[test]
    fn existing_files_are_never_overwritten_and_stale_locks_recover() {
        let (dir, s) = repo();
        // An unnumbered or foreign file with the would-be name is untouched.
        fs::write(dir.path().join("epics/004-keep.md"), "mine\n").unwrap();
        let a = s.submit("keep", &doc("")).unwrap();
        assert_eq!(a.number, 5);
        assert_eq!(
            fs::read_to_string(dir.path().join("epics/004-keep.md")).unwrap(),
            "mine\n"
        );

        let (dir2, s2) = repo();
        fs::create_dir_all(dir2.path().join(".baho")).unwrap();
        let lock = dir2.path().join(".baho/epic-submit.lock");
        fs::write(&lock, "999\n").unwrap();
        let old = SystemTime::now() - Duration::from_secs(600);
        fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(s2.submit("after-stale", &doc("")).is_ok());
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn tool_accepts_once_and_reports_reasons() {
        let (_d, s) = repo();
        let tool = SubmitEpic(s);
        let bad = tool
            .call(SubmitEpicArgs {
                slug: "x".into(),
                markdown: doc(""),
            })
            .await
            .unwrap_err();
        assert!(bad.to_string().contains("BadSlug"));
        let ok = tool
            .call(SubmitEpicArgs {
                slug: "fine-slug".into(),
                markdown: doc(""),
            })
            .await
            .unwrap();
        assert_eq!(ok["accepted"], true);
        let again = tool
            .call(SubmitEpicArgs {
                slug: "fine-slug".into(),
                markdown: doc(""),
            })
            .await
            .unwrap_err();
        assert!(again.to_string().contains("already accepted"));
    }
}
