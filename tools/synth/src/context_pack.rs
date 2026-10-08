//! Per-goal context packs: fixed, harness-resolved excerpts injected ahead of the evaluation.
//!
//! A pack is a JSON manifest (`tools/synth/context/<name>.json`; JSON because the package
//! already depends on serde_json and a TOML parser would be a new dependency for a
//! developer-only file). Entries are read through the same sanitized reader as `read_file`
//! (allowlist, denylist, symlink recheck, redaction, original line numbers), so a pack cannot
//! reach anything the model could not read itself. Anchors are resolved against the working
//! tree at run time; an unresolved anchor, a disallowed path, or an oversized pack fails the
//! run before any model call.

use crate::planner_tools::Toolbox;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

/// Total rendered size cap.
pub const MAX_PACK_BYTES: usize = 64 * 1024;
const MAX_ENTRIES: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PackError {
    #[error("context pack name must be 1-48 characters of [a-z0-9-]")]
    Name,
    #[error("context pack '{0}' cannot be read")]
    Missing(String),
    #[error("context pack '{0}' is not a valid manifest: {1}")]
    Manifest(String, String),
    #[error("context pack entry {index} ({path}): {reason}")]
    Entry {
        index: usize,
        path: String,
        reason: String,
    },
    #[error("context pack is {bytes} bytes; the cap is {MAX_PACK_BYTES}")]
    TooLarge { bytes: usize },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    entries: Vec<Entry>,
}

/// Either `anchor` (+ optional `lines_before`/`lines_after`) or `start_line`+`end_line`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    anchor: Option<String>,
    #[serde(default)]
    lines_before: usize,
    #[serde(default)]
    lines_after: usize,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedEntry {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// What the plan record keeps: no contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PackRecord {
    pub name: String,
    pub hash: String,
    pub bytes: usize,
    pub entries: Vec<ResolvedEntry>,
}

pub struct ContextPack {
    pub text: String,
    pub record: PackRecord,
}

pub fn load(manifest_dir: &Path, name: &str, toolbox: &Toolbox) -> Result<ContextPack, PackError> {
    let valid = !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err(PackError::Name);
    }
    let raw = fs::read_to_string(manifest_dir.join(format!("{name}.json")))
        .map_err(|_| PackError::Missing(name.into()))?;
    let manifest: Manifest = serde_json::from_str(&raw)
        .map_err(|e| PackError::Manifest(name.into(), e.classify_name()))?;
    if manifest.entries.is_empty() || manifest.entries.len() > MAX_ENTRIES {
        return Err(PackError::Manifest(
            name.into(),
            format!("entry count must be 1-{MAX_ENTRIES}"),
        ));
    }
    let mut text = format!(
        "## Context pack `{name}` (provided by the harness)\n\nThe excerpts below are real, \
         sanitized repository text with original line numbers. Their line numbers are citable \
         as `path:line` (list the file under Existing evidence files). Use tools only for \
         anything not covered here.\n"
    );
    let mut entries = Vec::new();
    for (index, e) in manifest.entries.iter().enumerate() {
        let fail = |reason: String| PackError::Entry {
            index,
            path: e.path.clone(),
            reason,
        };
        let (start, end) = match (&e.anchor, e.start_line, e.end_line) {
            (Some(anchor), None, None) => {
                let (hits, _) = toolbox.find_lines(&e.path, anchor).map_err(&fail)?;
                match hits.as_slice() {
                    [line] => (
                        line.saturating_sub(e.lines_before).max(1),
                        line + e.lines_after,
                    ),
                    [] => return Err(fail("anchor does not resolve".into())),
                    many => {
                        return Err(fail(format!(
                            "anchor is ambiguous ({} matches)",
                            many.len()
                        )));
                    }
                }
            }
            (None, Some(a), Some(b)) if a >= 1 && a <= b => (a, b),
            _ => {
                return Err(fail(
                    "give either an anchor, or start_line and end_line (1 <= start <= end)".into(),
                ));
            }
        };
        let r = toolbox.read_range(&e.path, start, end).map_err(&fail)?;
        if r.lines.is_empty() {
            return Err(fail("range is empty".into()));
        }
        let block = format!(
            "\n### {}:{}-{}\n{}\n",
            r.path,
            r.start_line,
            r.end_line,
            r.lines.join("\n")
        );
        if !toolbox.is_clean(&block) {
            return Err(fail("content could not be sanitized safely".into()));
        }
        text.push_str(&block);
        if text.len() > MAX_PACK_BYTES {
            return Err(PackError::TooLarge { bytes: text.len() });
        }
        entries.push(ResolvedEntry {
            path: r.path,
            start_line: r.start_line,
            end_line: r.end_line,
        });
    }
    let record = PackRecord {
        name: name.into(),
        hash: format!("{:x}", Sha256::digest(text.as_bytes())),
        bytes: text.len(),
        entries,
    };
    Ok(ContextPack { text, record })
}

trait ClassifyName {
    fn classify_name(&self) -> String;
}
impl ClassifyName for serde_json::Error {
    /// Category and position only; never echo manifest text.
    fn classify_name(&self) -> String {
        format!("{:?} at line {}", self.classify(), self.line())
    }
}
