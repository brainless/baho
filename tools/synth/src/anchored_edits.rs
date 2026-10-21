//! Anchored-range edits for the repair coder (Epic 019, section 6).
//!
//! The coder never returns whole files. Each edit names a file hash, an
//! inclusive 1-based line range, the exact current text of that range, and its
//! replacement. [`plan`] checks every edit in memory against the checkout and
//! either yields the complete result or rejects the whole patch; nothing is
//! written until [`apply`]. Line numbers alone are never trusted.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bound on one new file's content; larger additions must be split.
pub const MAX_NEW_FILE_BYTES: usize = 16 * 1024;
pub const MAX_EDITS: usize = 32;
pub const MAX_NEW_FILES: usize = 4;
/// Stable rejection code for any anchor, hash, overlap, or range failure.
pub const ANCHOR_MISMATCH: &str = "edit_anchor_mismatch";
pub const NEW_FILE_REJECTED: &str = "new_file_rejected";
pub const EDIT_SET_INVALID: &str = "edit_set_invalid";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub path: String,
    pub baseline_sha256: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Exact current text of the lines, without the final line terminator.
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewFile {
    pub path: String,
    pub content: String,
}

/// The coder's answer. A whole-file `{"files": [...]}` answer does not parse.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodeAnswer {
    pub edits: Vec<Edit>,
    #[serde(default)]
    pub new_files: Vec<NewFile>,
}

/// Why a patch was refused. `detail` names paths and edit indexes only, never
/// source text.
#[derive(Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: &'static str,
    pub detail: String,
}

