//! `submit_epic`: the only write path of the epic planner (Epic 016, T6).
//!
//! The agent supplies a slug, Markdown, and versioned proposal evidence. The harness validates the whole submission,
//! chooses the file name, allocates the number under a repository-scoped lock, and publishes
//! the complete file with no-overwrite semantics. Rejected submissions write nothing.

use crate::{
    contracts::{EvaluationReport, EvidenceEligibility, ReviewStatus},
    planner_tools::{ToolError, allowed_file, is_denied, normalize},
    redaction::{Redactor, ViolationKind},
};
use rig::tool::{PortableTool, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    EvidenceEligibility,
    SubmissionVersion,
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
    BadCitation,
    MissingCitation,
    QuestionFormat,
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
    eligibility: BTreeMap<String, EvidenceEligibility>,
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
            eligibility: report
                .cases
                .iter()
                .map(|c| (c.case_id.clone(), report.evidence_eligibility(c)))
                .collect(),
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

    #[cfg(test)]
    fn submit(&self, slug: &str, markdown: &str) -> Result<Accepted, SubmitError> {
        self.submit_proposal(
            slug,
            markdown,
            &ProposalEvidence {
                schema_version: SUBMISSION_SCHEMA_VERSION,
                kind: ProposalKind::Investigation,
                motivating_case_ids: vec![format!("{}-0001", self.0.generation_id)],
            },
        )
    }

    /// Validate a draft with the publication contract and shared attempt budget, without writing.
    pub fn validate_proposal(
        &self,
        slug: &str,
        markdown: &str,
        evidence: &ProposalEvidence,
    ) -> Result<(), SubmitError> {
        let ctx = &*self.0;
        let mut state = ctx.state.lock().unwrap();
        // Keep one attempt available for publishing a reviewed draft.
        if state.accepted.is_none() && state.attempts >= MAX_SUBMISSIONS - 1 {
            return Err(SubmitError::AttemptsExhausted);
        }
        validate_attempt(ctx, &mut state, slug, markdown, evidence)?;
        Ok(())
    }

    pub fn submit_proposal(
        &self,
        slug: &str,
        markdown: &str,
        evidence: &ProposalEvidence,
    ) -> Result<Accepted, SubmitError> {
        let ctx = &*self.0;
        // Held for the whole call: in-process submissions are serialized, and a second
        // acceptance is impossible.
        let mut state = ctx.state.lock().unwrap();
        let title = validate_attempt(ctx, &mut state, slug, markdown, evidence)?;
        let accepted =
            publish(ctx, slug, &title, markdown, evidence).map_err(SubmitError::Publish)?;
        state.accepted = Some(accepted.clone());
        Ok(accepted)
    }
}

