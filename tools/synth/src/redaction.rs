//! Neutral-mapping redaction for model-visible surfaces (Epic 016, T3).
//!
//! The evaluator builds one consistent mapping from profiled `Role` values to stand-ins:
//! repeated values share a stand-in, case variants share a base and keep their case shape,
//! and word count is preserved. The mapping lives only in a private sidecar; scanners
//! report violation kinds and never echo what they matched.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::contracts::{
    ContractError, EvaluationReport, PlanLiteral, RedactionSidecar, SIDECAR_SCHEMA_VERSION,
    SidecarEntry,
};

const STAND_IN_PREFIX: &str = "zz";
const MAX_STAND_IN_ATTEMPTS: usize = 10_000;
/// Words that collide with the prompt/SQL vocabulary; a literal using one can change how a
/// prompt parses, so its stand-in is flagged rather than trusted.
const KEYWORDS: &[&str] = &[
    "show", "keep", "where", "rows", "records", "role", "equals", "and", "or", "not", "is", "the",
    "for", "with", "matches", "select", "from", "please", "return", "all", "only",
];
const PLACEHOLDER_SECRET: &str = "[secret]";
const PLACEHOLDER_PATH: &str = "[path]";
const SECRET_KEY_WORDS: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
    "credential",
];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RedactionError {
    #[error("{0}")]
    Contract(#[from] ContractError),
    #[error("no safe stand-in could be generated")]
    Unsatisfiable,
    #[error("inconsistent redaction mapping: {0}")]
    Inconsistent(&'static str),
    #[error("surface still contains protected content: {0:?}")]
    Unsafe(Vec<ViolationKind>),
    #[error("redaction sidecar I/O failed: {0}")]
    Io(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    SourceLiteral,
    AbsolutePath,
    Credential,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitationKind {
    Punctuation,
    NonAscii,
    MixedCase,
    KeywordCollision,
}

/// A property of the source literal a stand-in may not preserve. Carries the stand-in only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappingLimitation {
    pub stand_in: String,
    pub kind: LimitationKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Lower,
    Upper,
    Title,
    Mixed,
}

fn shape_of(text: &str) -> Shape {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if !letters.iter().any(|c| c.is_uppercase()) {
        return Shape::Lower;
    }
    if letters.len() >= 2 && !letters.iter().any(|c| c.is_lowercase()) {
        return Shape::Upper;
    }
    let title = text.split_whitespace().all(|w| {
        let mut cs = w.chars().filter(|c| c.is_alphabetic());
        match cs.next() {
            Some(first) => first.is_uppercase() && cs.all(|c| c.is_lowercase()),
            None => true,
        }
    });
    if title { Shape::Title } else { Shape::Mixed }
}

fn apply_shape(shape: Shape, words: &[String]) -> String {
    let word = |w: &String| match shape {
        Shape::Lower | Shape::Mixed => w.clone(),
        Shape::Upper => w.to_uppercase(),
        Shape::Title => {
            let mut cs = w.chars();
            cs.next()
                .map(|f| f.to_uppercase().chain(cs).collect())
                .unwrap_or_default()
        }
    };
    words.iter().map(word).collect::<Vec<_>>().join(" ")
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn fold_str(s: &str) -> Vec<char> {
    s.chars().map(fold).collect()
}

fn letters(mut n: usize) -> String {
    let mut out = Vec::new();
    loop {
        out.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
        if n == 0 {
            break;
        }
    }
    while out.len() < 2 {
        out.push('a');
    }
    out.iter().rev().collect()
}

#[derive(Clone, Debug)]
struct Group {
    key: Vec<char>,
    base: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Redactor {
    /// Longest key first, so overlapping literals resolve to the longest match.
    groups: Vec<Group>,
    entries: Vec<SidecarEntry>,
}

fn contains_seq(haystack: &[char], needle: &[char]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

impl Redactor {
    /// Builds a deterministic mapping from the distinct profiled literals.
    pub fn new<I, S>(literals: I) -> Result<Self, RedactionError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let sources: BTreeSet<String> = literals
            .into_iter()
            .map(|s| s.as_ref().to_owned())
            .filter(|s| !s.trim().is_empty())
            .collect();
        let keys: BTreeSet<Vec<char>> = sources.iter().map(|s| fold_str(s)).collect();
        let mut counter = 0usize;
        let mut groups = Vec::new();
        for key in &keys {
            let word_count = sources
                .iter()
                .find(|s| &fold_str(s) == key)
                .map(|s| s.split_whitespace().count())
                .unwrap_or(1)
                .max(1);
            let mut base = Vec::new();
            let mut attempts = 0;
            while base.len() < word_count {
                attempts += 1;
                if attempts > MAX_STAND_IN_ATTEMPTS {
                    return Err(RedactionError::Unsatisfiable);
                }
                let word = format!("{STAND_IN_PREFIX}{}", letters(counter));
                counter += 1;
                let folded = fold_str(&word);
                if keys.iter().any(|k| contains_seq(&folded, k)) {
                    continue;
                }
                base.push(word);
            }
            groups.push(Group {
                key: key.clone(),
                base,
            });
        }
        Self::assemble(groups, Some(sources))
    }

    fn assemble(
        mut groups: Vec<Group>,
        sources: Option<BTreeSet<String>>,
    ) -> Result<Self, RedactionError> {
        // Joined stand-ins are checked as one string so a literal cannot straddle words.
        for g in &groups {
            let joined = fold_str(&g.base.join(" "));
            if groups.iter().any(|o| contains_seq(&joined, &o.key)) {
                return Err(RedactionError::Inconsistent(
                    "stand-in contains a source literal",
                ));
            }
        }
        let mut entries = Vec::new();
        if let Some(sources) = sources {
            for source in sources {
                let key = fold_str(&source);
                let group = groups.iter().find(|g| g.key == key).expect("group exists");
                entries.push(SidecarEntry {
                    stand_in: apply_shape(shape_of(&source), &group.base),
                    source,
                });
            }
        }
        groups.sort_by(|a, b| {
            b.key
                .len()
                .cmp(&a.key.len())
                .then_with(|| a.key.cmp(&b.key))
        });
        Ok(Self { groups, entries })
    }

    pub fn from_sidecar(sidecar: &RedactionSidecar) -> Result<Self, RedactionError> {
        if sidecar.schema_version != SIDECAR_SCHEMA_VERSION {
            return Err(ContractError::Version {
                what: "redaction sidecar",
                found: sidecar.schema_version,
                expected: SIDECAR_SCHEMA_VERSION,
            }
            .into());
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut sources = BTreeSet::new();
        for entry in &sidecar.entries {
            if entry.source.trim().is_empty() || entry.stand_in.trim().is_empty() {
                return Err(RedactionError::Inconsistent("empty mapping entry"));
            }
            let key = fold_str(&entry.source);
            let base: Vec<String> = entry
                .stand_in
                .split_whitespace()
                .map(|w| w.to_lowercase())
                .collect();
            match groups.iter().find(|g| g.key == key) {
                Some(g) if g.base != base => {
                    return Err(RedactionError::Inconsistent(
                        "case variants disagree on a stand-in",
                    ));
                }
                Some(_) => {}
                None => groups.push(Group { key, base }),
            }
            sources.insert(entry.source.clone());
        }
        Self::assemble(groups, Some(sources))
    }

    pub fn to_sidecar(&self, evaluation_hash: &str) -> RedactionSidecar {
        RedactionSidecar {
            schema_version: SIDECAR_SCHEMA_VERSION,
            evaluation_hash: evaluation_hash.to_owned(),
            entries: self.entries.clone(),
        }
    }

    /// Reports what the mapping cannot preserve, deterministically ordered. A person must
    /// treat conclusions drawn from these stand-ins as unsupported.
    pub fn limitations(&self) -> Vec<MappingLimitation> {
        let mut out = BTreeSet::new();
        for e in &self.entries {
            let mut add = |kind| {
                out.insert((e.stand_in.clone(), kind));
            };
            if !e.source.is_ascii() {
                add(LimitationKind::NonAscii);
            }
            if e.source
                .chars()
                .any(|c| !c.is_alphanumeric() && !c.is_whitespace())
            {
                add(LimitationKind::Punctuation);
            }
            if shape_of(&e.source) == Shape::Mixed {
                add(LimitationKind::MixedCase);
            }
            if e.source
                .split_whitespace()
                .any(|w| KEYWORDS.contains(&w.to_lowercase().as_str()))
            {
                add(LimitationKind::KeywordCollision);
            }
        }
        out.into_iter()
            .map(|(stand_in, kind)| MappingLimitation { stand_in, kind })
            .collect()
    }

    fn replace_literals(&self, text: &str) -> String {
        self.replace_literals_with(text, false)
    }

    fn replace_literals_with(&self, text: &str, whole_word: bool) -> String {
        let chars: Vec<char> = text.chars().collect();
        let folded: Vec<char> = chars.iter().map(|c| fold(*c)).collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        'scan: while i < chars.len() {
            for g in &self.groups {
                let end = i + g.key.len();
                let bounded = !whole_word
                    || ((i == 0 || !folded[i - 1].is_alphanumeric())
                        && (end >= folded.len() || !folded[end].is_alphanumeric()));
                if end <= chars.len() && folded[i..end] == g.key[..] && bounded {
                    let matched: String = chars[i..end].iter().collect();
                    out.push_str(&apply_shape(shape_of(&matched), &g.base));
                    i = end;
                    continue 'scan;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    fn has_literal(&self, text: &str) -> bool {
        let folded = fold_str(text);
        self.groups.iter().any(|g| contains_seq(&folded, &g.key))
    }

    /// Removes literals, absolute paths, and credential-shaped values.
    pub fn sanitize(&self, text: &str) -> String {
        self.replace_literals(&sanitize_tokens(text))
    }

    /// [`Redactor::sanitize`] for prose: literals are replaced only as whole words, so
    /// ordinary words that merely contain a short literal stay readable. Pair with
    /// [`Redactor::scan_prose`].
    pub fn sanitize_prose(&self, text: &str) -> String {
        self.replace_literals_with(&sanitize_tokens(text), true)
    }

    /// Violation kinds present in `text`, without the matched content.
    pub fn scan(&self, text: &str) -> Vec<ViolationKind> {
        let mut kinds: BTreeSet<ViolationKind> = scan_tokens(text).into_iter().collect();
        if self.has_literal(text) {
            kinds.insert(ViolationKind::SourceLiteral);
        }
        kinds.into_iter().collect()
    }

    /// Like [`Redactor::scan`], but a profiled literal counts only as a whole word (a plural
    /// `s`/`es` is still the literal). For free prose the agent writes itself, where a short
    /// literal is routinely a substring of ordinary words; `scan` keeps the strict substring
    /// rule for surfaces that must never carry even a fragment.
    pub fn scan_prose(&self, text: &str) -> Vec<ViolationKind> {
        let mut kinds: BTreeSet<ViolationKind> = scan_tokens(text).into_iter().collect();
        if self.has_whole_word_literal(text) {
            kinds.insert(ViolationKind::SourceLiteral);
        }
        kinds.into_iter().collect()
    }

    fn has_whole_word_literal(&self, text: &str) -> bool {
        let folded = fold_str(text);
        let word = |i: usize| folded.get(i).is_some_and(|c| c.is_alphanumeric());
        self.groups.iter().any(|g| {
            let n = g.key.len();
            n > 0
                && folded.windows(n).enumerate().any(|(i, w)| {
                    if w != &g.key[..] || (i > 0 && word(i - 1)) {
                        return false;
                    }
                    let end = i + n;
                    let plural = |suffix: &str| {
                        let k = suffix.chars().count();
                        folded.iter().skip(end).take(k).copied().eq(suffix.chars())
                            && !word(end + k)
                    };
                    !word(end) || plural("s") || plural("es")
                })
        })
    }

    /// Sanitizes, then rescans; a surface that cannot be made safe is rejected.
    pub fn sanitize_checked(&self, text: &str) -> Result<String, RedactionError> {
        let clean = self.sanitize(text);
        let remaining = self.scan(&clean);
        if remaining.is_empty() {
            Ok(clean)
        } else {
            Err(RedactionError::Unsafe(remaining))
        }
    }

    /// Sanitizes every free-text field of a report; stable codes are scanned, not rewritten.
    pub fn sanitize_report(
        &self,
        report: &EvaluationReport,
    ) -> Result<EvaluationReport, RedactionError> {
        let mut out = report.clone();
        for case in &mut out.cases {
            case.prompt = self.sanitize_checked(&case.prompt)?;
            if let Some(r) = &case.review_rationale {
                case.review_rationale = Some(self.sanitize_checked(r)?);
            }
            for code in case
                .diagnostic_codes
                .iter()
                .chain(case.evidence.oracle_code.iter())
                .chain(case.evidence.run_id.iter())
            {
                let remaining = self.scan(code);
                if !remaining.is_empty() {
                    return Err(RedactionError::Unsafe(remaining));
                }
            }
            let ev = &mut case.evidence;
            if let Some(r) = &ev.refusal_reason {
                ev.refusal_reason = Some(self.sanitize_checked(r)?);
            }
            for m in &mut ev.diagnostic_messages {
                *m = self.sanitize_checked(m)?;
            }
            if let Some(plan) = &mut ev.plan {
                plan.column = self.sanitize_checked(&plan.column)?;
                if let PlanLiteral::Text(t) = &plan.literal {
                    plan.literal = PlanLiteral::Text(self.sanitize_checked(t)?);
                }
            }
        }
        for note in &mut out.notes {
            *note = self.sanitize_checked(note)?;
        }
        out.provenance.binary_version = self.sanitize_checked(&out.provenance.binary_version)?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Paths and credentials
// ---------------------------------------------------------------------------

const EDGE: &[char] = &['"', '\'', '(', ')', '{', '}', '<', '>', ',', ';', '`'];

fn is_absolute_path(core: &str) -> bool {
    let bytes = core.as_bytes();
    // `//` and `///` are Rust comment markers, not rooted paths.
    (core.starts_with('/') && !core.starts_with("//") && core[1..].contains('/'))
        || core.starts_with("~/")
        || core.starts_with("file://")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/'))
}

fn is_secret_shaped(core: &str) -> bool {
    let long_token = core.len() >= 32
        && core
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+/_-=".contains(c))
        && core.chars().any(|c| c.is_ascii_digit())
        && core.chars().any(|c| c.is_ascii_alphabetic())
        && !core.chars().all(|c| c.is_ascii_hexdigit());
    let known_prefix = ["sk-", "ghp_", "gho_", "xoxb-", "xoxp-"]
        .iter()
        .any(|p| core.starts_with(p) && core.len() >= p.len() + 12);
    long_token || known_prefix
}

fn secret_key(name: &str) -> bool {
    let lower = name.to_lowercase();
    SECRET_KEY_WORDS.iter().any(|w| lower.contains(w))
}

/// Splits `core` into an optional `name=` prefix and the value being judged.
fn split_assignment(core: &str) -> (&str, &str) {
    match core.split_once('=') {
        Some((name, value)) if !name.is_empty() => (&core[..name.len() + 1], value),
        _ => ("", core),
    }
}

/// Harness-issued `generation-<nanos>-<pid>[-<n>]` and `evaluation-<nanos>-<pid>[-<n>]` IDs
/// are long and mixed but carry no secret: only digits and dashes follow the prefix.
fn is_harness_id(core: &str) -> bool {
    ["generation-", "evaluation-"].iter().any(|p| {
        core.strip_prefix(p).is_some_and(|rest| {
            rest.chars().next().is_some_and(|c| c.is_ascii_digit())
                && rest.chars().all(|c| c.is_ascii_digit() || c == '-')
        })
    })
}

/// A word that introduces a credential in `Authorization:` headers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lead {
    Bearer,
    Basic,
}

/// `Basic` is also an ordinary English word, so it only introduces a credential when the
/// next token looks like encoded data (a digit, base64 punctuation, or long).
fn credential_after(lead: Lead, value: &str) -> bool {
    match lead {
        Lead::Bearer => !value.is_empty(),
        Lead::Basic => {
            value.len() >= 16
                || value
                    .chars()
                    .any(|c| c.is_ascii_digit() || "+/=".contains(c))
        }
    }
}

fn token_violation(core: &str, after: Option<Lead>) -> Option<ViolationKind> {
    if is_harness_id(core) {
        return None;
    }
    if core.contains(PLACEHOLDER_SECRET) || core.contains(PLACEHOLDER_PATH) {
        return None;
    }
    if after.is_some_and(|lead| credential_after(lead, core)) {
        return Some(ViolationKind::Credential);
    }
    let (prefix, value) = split_assignment(core);
    if !prefix.is_empty() && secret_key(&prefix[..prefix.len() - 1]) && !value.is_empty() {
        return Some(ViolationKind::Credential);
    }
    if is_secret_shaped(value) {
        return Some(ViolationKind::Credential);
    }
    if is_absolute_path(value) {
        return Some(ViolationKind::AbsolutePath);
    }
    None
}

fn lead_of(core: &str) -> Option<Lead> {
    if core.eq_ignore_ascii_case("bearer") {
        Some(Lead::Bearer)
    } else if core.eq_ignore_ascii_case("basic") {
        Some(Lead::Basic)
    } else {
        None
    }
}

/// Visits each whitespace-delimited token as `(leading, core, trailing)` slices.
fn for_each_token<'a>(text: &'a str, mut visit: impl FnMut(&'a str, &'a str, &'a str)) {
    let mut start = 0;
    let mut in_token = false;
    let mut spans = Vec::new();
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if in_token {
                spans.push((start, i));
                in_token = false;
            }
        } else if !in_token {
            start = i;
            in_token = true;
        }
    }
    if in_token {
        spans.push((start, text.len()));
    }
    for (s, e) in spans {
        let token = &text[s..e];
        let trimmed_start = token.trim_start_matches(EDGE);
        let lead_len = token.len() - trimmed_start.len();
        let core_end = trimmed_start
            .trim_end_matches(|c: char| EDGE.contains(&c) || c == '.' || c == ':')
            .len();
        visit(
            &token[..lead_len],
            &trimmed_start[..core_end],
            &trimmed_start[core_end..],
        );
    }
}

fn scan_tokens(text: &str) -> Vec<ViolationKind> {
    let mut found = Vec::new();
    let mut lead = None;
    for_each_token(text, |_, core, _| {
        if let Some(kind) = token_violation(core, lead) {
            found.push(kind);
        }
        lead = lead_of(core);
    });
    found
}

fn sanitize_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut after = None;
    for_each_token(text, |lead, core, trail| {
        let lead_at = lead.as_ptr() as usize - text.as_ptr() as usize;
        out.push_str(&text[cursor..lead_at]);
        out.push_str(lead);
        match token_violation(core, after) {
            Some(ViolationKind::Credential) => {
                let (prefix, _) = split_assignment(core);
                let prefix = if after.is_some() { "" } else { prefix };
                out.push_str(prefix);
                out.push_str("[secret]");
            }
            Some(_) => {
                let (prefix, _) = split_assignment(core);
                out.push_str(prefix);
                out.push_str("[path]");
            }
            None => out.push_str(core),
        }
        out.push_str(trail);
        cursor = lead_at + lead.len() + core.len() + trail.len();
        after = lead_of(core);
    });
    out.push_str(&text[cursor..]);
    out
}

// ---------------------------------------------------------------------------
// Private sidecar file
// ---------------------------------------------------------------------------

/// Writes the sidecar owner-only and refuses to overwrite an existing file.
pub fn write_sidecar(path: &Path, sidecar: &RedactionSidecar) -> Result<(), RedactionError> {
    let io = |e: std::io::Error| RedactionError::Io(e.kind().to_string());
    let bytes =
        serde_json::to_vec_pretty(sidecar).map_err(|e| RedactionError::Io(e.to_string()))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(io)?;
    file.write_all(&bytes).map_err(io)
}

/// Loads a sidecar; fails before any model call if it is absent or not for this evaluation.
pub fn load_sidecar(path: &Path, evaluation_hash: &str) -> Result<Redactor, RedactionError> {
    let bytes = std::fs::read(path).map_err(|e| RedactionError::Io(e.kind().to_string()))?;
    let sidecar = RedactionSidecar::parse_for(&bytes, evaluation_hash)?;
    Redactor::from_sidecar(&sidecar)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor::new([
            "Senior Analyst",
            "senior analyst",
            "SENIOR ANALYST",
            "Manager",
        ])
        .unwrap()
    }

    #[test]
    fn variants_share_a_base_and_keep_case_shape_and_word_count() {
        let r = redactor();
        let map: std::collections::HashMap<_, _> = r
            .entries
            .iter()
            .map(|e| (e.source.as_str(), e.stand_in.as_str()))
            .collect();
        let lower = map["senior analyst"];
        assert_eq!(lower.split_whitespace().count(), 2);
        assert_eq!(map["SENIOR ANALYST"], lower.to_uppercase());
        assert_eq!(map["Senior Analyst"].to_lowercase(), lower);
        assert!(map["Senior Analyst"].starts_with("Zz"));
        assert_ne!(map["Manager"].to_lowercase(), lower);
        assert_eq!(map["Manager"].split_whitespace().count(), 1);
    }

    #[test]
    fn mapping_is_deterministic_and_round_trips_through_the_sidecar() {
        let a = redactor();
        let b = Redactor::new([
            "Manager",
            "SENIOR ANALYST",
            "Senior Analyst",
            "senior analyst",
        ])
        .unwrap();
        assert_eq!(a.entries, b.entries);
        let sidecar = a.to_sidecar("eval1");
        let loaded = Redactor::from_sidecar(&sidecar).unwrap();
        let text = "Show rows where Role = Senior Analyst or MANAGER";
        assert_eq!(loaded.sanitize(text), a.sanitize(text));
    }

    #[test]
    fn sanitize_covers_prompts_diagnostics_and_unlisted_case_variants() {
        let r = redactor();
        let out =
            r.sanitize("I'd like rows where Role equals 'Senior Analyst', please. sEnIoR aNaLySt!");
        assert!(r.scan(&out).is_empty(), "{out}");
        assert!(!out.to_lowercase().contains("analyst"));
        assert!(out.contains("Role equals"));
        let diag = r.sanitize("column not found for term 'manager role'");
        assert!(!diag.to_lowercase().contains("manager"));
        // Same literal, same stand-in everywhere.
        assert_eq!(r.sanitize("Manager and Manager"), {
            let one = r.sanitize("Manager");
            format!("{one} and {one}")
        });
    }

    #[test]
    fn scan_reports_kinds_without_content() {
        let r = redactor();
        assert_eq!(r.scan("a Manager"), [ViolationKind::SourceLiteral]);
        assert_eq!(
            r.scan("see /Users/someone/data.csv now"),
            [ViolationKind::AbsolutePath]
        );
        assert_eq!(r.scan("read ~/x/y"), [ViolationKind::AbsolutePath]);
        assert_eq!(r.scan("XIAOMI_API_KEY=abc123"), [ViolationKind::Credential]);
        assert_eq!(
            r.scan("Authorization: Bearer abcdef"),
            [ViolationKind::Credential]
        );
        assert_eq!(
            r.scan("key sk-abcdefghijklmnopqrstuvwx"),
            [ViolationKind::Credential]
        );
        assert!(
            r.scan("crates/baho-core/src/lib.rs and/or hashes")
                .is_empty()
        );
        let hash = "a".repeat(64);
        assert!(r.scan(&hash).is_empty());
    }

    #[test]
    fn prose_scan_uses_whole_words_but_strict_scan_does_not() {
        let r = Redactor::new(["Admin"]).unwrap();
        assert!(r.scan_prose("administration and badminton").is_empty());
        assert!(!r.scan("administration").is_empty());
        assert_eq!(r.scan_prose("the admin"), [ViolationKind::SourceLiteral]);
        assert_eq!(r.scan_prose("Admins."), [ViolationKind::SourceLiteral]);
        assert_eq!(r.scan_prose("ADMIN_ROLE"), [ViolationKind::SourceLiteral]);
        let out = r.sanitize_prose("administration vs Admin");
        assert!(out.starts_with("administration vs "), "{out}");
        assert!(!out.to_lowercase().ends_with("admin"));
    }

    #[test]
    fn basic_as_an_english_word_is_not_a_credential_lead() {
        let r = redactor();
        assert!(r.scan("basic headers and a Basic plan").is_empty());
        assert_eq!(
            r.scan("Authorization: Basic dXNlcjpwYXNz1234="),
            [ViolationKind::Credential]
        );
    }

    #[test]
    fn sanitize_strips_paths_and_credentials() {
        let r = redactor();
        let out = r.sanitize(
            "failed at (/Users/someone/proj/a.csv): XIAOMI_API_KEY=abc and Bearer tok123, Manager",
        );
        assert!(r.scan(&out).is_empty(), "{out}");
        assert!(out.contains("[path]") && out.contains("[secret]"));
        assert!(out.contains("XIAOMI_API_KEY="));
        assert!(!out.contains("someone") && !out.contains("tok123"));
        assert!(r.sanitize_checked("plain text").is_ok());
    }

    #[test]
    fn limitations_are_flagged_without_source_text() {
        let r = Redactor::new(["R&D Lead", "Show Host", "Café", "McDonald", "plain"]).unwrap();
        let lims = r.limitations();
        let kinds: BTreeSet<_> = lims.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LimitationKind::Punctuation));
        assert!(kinds.contains(&LimitationKind::KeywordCollision));
        assert!(kinds.contains(&LimitationKind::NonAscii));
        assert!(kinds.contains(&LimitationKind::MixedCase));
        let json = serde_json::to_string(&lims).unwrap();
        assert!(!json.to_lowercase().contains("lead"));
        assert!(
            !lims
                .iter()
                .any(|l| l.stand_in.eq_ignore_ascii_case("plain"))
        );
    }

    #[test]
    fn stand_ins_never_contain_other_literals() {
        // A one-letter literal cannot be hidden behind letter stand-ins.
        assert_eq!(
            Redactor::new(["z"]).unwrap_err(),
            RedactionError::Unsatisfiable
        );
        let r = Redactor::new(["zzaa", "zzab"]).unwrap();
        for e in &r.entries {
            assert!(!e.stand_in.eq_ignore_ascii_case("zzaa"));
            assert!(!e.stand_in.eq_ignore_ascii_case("zzab"));
            assert!(r.scan(&e.stand_in).is_empty());
        }
    }

    #[test]
    fn sidecar_must_match_and_be_consistent() {
        let r = redactor();
        let mut sidecar = r.to_sidecar("e1");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sidecar.json");
        write_sidecar(&path, &sidecar).unwrap();
        assert!(write_sidecar(&path, &sidecar).is_err(), "no overwrite");
        assert!(load_sidecar(&path, "e1").is_ok());
        assert!(load_sidecar(&path, "e2").is_err());
        assert!(load_sidecar(&dir.path().join("missing.json"), "e1").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
        let variant = sidecar
            .entries
            .iter_mut()
            .find(|e| e.source == "senior analyst")
            .unwrap();
        variant.stand_in = "something else".into();
        assert!(Redactor::from_sidecar(&sidecar).is_err());
    }

    #[test]
    fn harness_ids_are_not_credentials() {
        let r = redactor();
        for id in [
            "generation-1791425654630970000-3920",
            "generation-1791425654630970000-3920-0007",
            "evaluation-1791425654630970000-3920-0",
        ] {
            assert!(r.scan(id).is_empty(), "{id}");
            assert_eq!(r.sanitize(&format!("see {id}.")), format!("see {id}."));
        }
        let sneaky = "generation-1791425654630970000sk9f8d7s6a5f4g3h2j1k";
        assert!(!r.scan(sneaky).is_empty());
    }

    #[test]
    fn report_text_fields_are_sanitized() {
        use crate::contracts::*;
        let r = redactor();
        let report = EvaluationReport {
            schema_version: REPORT_SCHEMA_VERSION,
            evaluation_id: "e".into(),
            generation_id: "g".into(),
            generation_hash: "h".into(),
            candidates_hash: "c".into(),
            metadata_hash: "m".into(),
            dataset_hash: "d".into(),
            cases: vec![CaseReport {
                case_id: "g-1".into(),
                candidate_hash: "x".into(),
                prompt: "Keep Manager".into(),
                review: ReviewStatus::Confirmed,
                review_rationale: Some("wrong for manager at /Users/a/b".into()),
                observed: ObservedResult::Refused,
                plan_comparison: PlanComparison::Unavailable,
                diagnostic_codes: vec!["intent.column_not_found".into()],
                row_counts: None,
                evidence: CaseEvidence::default(),
            }],
            provenance: ToolProvenance::default(),
            aggregates: Aggregates::default(),
            limitations: vec![],
            notes: vec![],
        };
        let clean = r.sanitize_report(&report).unwrap();
        let json = serde_json::to_string(&clean).unwrap();
        assert!(r.scan(&json).is_empty(), "{json}");
        let mut bad = report.clone();
        bad.cases[0].diagnostic_codes = vec!["leak.manager".into()];
        assert!(r.sanitize_report(&bad).is_err());
    }
}