fn reject<T>(code: &'static str, detail: impl Into<String>) -> Result<T, Rejection> {
    Err(Rejection {
        code,
        detail: detail.into(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub start_line: usize,
    pub end_line: usize,
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug)]
pub struct FileEdit {
    pub path: PathBuf,
    pub original: String,
    pub updated: String,
    /// Ascending, non-overlapping, in baseline line numbers.
    pub hunks: Vec<Hunk>,
}

#[derive(Clone, Debug, Default)]
pub struct Planned {
    pub files: Vec<FileEdit>,
    pub new_files: Vec<(PathBuf, String)>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Byte offset at which each line starts; the same numbering `read_lines`
/// shows the model. An empty text has no lines.
fn line_starts(text: &str) -> Vec<usize> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    if starts.last() == Some(&text.len()) {
        starts.pop();
    }
    starts
}

/// Byte span of lines `start..=end` without the final line terminator.
fn span(text: &str, starts: &[usize], start: usize, end: usize) -> (usize, usize) {
    let from = starts[start - 1];
    let mut to = starts.get(end).copied().unwrap_or(text.len());
    if to > from && text.as_bytes()[to - 1] == b'\n' {
        to -= 1;
        if to > from && text.as_bytes()[to - 1] == b'\r' {
            to -= 1;
        }
    }
    (from, to)
}

/// Verifies `answer` against the tree under `root` without writing.
/// `in_scope(path, was_present)` is the caller's write-scope predicate.
pub fn plan(
    root: &Path,
    answer: &CodeAnswer,
    in_scope: impl Fn(&Path, bool) -> bool,
) -> Result<Planned, Rejection> {
    if answer.edits.len() + answer.new_files.len() == 0
        || answer.edits.len() > MAX_EDITS
        || answer.new_files.len() > MAX_NEW_FILES
    {
        return reject(
            EDIT_SET_INVALID,
            format!(
                "edits={} new_files={}",
                answer.edits.len(),
                answer.new_files.len()
            ),
        );
    }
    let mut by_path: BTreeMap<&str, Vec<(usize, &Edit)>> = BTreeMap::new();
    for (index, edit) in answer.edits.iter().enumerate() {
        by_path.entry(&edit.path).or_default().push((index, edit));
    }
    let mut planned = Planned::default();
    for (name, mut edits) in by_path {
        let path = Path::new(name);
        if !in_scope(path, true) {
            return reject(
                ANCHOR_MISMATCH,
                format!("edit path outside write scope: {name}"),
            );
        }
        let full = root.join(path);
        let is_file = fs::symlink_metadata(&full)
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);
        let original = match is_file.then(|| fs::read(&full).ok()).flatten() {
            Some(bytes) => (digest(&bytes), String::from_utf8(bytes).ok()),
            None => return reject(ANCHOR_MISMATCH, format!("not a readable file: {name}")),
        };
        let (sha, Some(original)) = original else {
            return reject(ANCHOR_MISMATCH, format!("file is not UTF-8: {name}"));
        };
        let starts = line_starts(&original);
        edits.sort_by_key(|(_, e)| (e.start_line, e.end_line));
        let mut previous_end = 0usize;
        let mut spans = Vec::new();
        for (index, edit) in &edits {
            if edit.baseline_sha256 != sha {
                return reject(
                    ANCHOR_MISMATCH,
                    format!("edit {index}: file hash differs: {name}"),
                );
            }
            if edit.start_line == 0
                || edit.start_line > edit.end_line
                || edit.end_line > starts.len()
            {
                return reject(
                    ANCHOR_MISMATCH,
                    format!(
                        "edit {index}: range {}-{} invalid for {} lines: {name}",
                        edit.start_line,
                        edit.end_line,
                        starts.len()
                    ),
                );
            }
            if edit.start_line <= previous_end {
                return reject(
                    ANCHOR_MISMATCH,
                    format!("edit {index}: overlaps another edit: {name}"),
                );
            }
            previous_end = edit.end_line;
            let (from, to) = span(&original, &starts, edit.start_line, edit.end_line);
            let current = &original[from..to];
            // read_lines hides CRLF terminators, so accept the exact text it
            // showed the coder while retaining the original bytes for writes.
            let normalized = current.replace("\r\n", "\n");
            let normalized = normalized.strip_suffix('\r').unwrap_or(&normalized);
            if current != edit.old && normalized != edit.old {
                return reject(
                    ANCHOR_MISMATCH,
                    format!(
                        "edit {index}: old text differs from lines {}-{}: {name}",
                        edit.start_line, edit.end_line
                    ),
                );
            }
            let to = if edit.new.is_empty() {
                if original[to..].starts_with("\r\n") {
                    to + 2
                } else if original[to..].starts_with('\n') {
                    to + 1
                } else {
                    to
                }
            } else {
                to
            };
            spans.push((from, to, *edit));
        }
        let mut updated = original.clone();
        for (from, to, edit) in spans.iter().rev() {
            let replacement = if edit.new.is_empty() {
                String::new()
            } else if original[*from..*to].contains("\r\n")
                || original[*from..*to].ends_with('\r')
                || original.contains("\r\n")
            {
                edit.new.replace("\r\n", "\n").replace('\n', "\r\n")
            } else {
                edit.new.clone()
            };
            updated.replace_range(*from..*to, &replacement);
        }
        planned.files.push(FileEdit {
            path: path.to_path_buf(),
            original,
            updated,
            hunks: spans
                .iter()
                .map(|(_, _, e)| Hunk {
                    start_line: e.start_line,
                    end_line: e.end_line,
                    old: e.old.clone(),
                    new: e.new.clone(),
                })
                .collect(),
        });
    }
    let mut seen = BTreeSet::new();
    for file in &answer.new_files {
        let path = Path::new(&file.path);
        let absent = matches!(
            fs::symlink_metadata(root.join(path)),
            Err(e) if e.kind() == io::ErrorKind::NotFound
        );
        if !in_scope(path, false)
            || !absent
            || file.content.len() > MAX_NEW_FILE_BYTES
            || !seen.insert(file.path.as_str())
            || answer.edits.iter().any(|e| e.path == file.path)
        {
            return reject(
                NEW_FILE_REJECTED,
                format!(
                    "new file not allowed, present, duplicate, or over {MAX_NEW_FILE_BYTES} bytes: {}",
                    file.path
                ),
            );
        }
        planned
            .new_files
            .push((path.to_path_buf(), file.content.clone()));
    }
    Ok(planned)
}

/// Writes a verified plan. On failure every touched path is restored, so the
/// checkout is left as it was.
pub fn apply(root: &Path, planned: &Planned) -> io::Result<()> {
    let mut written: Vec<&FileEdit> = Vec::new();
    let mut created: Vec<PathBuf> = Vec::new();
    let result = (|| {
        for file in &planned.files {
            fs::write(root.join(&file.path), &file.updated)?;
            written.push(file);
        }
        for (path, content) in &planned.new_files {
            let dest = root.join(path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, content)?;
            created.push(dest);
        }
        Ok(())
    })();
    if result.is_err() {
        for file in written {
            let _ = fs::write(root.join(&file.path), &file.original);
        }
        for path in created {
            let _ = fs::remove_file(path);
        }
    }
    result
}

/// Restores the files in a successfully applied plan after later validation
/// rejects it. A failed restoration is reported after all paths are attempted.
pub fn restore(root: &Path, planned: &Planned) -> io::Result<()> {
    let mut first_error = None;
    for (path, _) in &planned.new_files {
        if let Err(error) = fs::remove_file(root.join(path))
            && error.kind() != io::ErrorKind::NotFound
        {
            first_error.get_or_insert(error);
        }
    }
    for file in &planned.files {
        if let Err(error) = fs::write(root.join(&file.path), &file.original) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Unified diff of a plan with `context` lines around each hunk, cut at a line
/// boundary once it would exceed `max_bytes`. Returns the text and whether it
/// was cut.
pub fn render_diff(planned: &Planned, context: usize, max_bytes: usize) -> (String, bool) {
    let mut out = String::new();
    let mut truncated = false;
    let push = |line: &str, out: &mut String| -> bool {
        if out.len() + line.len() + 1 > max_bytes {
            return false;
        }
        out.push_str(line);
        out.push('\n');
        true
    };
    'files: for file in &planned.files {
        let name = file.path.display();
        let starts = line_starts(&file.original);
        let line = |n: usize| {
            let (a, b) = span(&file.original, &starts, n, n);
            &file.original[a..b]
        };
        if !push(&format!("--- a/{name}"), &mut out) || !push(&format!("+++ b/{name}"), &mut out) {
            truncated = true;
            break;
        }
        let mut delta: isize = 0;
        let mut index = 0;
        while index < file.hunks.len() {
            let mut last = index;
            while last + 1 < file.hunks.len()
                && file.hunks[last + 1].start_line - file.hunks[last].end_line - 1 <= 2 * context
            {
                last += 1;
            }
            let group = &file.hunks[index..=last];
            let first_line = group[0].start_line.saturating_sub(context).max(1);
            let last_line = (group[group.len() - 1].end_line + context).min(starts.len());
            let old_count = last_line + 1 - first_line;
            let new_lines = |h: &Hunk| usize::from(!h.new.is_empty()) + h.new.matches('\n').count();
            let group_delta: isize = group
                .iter()
                .map(|h| new_lines(h) as isize - (h.end_line - h.start_line + 1) as isize)
                .sum();
            let new_start = (first_line as isize + delta) as usize;
            let new_count = (old_count as isize + group_delta) as usize;
            if !push(
                &format!("@@ -{first_line},{old_count} +{new_start},{new_count} @@"),
                &mut out,
            ) {
                truncated = true;
                break 'files;
            }
            let mut cursor = first_line;
            let mut lines: Vec<String> = Vec::new();
            for hunk in group {
                lines.extend((cursor..hunk.start_line).map(|n| format!(" {}", line(n))));
                lines.extend(hunk.old.split('\n').map(|l| format!("-{l}")));
                if !hunk.new.is_empty() {
                    lines.extend(hunk.new.split('\n').map(|l| format!("+{l}")));
                }
                cursor = hunk.end_line + 1;
            }
            lines.extend((cursor..=last_line).map(|n| format!(" {}", line(n))));
            for text in lines {
                if !push(&text, &mut out) {
                    truncated = true;
                    break 'files;
                }
            }
            delta += group_delta;
            index = last + 1;
        }
    }
    if !truncated {
        for (path, content) in &planned.new_files {
            let count = content.lines().count();
            let header = [
                "--- /dev/null".to_owned(),
                format!("+++ b/{}", path.display()),
                format!("@@ -0,0 +1,{count} @@"),
            ];
            if !header.iter().all(|h| push(h, &mut out)) {
                truncated = true;
                break;
            }
            if !content.lines().all(|l| push(&format!("+{l}"), &mut out)) {
                truncated = true;
                break;
            }
        }
    }
    (out, truncated)
}

/// Lays out a read-only review view under `view`: `candidate/` holds the
/// patched source and test trees, `baseline/` holds the pre-patch text of each
/// edited file. Unedited files are identical in both, so they are not copied
/// twice. Symlinks are skipped.
pub fn materialize_review_view(checkout: &Path, view: &Path, planned: &Planned) -> io::Result<()> {
    if view.exists() {
        fs::remove_dir_all(view)?;
    }
    for tree in ["crates/baho-core/src", "crates/baho-core/tests"] {
        let from = checkout.join(tree);
        if from.is_dir() {
            copy_tree(&from, &view.join("candidate").join(tree))?;
        }
    }
    for file in &planned.files {
        let dest = view.join("baseline").join(&file.path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(dest, &file.original)?;
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    let mut entries: Vec<_> = fs::read_dir(from)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let kind = entry.file_type()?;
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "crates/baho-core/src/lib.rs";

    fn scope(path: &Path, present: bool) -> bool {
        let text = path.to_string_lossy();
        text.starts_with("crates/baho-core/src/")
            || (!present && text.starts_with("crates/baho-core/tests/"))
    }

    fn tree(text: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SRC);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        (dir, digest(text.as_bytes()))
    }

    fn edit(sha: &str, start: usize, end: usize, old: &str, new: &str) -> Edit {
        Edit {
            path: SRC.into(),
            baseline_sha256: sha.into(),
            start_line: start,
            end_line: end,
            old: old.into(),
            new: new.into(),
        }
    }

    fn answer(edits: Vec<Edit>) -> CodeAnswer {
        CodeAnswer {
            edits,
            new_files: vec![],
        }
    }

    const TEXT: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";

    #[test]
    fn applies_matching_edits_bottom_up_with_changing_line_counts() {
        let (dir, sha) = tree(TEXT);
        let plan = plan(
            dir.path(),
            &answer(vec![
                edit(&sha, 6, 6, "f", "F1\nF2\nF3"),
                edit(&sha, 2, 3, "b\nc", "BC"),
            ]),
            scope,
        )
        .unwrap();
        assert_eq!(plan.files[0].updated, "a\nBC\nd\ne\nF1\nF2\nF3\ng\nh\n");
        assert_eq!(plan.files[0].hunks[0].start_line, 2);
        apply(dir.path(), &plan).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(SRC)).unwrap(),
            "a\nBC\nd\ne\nF1\nF2\nF3\ng\nh\n"
        );
    }

    #[test]
    fn last_line_without_terminator_and_adjacent_edits_work() {
        let (dir, sha) = tree("x\ny\nz");
        let plan = plan(
            dir.path(),
            &answer(vec![
                edit(&sha, 3, 3, "z", "Z"),
                edit(&sha, 1, 2, "x\ny", "XY"),
            ]),
            scope,
        )
        .unwrap();
        assert_eq!(plan.files[0].updated, "XY\nZ");
    }

    #[test]
    fn crlf_anchor_matches_read_lines_text_and_preserves_terminators() {
        let original = "one\r\ntwo\r\nthree\r\n";
        let (dir, sha) = tree(original);
        let planned = plan(
            dir.path(),
            &answer(vec![edit(&sha, 1, 2, "one\ntwo", "ONE\nTWO")]),
            scope,
        )
        .unwrap();
        assert_eq!(planned.files[0].updated, "ONE\r\nTWO\r\nthree\r\n");
        apply(dir.path(), &planned).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(SRC)).unwrap(),
            "ONE\r\nTWO\r\nthree\r\n"
        );
    }

    #[test]
    fn empty_replacement_deletes_lines_instead_of_leaving_blank_line() {
        let (dir, sha) = tree("one\ntwo\nthree\n");
        let planned = plan(
            dir.path(),
            &answer(vec![edit(&sha, 2, 2, "two", "")]),
            scope,
        )
        .unwrap();
        assert_eq!(planned.files[0].updated, "one\nthree\n");
        let (diff, truncated) = render_diff(&planned, 1, 1024);
        assert!(!truncated);
        assert!(diff.contains("@@ -1,3 +1,2 @@"));
        assert!(diff.contains("-two\n"));
        assert!(!diff.contains("+\n"));
        apply(dir.path(), &planned).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(SRC)).unwrap(),
            "one\nthree\n"
        );

        let (dir, sha) = tree("one\r\ntwo\r\n");
        let planned = plan(
            dir.path(),
            &answer(vec![edit(&sha, 2, 2, "two", "")]),
            scope,
        )
        .unwrap();
        assert_eq!(planned.files[0].updated, "one\r\n");
    }

    #[test]
    fn any_mismatch_rejects_the_whole_patch_and_leaves_files_unchanged() {
        let (dir, sha) = tree(TEXT);
        let good = edit(&sha, 1, 1, "a", "A");
        let cases = [
            ("hash", edit(&"0".repeat(64), 2, 2, "b", "B")),
            ("old text", edit(&sha, 2, 2, "not b", "B")),
            ("shifted lines", edit(&sha, 3, 3, "b", "B")),
            ("past end", edit(&sha, 8, 9, "h", "H")),
            ("zero start", edit(&sha, 0, 1, "a", "A")),
            ("reversed", edit(&sha, 4, 3, "d", "D")),
            ("overlap", edit(&sha, 1, 2, "a\nb", "AB")),
        ];
        for (label, bad) in cases {
            let rejection =
                plan(dir.path(), &answer(vec![good.clone(), bad]), scope).expect_err(label);
            assert_eq!(rejection.code, ANCHOR_MISMATCH, "{label}");
            assert_eq!(
                fs::read_to_string(dir.path().join(SRC)).unwrap(),
                TEXT,
                "{label}: checkout unchanged"
            );
        }
        let rejection = plan(dir.path(), &answer(vec![good.clone(), good]), scope).unwrap_err();
        assert!(rejection.detail.contains("overlaps"));
    }

    #[test]
    fn paths_must_be_in_scope_existing_regular_files() {
        let (dir, sha) = tree(TEXT);
        let mut outside = edit(&sha, 1, 1, "a", "A");
        outside.path = "crates/baho-model/src/lib.rs".into();
        assert_eq!(
            plan(dir.path(), &answer(vec![outside]), scope)
                .unwrap_err()
                .code,
            ANCHOR_MISMATCH
        );
        let mut missing = edit(&sha, 1, 1, "a", "A");
        missing.path = "crates/baho-core/src/none.rs".into();
        assert!(plan(dir.path(), &answer(vec![missing]), scope).is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("crates/baho-core/src/link.rs");
            std::os::unix::fs::symlink(dir.path().join(SRC), &link).unwrap();
            let mut via_link = edit(&sha, 1, 1, "a", "A");
            via_link.path = "crates/baho-core/src/link.rs".into();
            assert!(plan(dir.path(), &answer(vec![via_link]), scope).is_err());
        }
    }

    #[test]
    fn new_files_are_bounded_new_and_unique() {
        let (dir, sha) = tree(TEXT);
        let ok = NewFile {
            path: "crates/baho-core/tests/t.rs".into(),
            content: "#[test]\nfn t() {}\n".into(),
        };
        let planned = plan(
            dir.path(),
            &CodeAnswer {
                edits: vec![edit(&sha, 1, 1, "a", "A")],
                new_files: vec![ok.clone()],
            },
            scope,
        )
        .unwrap();
        apply(dir.path(), &planned).unwrap();
        assert!(dir.path().join("crates/baho-core/tests/t.rs").is_file());
        let only_new = |files: Vec<NewFile>| {
            plan(
                dir.path(),
                &CodeAnswer {
                    edits: vec![],
                    new_files: files,
                },
                scope,
            )
            .unwrap_err()
        };
        assert_eq!(only_new(vec![ok.clone()]).code, NEW_FILE_REJECTED, "exists");
        let big = NewFile {
            path: "crates/baho-core/tests/big.rs".into(),
            content: "x".repeat(MAX_NEW_FILE_BYTES + 1),
        };
        assert_eq!(only_new(vec![big]).code, NEW_FILE_REJECTED);
        let mut twice = ok.clone();
        twice.path = "crates/baho-core/tests/u.rs".into();
        assert_eq!(only_new(vec![twice.clone(), twice]).code, NEW_FILE_REJECTED);
        assert_eq!(only_new(vec![]).code, EDIT_SET_INVALID);
    }

    #[test]
    fn failed_apply_restores_files() {
        let (dir, sha) = tree(TEXT);
        let mut planned =
            plan(dir.path(), &answer(vec![edit(&sha, 1, 1, "a", "A")]), scope).unwrap();
        // A new file below a regular file cannot be created.
        planned
            .new_files
            .push((PathBuf::from(SRC).join("nested.rs"), "x".into()));
        assert!(apply(dir.path(), &planned).is_err());
        assert_eq!(fs::read_to_string(dir.path().join(SRC)).unwrap(), TEXT);
    }

    #[test]
    fn restore_after_later_rejection_reverts_edits_and_new_files() {
        let (dir, sha) = tree(TEXT);
        let new_path = "crates/baho-core/tests/new.rs";
        let planned = plan(
            dir.path(),
            &CodeAnswer {
                edits: vec![edit(&sha, 1, 1, "a", "A")],
                new_files: vec![NewFile {
                    path: new_path.into(),
                    content: "#[test] fn new_test() {}\n".into(),
                }],
            },
            scope,
        )
        .unwrap();
        apply(dir.path(), &planned).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(SRC)).unwrap(),
            planned.files[0].updated
        );
        assert!(dir.path().join(new_path).is_file());
        restore(dir.path(), &planned).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join(SRC)).unwrap(), TEXT);
        assert!(!dir.path().join(new_path).exists());
    }

    #[test]
    fn whole_file_answers_do_not_parse() {
        let old = r#"{"files":[{"path":"crates/baho-core/src/a.rs","baseline_sha256":"x","content":"c"}]}"#;
        assert!(serde_json::from_str::<CodeAnswer>(old).is_err());
        let new = r#"{"edits":[{"path":"p","baseline_sha256":"x","start_line":1,"end_line":1,"old":"a","new":"b"}]}"#;
        assert!(
            serde_json::from_str::<CodeAnswer>(new)
                .unwrap()
                .new_files
                .is_empty()
        );
    }

    #[test]
    fn diff_has_context_merged_hunks_and_correct_new_line_numbers() {
        let text: String = (1..=30).map(|i| format!("l{i}\n")).collect();
        let (dir, sha) = tree(&text);
        let planned = plan(
            dir.path(),
            &CodeAnswer {
                edits: vec![
                    edit(&sha, 5, 5, "l5", "n5a\nn5b"),
                    edit(&sha, 8, 8, "l8", "n8"),
                    edit(&sha, 25, 25, "l25", "n25"),
                ],
                new_files: vec![NewFile {
                    path: "crates/baho-core/tests/t.rs".into(),
                    content: "fn t() {}\n".into(),
                }],
            },
            scope,
        )
        .unwrap();
        let (diff, truncated) = render_diff(&planned, 2, 4096);
        assert!(!truncated);
        let expected = "\
--- a/crates/baho-core/src/lib.rs
+++ b/crates/baho-core/src/lib.rs
@@ -3,8 +3,9 @@
 l3
 l4
-l5
+n5a
+n5b
 l6
 l7
-l8
+n8
 l9
 l10
@@ -23,5 +24,5 @@
 l23
 l24
-l25
+n25
 l26
 l27
--- /dev/null
+++ b/crates/baho-core/tests/t.rs
@@ -0,0 +1,1 @@
+fn t() {}
";
        assert_eq!(diff, expected);
        let (cut, truncated) = render_diff(&planned, 2, 120);
        assert!(truncated && cut.len() <= 120 && cut.ends_with('\n'));
    }

    #[test]
    fn review_view_has_candidate_tree_and_baseline_of_edited_files_only() {
        let (dir, sha) = tree(TEXT);
        fs::write(dir.path().join("crates/baho-core/src/other.rs"), "o\n").unwrap();
        let planned = plan(dir.path(), &answer(vec![edit(&sha, 1, 1, "a", "A")]), scope).unwrap();
        apply(dir.path(), &planned).unwrap();
        let view = tempfile::tempdir().unwrap();
        let view_path = view.path().join("v");
        materialize_review_view(dir.path(), &view_path, &planned).unwrap();
        let read = |p: &str| fs::read_to_string(view_path.join(p)).unwrap();
        assert!(read(&format!("candidate/{SRC}")).starts_with("A\n"));
        assert!(read(&format!("baseline/{SRC}")).starts_with("a\n"));
        assert_eq!(read("candidate/crates/baho-core/src/other.rs"), "o\n");
        assert!(
            !view_path
                .join("baseline/crates/baho-core/src/other.rs")
                .exists()
        );
    }
}