/// Share rejection records and attempt accounting between drafts and publication.
fn validate_attempt(
    ctx: &Ctx,
    state: &mut State,
    slug: &str,
    markdown: &str,
    evidence: &ProposalEvidence,
) -> Result<String, SubmitError> {
    if state.accepted.is_some() {
        return Err(SubmitError::AlreadyAccepted);
    }
    if state.attempts >= MAX_SUBMISSIONS {
        return Err(SubmitError::AttemptsExhausted);
    }
    state.attempts += 1;
    validate(ctx, slug, markdown, evidence).map_err(|mut reasons| {
        reasons.sort_by_key(|r| r.code);
        for reason in &mut reasons {
            reason.detail = ctx.redactor.sanitize_prose(&reason.detail);
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
        SubmitError::Rejected(reasons)
    })
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
fn validate(
    ctx: &Ctx,
    slug: &str,
    md: &str,
    evidence: &ProposalEvidence,
) -> Result<String, Vec<Reason>> {
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
        "risks",
        "implementation outline",
        "path inventory",
        "unresolved questions",
    ] {
        if !has(kw) {
            reject(&mut r, MissingSection, format!("missing section: {kw}"));
        }
    }

    check_eligibility(ctx, md, evidence, &mut r);
    check_goals(&lines, &hs, &mut r);
    check_evidence(ctx, md, &mut r);
    check_increment(&lines, &hs, &mut r);
    check_paths(ctx, &lines, &hs, &mut r);
    check_questions(&lines, &hs, &mut r);

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

fn check_eligibility(ctx: &Ctx, md: &str, evidence: &ProposalEvidence, reasons: &mut Vec<Reason>) {
    if evidence.schema_version != SUBMISSION_SCHEMA_VERSION {
        reject(
            reasons,
            ReasonCode::SubmissionVersion,
            "unsupported proposal evidence schema version; expected 1",
        );
    }
    let ids: BTreeSet<_> = evidence.motivating_case_ids.iter().collect();
    if ids.is_empty() || ids.len() != evidence.motivating_case_ids.len() || ids.len() > 100 {
        reject(
            reasons,
            ReasonCode::EvidenceEligibility,
            "name 1 to 100 unique motivating case IDs",
        );
    }
    let citations = id_tokens(md);
    for id in ids {
        match ctx.eligibility.get(id) {
            None => reject(
                reasons,
                ReasonCode::UnknownEvidenceId,
                format!("unknown motivating case ID '{id}'"),
            ),
            Some(EvidenceEligibility::Observation)
                if evidence.kind == ProposalKind::Implementation =>
            {
                reject(
                    reasons,
                    ReasonCode::EvidenceEligibility,
                    format!(
                        "case '{id}' is observation-only; use investigation or finish_no_epic until human review and supporting evidence establish a wrong plan or refusal"
                    ),
                );
            }
            _ => {}
        }
        if !citations.contains(id) {
            reject(
                reasons,
                ReasonCode::MissingEvidenceId,
                format!("motivating case '{id}' must be cited in the Markdown"),
            );
        }
    }
    let expected = format!("Proposal kind: {}", evidence.kind.as_str());
    let declarations: Vec<_> = md
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("Proposal kind:"))
        .collect();
    if declarations != [expected.as_str()] {
        reject(
            reasons,
            ReasonCode::EvidenceEligibility,
            format!("declare '{expected}' to match the structured proposal kind"),
        );
    }
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
        // The decision must be a verdict; a bare "Decision" says nothing a reviewer can check.
        let verdict = cells
            .get(1)
            .map(|c| c.replace('*', "").trim().to_lowercase());
        let has_verdict = verdict
            .as_deref()
            .is_some_and(|v| v.starts_with("affected") || v.starts_with("not affected"));
        if !GOALS.contains(&n)
            || cells.len() < 3
            || !filled(cells.get(1))
            || !filled(cells.get(2))
            || !has_verdict
        {
            reject(
                r,
                ReasonCode::BadGoalRow,
                format!(
                    "goal table row for '{n}' needs a decision starting with 'affected' or 'not affected', and a reason; goals 1 to 8"
                ),
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
    check_citations(ctx, lines, &existing, r);
}

/// Minimum verified `path:line` citations. A claim about code that cites no line cannot be
/// checked by a reviewer, and the harness cannot confirm a citation it never sees.
const MIN_CITATIONS: usize = 3;

/// Parse a backticked `path:N` or `path:N-M` citation.
fn parse_citation(tok: &str) -> Option<(&str, usize, usize)> {
    let (path, range) = tok.rsplit_once(':')?;
    if !looks_like_path(path) {
        return None;
    }
    let (a, b) = match range.split_once('-') {
        Some((a, b)) => (a, b),
        None => (range, range),
    };
    let (a, b) = (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?);
    Some((path, a, b))
}

/// Every `path:line` citation must name an inventoried existing file and a line range inside it.
fn check_citations(ctx: &Ctx, lines: &[&str], existing: &BTreeSet<String>, r: &mut Vec<Reason>) {
    let mut valid = 0;
    for l in lines {
        for tok in backticked(l) {
            let Some((path, a, b)) = parse_citation(tok) else {
                continue;
            };
            let total = normalize(path)
                .ok()
                .filter(|c| exists_in_repo(&ctx.root, c))
                .and_then(|_| fs::read_to_string(ctx.root.join(path)).ok())
                .map(|text| text.lines().count());
            match total {
                _ if !existing.contains(path) => reject(
                    r,
                    ReasonCode::BadCitation,
                    format!("citation '{tok}': '{path}' is not under 'Existing evidence files'"),
                ),
                Some(n) if a >= 1 && a <= b && b <= n => valid += 1,
                Some(n) => reject(
                    r,
                    ReasonCode::BadCitation,
                    format!("citation '{tok}': lines must satisfy 1 <= start <= end <= {n}"),
                ),
                None => reject(
                    r,
                    ReasonCode::BadCitation,
                    format!("citation '{tok}': file could not be read"),
                ),
            }
        }
    }
    if valid < MIN_CITATIONS {
        reject(
            r,
            ReasonCode::MissingCitation,
            format!(
                "cite at least {MIN_CITATIONS} verified code locations as `path:line` or \
                 `path:start-end`; found {valid}. Use search_text/read_file line numbers"
            ),
        );
    }
}

const MAX_QUESTIONS: usize = 4;

/// Unresolved questions are product decisions for the user, not design work left undone.
/// Each must carry the agent's recommended default so the reviewer can accept or override it.
fn check_questions(lines: &[&str], hs: &[Heading], r: &mut Vec<Reason>) {
    let Some(idx) = hs
        .iter()
        .position(|h| h.level >= 2 && h.text.contains("unresolved questions"))
    else {
        return;
    };
    let (a, b) = section_range(hs, idx, lines.len());
    let items: Vec<&str> = lines[a..b]
        .iter()
        .map(|l| l.trim_start())
        .filter(|l| {
            l.starts_with("- ")
                || l.starts_with("* ")
                || l.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
        .collect();
    if items.len() > MAX_QUESTIONS {
        reject(
            r,
            ReasonCode::QuestionFormat,
            format!(
                "at most {MAX_QUESTIONS} unresolved questions; settle design choices in the \
                 design and keep only decisions the user must make"
            ),
        );
    }
    if items
        .iter()
        .any(|q| !q.to_lowercase().contains("recommend"))
    {
        reject(
            r,
            ReasonCode::QuestionFormat,
            "every unresolved question must state a 'Recommended:' default",
        );
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

fn render_document(
    ctx: &Ctx,
    number: u32,
    title: &str,
    md: &str,
    evidence: &ProposalEvidence,
) -> String {
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
            out.push_str(&format!(
                "<!-- Proposal evidence schema: {}; kind: {}; motivating cases: {} -->\n",
                evidence.schema_version,
                evidence.kind.as_str(),
                evidence.motivating_case_ids.join(", ")
            ));
            header_done = true;
        }
    }
    out
}

fn publish(
    ctx: &Ctx,
    slug: &str,
    title: &str,
    md: &str,
    evidence: &ProposalEvidence,
) -> Result<Accepted, String> {
    let epics = ctx.root.join("epics");
    fs::create_dir_all(&epics).map_err(|e| format!("epics directory: {e}"))?;
    let _lock = acquire_lock(&ctx.root)?;
    // A collision with a writer outside the lock requires fresh allocation, never overwrite.
    for _ in 0..16 {
        let number = next_number(&epics)?;
        let name = format!("{number:03}-{slug}.md");
        let text = render_document(ctx, number, title, md, evidence);
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

pub const SUBMISSION_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Implementation,
    Investigation,
}

impl ProposalKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Implementation => "implementation",
            Self::Investigation => "investigation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalEvidence {
    pub schema_version: u32,
    pub kind: ProposalKind,
    pub motivating_case_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitEpicArgs {
    pub slug: String,
    pub markdown: String,
    pub evidence: ProposalEvidence,
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
        "Submit one proposed epic: slug, Markdown, and versioned evidence (kind and motivating case IDs). The \
         harness validates it, assigns the number and file name, and writes it. Rejections list \
         reasons so you can fix and resubmit; stop after an accepted submission."
            .into()
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{
            "slug":{"type":"string","pattern":"^[a-z0-9-]{3,48}$"},
            "markdown":{"type":"string","maxLength":MAX_EPIC_BYTES},
            "evidence":{"type":"object","properties":{
                "schema_version":{"type":"integer","enum":[SUBMISSION_SCHEMA_VERSION]},
                "kind":{"type":"string","enum":["implementation","investigation"]},
                "motivating_case_ids":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":100,"uniqueItems":true}
            },"required":["schema_version","kind","motivating_case_ids"],"additionalProperties":false}
        },"required":["slug","markdown","evidence"],"additionalProperties":false})
    }
    async fn call(&self, args: SubmitEpicArgs) -> Result<Value, ToolError> {
        match self
            .0
            .submit_proposal(&args.slug, &args.markdown, &args.evidence)
        {
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
        w(
            "crates/a/src/lib.rs",
            "fn x() {}\nfn y() {}\nfn z() {}\nfn w() {}\n",
        );
        w(".env", "K=1\n");
        w(".datasets/d.csv", "x\n");
        let s =
            EpicSubmitter::new(dir.path(), &report(), Redactor::new(literals).unwrap()).unwrap();
        (dir, s)
    }

    fn doc(extra: &str) -> String {
        format!(
            "# Parse refusals for polite lead-ins\n\nStatus: Proposed\n\nProposal kind: investigation\n\n\
## Motivating evidence and user intent\nRun `{EVAL}` from `{GEN}`; case {GEN}-0001 is unreviewed.\n\n\
## Observed behavior\nRefused (`crates/a/src/lib.rs:1`, `crates/a/src/lib.rs:2-3`, `crates/a/src/lib.rs:4`).\n\n## Problem and desired behavior\nx\n\n## Out of scope\nx\n\n\
## Architecture and crate ownership\nx\n\n## Design and alternatives\nChosen increment: normalize lead-ins.\n\n\
## Goal coverage\n\n| Goal | Decision | Reason |\n|---|---|---|\n{}\n\
## Acceptance criteria\nx\n\n## Fixture and test strategy\nx\n\n## Risks and regressions\nx\n\n## Implementation outline\nx\n\n\
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

    fn proposal(kind: ProposalKind) -> ProposalEvidence {
        ProposalEvidence {
            schema_version: 1,
            kind,
            motivating_case_ids: vec![format!("{GEN}-0001")],
        }
    }

    #[test]
    fn implementation_gate_requires_review_and_supporting_evidence() {
        for review in [
            ReviewStatus::Unreviewed,
            ReviewStatus::Ambiguous,
            ReviewStatus::Rejected,
            ReviewStatus::Confirmed,
        ] {
            let (dir, _) = repo();
            let mut report = report();
            let case = &mut report.cases[1];
            case.review = review;
            case.observed = ObservedResult::RowsMatch;
            case.plan_comparison = PlanComparison::Different;
            case.evidence.plan = Some(PlanEvidence {
                column: "Role".into(),
                operator: PlanOperator::Equal,
                literal: PlanLiteral::Text("is Analyst".into()),
                text_match_policy: TextMatchPolicy::UnicodeLowercase,
            });
            let s =
                EpicSubmitter::new(dir.path(), &report, Redactor::new([] as [&str; 0]).unwrap())
                    .unwrap();
            let md = doc("").replace(
                "Proposal kind: investigation",
                "Proposal kind: implementation",
            );
            let result = s.submit_proposal(
                "word-operators",
                &md,
                &proposal(ProposalKind::Implementation),
            );
            if review == ReviewStatus::Confirmed {
                let a = result.unwrap();
                let published = fs::read_to_string(dir.path().join(a.relative_path)).unwrap();
                assert!(published.contains("Proposal evidence schema: 1; kind: implementation"));
            } else {
                assert!(codes(result).contains(&ReasonCode::EvidenceEligibility));
                assert_eq!(epics_len(&dir), 3);
                assert!(
                    s.submit_proposal(
                        "investigate-operators",
                        &doc("Ambiguous cases are observations."),
                        &proposal(ProposalKind::Investigation)
                    )
                    .is_ok()
                );
            }
        }
    }

    #[test]
    fn eligibility_frozen_cases_cover_missing_evidence_and_mapping_limits() {
        let mut report = report();
        report.cases[1].review = ReviewStatus::Confirmed;
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::Observation
        );
        report.cases[1].evidence.refusal_reason = Some("unsupported syntax".into());
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::ConfirmedRefusal
        );
        report.cases[1].observed = ObservedResult::Clarification;
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::Observation
        );
        report.cases[1].observed = ObservedResult::Refused;
        report.cases[1].evidence.spawn_failure = Some("spawn_failed".into());
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::Observation
        );
        report.cases[1].evidence.spawn_failure = None;
        report.cases[1].evidence.oracle_code = Some("oracle_mismatch".into());
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::Observation
        );
        report.cases[1].evidence.oracle_code = None;
        report.cases[1].prompt = "Role is Analyst".into();
        report
            .limitations
            .push(crate::redaction::MappingLimitation {
                stand_in: "Unrelated".into(),
                kind: crate::redaction::LimitationKind::Punctuation,
            });
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::ConfirmedRefusal
        );
        report.limitations[0].stand_in = "Analyst".into();
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::Observation
        );
        report.limitations.clear();
        report.cases[1].observed = ObservedResult::WrongRows;
        report.cases[1].plan_comparison = PlanComparison::Different;
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::Observation
        );
        report.cases[1].plan_comparison = PlanComparison::Equivalent;
        assert_eq!(
            report.evidence_eligibility(&report.cases[1]),
            EvidenceEligibility::Observation
        );
    }

    #[test]
    fn supporting_observations_are_allowed_but_cannot_be_promoted_to_motivators() {
        let (dir, _) = repo();
        let mut report = report();
        report.cases[1].review = ReviewStatus::Confirmed;
        report.cases[1].evidence.refusal_reason = Some("unsupported syntax".into());
        let s = EpicSubmitter::new(dir.path(), &report, Redactor::new([] as [&str; 0]).unwrap())
            .unwrap();
        let md = doc(&format!(
            "Case {GEN}-0002 is an unreviewed supporting observation."
        ))
        .replace(
            "Proposal kind: investigation",
            "Proposal kind: implementation",
        );
        let mut evidence = proposal(ProposalKind::Implementation);
        evidence.motivating_case_ids.push(format!("{GEN}-0002"));
        assert!(
            codes(s.submit_proposal("extend-coverage", &md, &evidence))
                .contains(&ReasonCode::EvidenceEligibility)
        );
        evidence.motivating_case_ids.pop();
        assert!(s.submit_proposal("extend-coverage", &md, &evidence).is_ok());
    }

    #[test]
    fn structured_evidence_version_ids_and_kind_are_checked() {
        let (_dir, s) = repo();
        let mut evidence = proposal(ProposalKind::Investigation);
        evidence.schema_version = 2;
        assert!(
            codes(s.submit_proposal("good-slug", &doc(""), &evidence))
                .contains(&ReasonCode::SubmissionVersion)
        );
        evidence.schema_version = 1;
        evidence.motivating_case_ids.clear();
        assert!(
            codes(s.submit_proposal("good-slug", &doc(""), &evidence))
                .contains(&ReasonCode::EvidenceEligibility)
        );
        evidence.motivating_case_ids = vec![format!("{GEN}-9999")];
        assert!(
            codes(s.submit_proposal("good-slug", &doc(""), &evidence))
                .contains(&ReasonCode::UnknownEvidenceId)
        );
        evidence = proposal(ProposalKind::Investigation);
        evidence
            .motivating_case_ids
            .push(evidence.motivating_case_ids[0].clone());
        assert!(
            codes(s.submit_proposal("good-slug", &doc(""), &evidence))
                .contains(&ReasonCode::EvidenceEligibility)
        );
        evidence = proposal(ProposalKind::Investigation);
        let md = doc("").replace(
            "Proposal kind: investigation",
            "Proposal kind: implementation",
        );
        assert!(
            codes(s.submit_proposal("good-slug", &md, &evidence))
                .contains(&ReasonCode::EvidenceEligibility)
        );
        assert!(
            serde_json::from_value::<SubmitEpicArgs>(json!({"slug":"old-call","markdown":doc("")}))
                .is_err()
        );
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
    fn citations_must_resolve_and_be_numerous_enough() {
        let (_d, s) = repo();
        let bad_line = doc("").replace("lib.rs:4`", "lib.rs:99`");
        assert!(codes(s.submit("good-slug", &bad_line)).contains(&ReasonCode::BadCitation));
        let few = doc("").replace(", `crates/a/src/lib.rs:2-3`, `crates/a/src/lib.rs:4`", "");
        assert!(codes(s.submit("good-slug", &few)).contains(&ReasonCode::MissingCitation));
        let unlisted = doc("").replace("lib.rs:4`", "lib.rs:4`, `epics/001-a.md:1`");
        assert!(codes(s.submit("good-slug", &unlisted)).contains(&ReasonCode::BadCitation));
    }

    #[test]
    fn goal_decision_must_be_a_verdict() {
        let (_d, s) = repo();
        let vague = doc("").replace("| 3 | not affected |", "| 3 | Decision |");
        assert!(codes(s.submit("good-slug", &vague)).contains(&ReasonCode::BadGoalRow));
        let ok = doc("").replace("| 3 | not affected |", "| 3 | **Affected**: maps words |");
        assert!(s.submit("good-slug", &ok).is_ok());
    }

    #[test]
    fn risks_section_is_required() {
        let (_d, s) = repo();
        let bad = doc("").replace("## Risks and regressions", "## Elsewhere");
        assert!(codes(s.submit("good-slug", &bad)).contains(&ReasonCode::MissingSection));
    }

    #[test]
    fn unresolved_questions_are_bounded_and_carry_a_recommendation() {
        let (_d, s) = repo();
        let unrecommended = doc("").replace("None.", "1. Should we do X?");
        assert!(codes(s.submit("good-slug", &unrecommended)).contains(&ReasonCode::QuestionFormat));
        let many = doc("").replace(
            "None.",
            &(1..=5)
                .map(|i| format!("{i}. Q? Recommended: yes.\n"))
                .collect::<String>(),
        );
        assert!(codes(s.submit("good-slug", &many)).contains(&ReasonCode::QuestionFormat));
        let ok = doc("").replace("None.", "1. Q? Recommended: yes.");
        assert!(s.submit("good-slug", &ok).is_ok());
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
            vec![ReasonCode::MissingEvidenceId, ReasonCode::MissingEvidenceId]
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
                evidence: proposal(ProposalKind::Investigation),
            })
            .await
            .unwrap_err();
        assert!(bad.to_string().contains("BadSlug"));
        let ok = tool
            .call(SubmitEpicArgs {
                slug: "fine-slug".into(),
                markdown: doc(""),
                evidence: proposal(ProposalKind::Investigation),
            })
            .await
            .unwrap();
        assert_eq!(ok["accepted"], true);
        let again = tool
            .call(SubmitEpicArgs {
                slug: "fine-slug".into(),
                markdown: doc(""),
                evidence: proposal(ProposalKind::Investigation),
            })
            .await
            .unwrap_err();
        assert!(again.to_string().contains("already accepted"));
    }
    #[tokio::test]
    async fn draft_validation_has_no_publication_or_allocation() {
        use crate::draft_review::{DraftProposal, SubmitDraft};
        let (dir, submitter) = repo();
        let n = epics_len(&dir);
        let tool = SubmitDraft::new(submitter.clone());
        let draft = DraftProposal {
            slug: "draft-only".into(),
            markdown: doc(""),
            evidence: proposal(ProposalKind::Investigation),
        };
        let mut bad = draft.clone();
        bad.markdown = "invalid".into();
        assert!(tool.call(bad).await.is_err());
        assert_eq!(submitter.rejections().len(), 1);
        assert!(tool.call(draft.clone()).await.is_ok());
        assert_eq!(tool.accepted(), Some(draft.clone()));
        assert!(tool.call(draft).await.is_err());
        assert_eq!(epics_len(&dir), n);
        assert!(!dir.path().join(".baho").exists());
        assert!(submitter.accepted().is_none());
    }

    #[tokio::test]
    async fn semantic_review_is_safe_hash_bound_and_checks_citations() {
        use crate::{
            draft_review::*,
            planner_tools::{ToolLimits, Toolbox},
        };
        let (dir, _) = repo();
        let redactor = Redactor::new(["Senior Analyst"]).unwrap();
        let toolbox = Toolbox::new(
            dir.path(),
            &report(),
            redactor.clone(),
            ToolLimits::default(),
        )
        .unwrap();
        let draft = DraftProposal {
            slug: "draft-only".into(),
            markdown: doc(""),
            evidence: proposal(ProposalKind::Investigation),
        };
        let tool = SubmitReview::new(&draft, redactor.clone(), toolbox.clone());
        let pass = Review {
            schema_version: 1,
            draft_hash: draft_hash(&draft),
            verdict: ReviewVerdict::Pass,
            findings: vec![],
            summary: "No blocking plan issues found.".into(),
        };
        let mut bad = pass.clone();
        bad.draft_hash = "00".repeat(32);
        assert!(tool.call(bad).await.is_err());
        let mut bad = pass.clone();
        bad.summary = "Senior Analyst".into();
        let error = tool.call(bad).await.unwrap_err().to_string();
        assert!(!error.contains("Senior Analyst"));
        let mut bad = pass.clone();
        bad.verdict = ReviewVerdict::Revise;
        assert!(tool.call(bad).await.is_err());
        let finding = ReviewFinding {
            issue: "Fixture does not distinguish the binding rules.".into(),
            evidence: vec![ReviewCitation {
                path: "crates/a/src/lib.rs".into(),
                start_line: 1,
                end_line: 2,
            }],
            requested_correction: "Add a distinguishing synthetic fixture.".into(),
        };
        let mut bad = pass.clone();
        bad.findings.push(finding.clone());
        assert!(tool.call(bad).await.is_err());
        let mut revise = pass.clone();
        revise.verdict = ReviewVerdict::Revise;
        revise.findings.push(finding);
        let mut bad = revise.clone();
        bad.findings[0].evidence[0].end_line = 99;
        assert!(tool.call(bad).await.is_err());
        let mut bad = revise.clone();
        bad.findings[0].evidence[0].path = ".env".into();
        assert!(tool.call(bad).await.is_err());
        assert!(tool.call(revise.clone()).await.is_ok());
        assert_eq!(tool.accepted(), Some(revise));
        assert_eq!(tool.rejections().len(), 6);
        assert!(
            tool.rejections()
                .iter()
                .all(|r| !r.contains("Senior Analyst") && r.chars().count() <= 400)
        );
        assert!(tool.call(pass.clone()).await.is_err());
        let fresh = SubmitReview::new(&draft, redactor, toolbox);
        assert!(fresh.call(pass).await.is_ok());
        assert!(!dir.path().join(".baho").exists());
        let mut changed = draft.clone();
        changed.markdown.push('\n');
        assert_ne!(draft_hash(&draft), draft_hash(&changed));
    }
    #[test]
    fn last_draft_attempt_reserves_publication() {
        let (dir, submitter) = repo();
        let evidence = proposal(ProposalKind::Investigation);
        for _ in 0..MAX_SUBMISSIONS - 2 {
            assert!(submitter.validate_proposal("bad", "", &evidence).is_err());
        }
        assert!(
            submitter
                .validate_proposal("last-draft", &doc(""), &evidence)
                .is_ok()
        );
        assert!(matches!(
            submitter.validate_proposal("extra-draft", &doc(""), &evidence),
            Err(SubmitError::AttemptsExhausted)
        ));
        assert!(!dir.path().join(".baho").exists());
        assert!(
            submitter
                .submit_proposal("last-draft", &doc(""), &evidence)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn draft_scope_drift_is_rejected_before_acceptance_and_can_be_corrected() {
        use crate::draft_review::{DraftProposal, SubmitDraft};
        let (_dir, submitter) = repo();
        let expected = proposal(ProposalKind::Investigation);
        let tool = SubmitDraft::with_evidence(submitter, expected.clone());
        let draft = DraftProposal {
            slug: "bound-draft".into(),
            markdown: doc(""),
            evidence: expected,
        };
        let mut bad = draft.clone();
        bad.evidence.kind = ProposalKind::Implementation;
        assert!(tool.call(bad).await.is_err());
        let mut bad = draft.clone();
        bad.evidence.motivating_case_ids = vec![format!("{GEN}-0002")];
        assert!(tool.call(bad).await.is_err());
        let mut bad = draft.clone();
        bad.evidence.schema_version = 2;
        assert!(tool.call(bad).await.is_err());
        assert!(tool.accepted().is_none());
        assert_eq!(tool.rejections().len(), 3);
        assert!(tool.call(draft).await.is_ok());
        assert!(tool.accepted().is_some());
        assert!(tool.rejections().iter().all(|r| r.chars().count() <= 400));
    }
}
