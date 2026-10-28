# Epic 019: Structural Source Outline and Bounded Range Reads

Status: Abandoned. These repair-agent tools were retired and removed with `tools/synth`; this epic remains as a historical record.

Motivating run: repair attempt `repair-1791610148027574000-14036-0` (Epic 018
harness), evaluation `evaluation-1791470942646632000-53201-0`, case
`generation-1791469817842875000-52681-0002`, baseline `6411899`.

User intent: Let repair agents find and read the relevant parts of large Rust
source files without ever receiving a whole large file. Expose high-level
structure (imports, type definitions, function signatures) with line numbers so
the model can request precise, small line ranges. Record what was exposed to
the model and what it asked for.

## Observed behavior

The attempt ended `model_exhausted` with reason
`investigation: max_agent_turns (limit=6); tool_budget_exhausted=false`.
`agent-events.jsonl` shows six model calls, each issuing tool calls, and no
final answer. Of 11 charged tool calls, 7 failed: all four `search_text` calls
and three of seven `read_file` calls. Rig 0.43 counts `max_turns` as total model
calls including the final answer (`rig-agent/src/run/mod.rs`, `max_turns`), so
the sixth call issuing tools left no call for an answer.

The failures come from the source tools in `tools/synth/src/repair_agents.rs`,
not from the model alone:

- `SourceTools::resolve` rejects any file larger than 64 KiB. In the pinned
  checkout `crates/baho-core/src/intent.rs` is 176,047 bytes and
  `orchestration.rs` 127,571 bytes. Comparison-operator parsing and most
  `ComparisonOperator::Equal` construction live in `intent.rs`, so the file most
  likely to hold the Role-equality defect was unreadable.
- `search_text` iterates the directory and calls `resolve(..)?` on each file.
  The first file over 64 KiB fails the entire search instead of being skipped.
  `read_dir` order is unspecified, so results are also nondeterministic.
- Every rejection returns the same `SourceToolError` text ("repair source tool
  denied request"). The model cannot distinguish a wrong path from an oversized
  file and spent turns guessing paths.
- The coder stage (`code()`) embeds every allowed file whole in its prompt and
  requires complete replacement file contents, with the same 64 KiB limit. A
  correct diagnosis pointing at `intent.rs` would end in `needs_design`.

### Logging gap

`tools/synth/src/repair_trace.rs` is deliberately payload-free. For each tool
call it records only `call_id`, tool name, `argument_bytes`, `elapsed_ms`, and a
boolean `succeeded`; for model calls only timings and token usage. It does
**not** record the requested path, query, or range, the reason for a rejection,
or what source was returned. The analysis above had to infer which reads failed
from file sizes and input-token jumps. "What the agent asked for and what we
exposed" is not currently recoverable from run artifacts.

## Problem

A whole-file size cap is the wrong control. It blocks access to large files
entirely while still permitting up to 64 KiB of source per response for smaller
files. The control we want is a small cap on each *response*, paired with a
structural map that lets the model choose what to read. That choice and its
result must be observable.

## Desired behavior

Agents navigate source in two steps: get a compact outline with line spans,
then read bounded line ranges. No tool returns more than a small fixed number of
source bytes per call, regardless of file size. Every source request and every
exposed span is recorded in structured events against the pinned source hash,
so the exact exposure can be reconstructed without storing source text in logs.

### Out of scope

- Go-to-definition, find-references, type inference, or macro expansion
  (rust-analyzer class features).
- Non-Rust source languages; files under the allowed scope that are not `.rs`
  are reached only through `read_lines`.
- Changing allowed write scope, the probe/check tools, or the review policy of
  Epic 018 beyond what is stated here.
- Changing the model or provider.

## Ownership

All changes are in the excluded `tools/synth` package: source tools and edit
application in `repair_agents.rs` (or a new `source_tools.rs` module), the
trace in `repair_trace.rs`, and settings/contract types in `repair.rs`. Product
crates are unchanged and gain no dependency. `syn` 3 (features `full`,
`parsing`, `printing`) and `proc-macro2` (`span-locations`) become direct
dependencies of `tools/synth` only; both are already present transitively in its
lockfile (`syn` 3.0.6, `proc-macro2` 1.0.107).

### Dependency behavior (read from local checkouts)

Read from `~/Projects/syn` (3.0.7) and `~/Projects/proc-macro2` (1.0.107):

- With `span-locations`, `Span::start()/end()` return `LineColumn` with a
  **1-indexed line** and a 0-indexed column in UTF-8 characters, and
  `Span::byte_range()` returns byte offsets. Outside a procedural macro both are
  accurate on stable (`proc-macro2/src/lib.rs`, `src/location.rs`).
- `syn::spanned::Spanned` requires the `parsing` and `printing` features; it
  joins first and last token spans. Whether an item's span covers its outer
  attributes and doc comments is verified by test rather than assumed.
- Span locations are backed by a **thread-local source map** that retains every
  parsed file's line table for the life of the thread and uses 32-bit offsets
  (`src/fallback.rs`, `extra::invalidate_current_thread_spans`). The harness runs
  on long-lived Tokio worker threads, so no `Span` may escape the parse
  function: spans are converted to plain line/byte data immediately, and parsing
  runs on a short-lived scoped thread whose source map is dropped on exit.

## Proposed design

### 1. `outline(path)`

Parse the file with `syn` and return a compact text outline in source order, one
line per item, with 1-based inclusive line spans (matching compiler and editor
line numbers shown to the model):

```
crates/baho-core/src/intent.rs  4773 lines  176047 bytes  sha256:ab12…
   1-  18  use  std::collections::BTreeMap; baho_model::{…}; crate::grounding::{…}
  20-  41  enum ComparisonOperator { Equal, NotEqual, Less, … }
 122- 610  impl Parser
 130- 188    fn parse_comparison(&mut self, pos: usize) -> Option<Filter>
 925- 940  fn operator_symbol(s: &str) -> Option<ComparisonOperator>
3990-4773  mod tests
3995-4010    #[test] fn equals_word_maps_to_equal
```

- Items: `use` blocks, `mod`, `struct`, `enum`, `union`, `trait`, `impl`,
  `fn`, `const`, `static`, `type`, `macro_rules!`, and nested items in `impl`,
  `trait`, and inline `mod` blocks.
- Contiguous `use` items collapse to one line listing top-level paths.
- Structs and enums list field/variant names only, without types.
- Functions show the signature without body or `where` clause. Signatures are
  sliced from the original source text by span and whitespace-collapsed, not
  re-rendered from tokens.
- Spans include leading doc comments and attributes, so a read starting at the
  span start gets full context.
- Each line is truncated (~120 chars) with an ellipsis marker.
- The outline is paged: at most ~6 KiB per response with
  `truncated: true, next_line: N` for continuation.
- If `syn` fails to parse, return a fallback outline built from line-prefix
  matches (`fn`, `pub fn`, `struct`, `enum`, `impl`, `mod`, `use`) and mark it
  `outline_kind: "fallback"`.

### 2. `read_lines(path, ranges)`

- Accepts up to 4 ranges `{start, end}` (1-based, inclusive).
- Returns numbered lines. The combined response is capped at **1 KiB** of source
  text. On overflow it cuts at the last whole line and returns
  `truncated: true, next_line: N` for the range in progress; later ranges are
  reported as not served.
- A single line longer than the cap is cut on a UTF-8 character boundary and
  flagged `line_truncated`.
- Works for any allowed file regardless of size or parseability.

### 3. `search_text(query)`

- Searches every file under the allowed scope regardless of size, streaming by
  line; no whole-file size gate. Files that cannot be read are skipped and
  counted, never fatal.
- Sorted by path then line; at most 40 matches; excerpts at most 160 chars.
- Each match names its innermost enclosing outline item and span, e.g.
  `intent.rs:931  "=" => Some(ComparisonOperator::Equal),  in fn operator_symbol [925-940]`.
  Outlines are parsed at most once per file per session and cached.

### 4. Typed tool errors

Replace the single `SourceToolError` with stable codes returned to the model
and recorded in the trace: `path_not_allowed`, `not_found`, `not_a_file`,
`invalid_range`, `query_invalid`, `budget_exhausted`,
`session_bytes_exhausted`, `parse_failed` (outline falls back rather than
erroring, but the code is recorded). Error text tells the model the next step
(for example "use outline, then read_lines").

### 5. Budgets

- Per response: 1 KiB for `read_lines`, ~6 KiB for an outline page, bounded
  match count for search.
- Per stage: a total exposed-source budget (initially 48 KiB, counting
  `read_lines` text, outline text, and search excerpts), alongside the existing
  tool-call budget.
- Investigator `max_agent_turns` increases (initially 12). One of those calls
  is reserved for the final answer (section 8), so the tool-enabled run gets
  `max_agent_turns - 1` calls. The prompt states the turn and byte budgets.
- All values live in `AttemptSettings`, are pinned in the contract, and are
  logged at `agent_started`. These initial values are accepted for the first
  implementation and tuned later from recorded exposure in real runs.

### 6. Coder edits by anchored ranges

The coder no longer receives whole files or returns whole files. It gets the
same outline/read/search tools and returns edits:

```json
{"edits": [{"path": "crates/baho-core/src/intent.rs",
            "baseline_sha256": "…",
            "start_line": 925, "end_line": 940,
            "old": "<exact current text of those lines>",
            "new": "<replacement text>"}],
 "new_files": [{"path": "crates/baho-core/tests/….rs", "content": "…"}]}
```

- The harness verifies the file hash, that `old` equals the stated lines
  exactly, and that edits do not overlap, then applies them bottom-up. Any
  mismatch rejects the whole patch (`rejected`, reason `edit_anchor_mismatch`).
  Line numbers alone are never trusted.
- New test files remain whole-content but bounded (initially 16 KiB each).
- Existing write-scope and protected-file checks in `check_edits` still apply
  to the resulting tree.
- The reviewer receives a bounded unified diff with context instead of whole
  patched files, plus read-only outline/read/search tools on both baseline and
  candidate trees.

This changes the coder's answer contract and lands in this epic: without it,
repairs to files over the prompt budget cannot succeed. The coder prompt
version is bumped; no compatibility with the whole-file answer is kept, since
no attempt has been accepted under it.

### 7. Request and exposure ledger

Bump the `agent-events.jsonl` sidecar to schema version 2 (version 1 readers
reject it; existing attempt manifests and contracts are unaffected). Add
structured events correlated by Rig `call_id`:

- `tool_call_requested` (from the dispatch hook, parsing args with the tool's
  typed argument struct): tool name plus a safe projection of arguments —
  `path`, `ranges`, outline page start, `suite`, `build_id`. Search queries are
  logged verbatim only if `Redactor::scan` returns no violations; otherwise only
  `query_sha256`, `query_chars`, and `withheld: <violation kinds>`. Probe
  arguments are logged as CSV/prompt/SQL hashes and sizes, never text.
  Unparseable arguments log `args_invalid` and the byte count.
- `tool_call_finished` gains `code` (`ok` or a typed error) and an `exposed`
  list: `{path, file_sha256, start_line, end_line, bytes}` for every span whose
  text reached the model, plus `truncated`, `next_line`, `match_count`,
  `files_skipped`, and `outline_kind` where relevant.
- `agent_finished` gains `source_bytes_exposed` and a per-file union of exposed
  line spans.

Source text itself is still never written to the trace. Because spans are
recorded against the pinned checkout hash, exact exposure is reproducible from
`protected/source.tar` and the ledger. The tools return a small structured
metadata block alongside content so the outcome hook can project it without
re-reading files.

### 8. Reserved final answer at the turn limit

Every stage (investigation, coding, review) reserves its last model call for an
answer. If the tool-enabled run ends with Rig's `PromptError::MaxTurns`, the
harness takes the `chat_history` that error carries and makes exactly one more
call through the runner's `history(..)` with no tools available, instructing:
budget exhausted; return the stage's final JSON now, or declare insufficient
evidence.

- The final call accepts either the stage's normal answer schema or
  `{"insufficient_evidence": true, "open_questions": [..]}` (at most 5 questions,
  each bounded). Normal answers pass the existing validation unchanged; the
  reserved call grants no extra authority.
- Insufficient evidence ends investigation and coding as `model_exhausted` and
  review as `review_incomplete`; it never yields acceptance. Open questions are
  stored in the attempt record after `Redactor::scan_prose`; flagged questions
  are withheld and recorded only as a hash and violation kinds.
- The reserved call obeys the provider request timeout and output-byte
  limits. A tool call emitted anyway ends the stage with cause
  `final_answer_tool_call`; invalid JSON ends with `invalid_final_json`.
- If the provider rejects tool-call messages in history when no tools are
  declared, keep the tools declared but disallow them for that turn, if Rig
  supports a per-turn tool restriction; verify against the configured provider
  with a mock before choosing.
- Trace events `final_answer_requested` and `final_answer_finished` record the
  cause and outcome. `agent_finished` distinguishes `completed` from
  `completed_after_budget` and `insufficient_evidence`, and the manifest reason
  names which applied.

Carrying notes or conclusions into a later retry is out of scope: Epic 018
requires retries to revalidate and inherit nothing. A future continuation
design should start from the deterministic exposure ledger, with any model
open questions marked unverified.

## Alternatives considered

- **Whole-item reads by name (`read_item`)**: convenient, but name resolution
  (multiple `impl` blocks, overloads, macro-generated items) adds complexity,
  and a large item still needs splitting. Outline plus ranges keeps the tool
  simple and leaves the model to choose read sizes.
- **Line ranges without an outline**: the model pages blindly through thousands
  of lines and exhausts turns.
- **tree-sitter with tree-sitter-rust**: tolerant of broken files and
  multi-language, but adds a C build dependency and grammar version to track.
  `syn` suffices because reads target a compiling baseline, and `read_lines`
  covers parse failures. Revisit if non-Rust sources come into scope.
- **rust-analyzer (LSP or `ra_ap_*`)**: definitions and references would help,
  but startup cost, process lifecycle, and dependency weight do not fit this
  confined repair loop now.
- **Raising the 64 KiB cap**: unbounded growth of per-response context; rejected
  by the user.

## Acceptance criteria

1. On a synthetic Rust file over 64 KiB, `outline` returns its items with
   correct 1-based spans, and `read_lines` serves any range, with no response
   over its cap.
2. `read_lines` honors the 1 KiB cap across up to 4 ranges, cutting on whole
   lines with correct `next_line`, and cuts an over-long single line on a UTF-8
   boundary with `line_truncated`.
3. `search_text` returns matches from files of any size in sorted order with
   enclosing item names; an unreadable file is skipped and counted, not fatal.
4. A file that fails to parse yields a fallback outline marked as such;
   `read_lines` still works on it.
5. Path escapes (`..`, absolute paths, symlinks out of the checkout) and paths
   outside the allowed scope return `path_not_allowed` and expose nothing.
6. Each typed error code is returned to the model and recorded in the trace.
7. Coder edits apply only when hashes and `old` text match exactly and edits do
   not overlap; any mismatch rejects the whole patch and leaves the checkout
   unchanged.
8. For a mock-agent session, the trace records every request's safe argument
   projection and every exposed span with file hash and byte count; replaying
   spans against the source tarball reproduces exactly the bytes the mock model
   received. A query containing a profiled source literal is logged only as a
   hash and length.
9. A schema-v1 trace reader rejects v2 events; existing manifest and contract
   schema tests still pass, and the contract records the new budget settings
   under an explicit version decision.
10. When a mock model issues tool calls on every turn, the harness makes one
    reserved tool-free call with the accumulated history. A valid answer
    proceeds through normal validation; an insufficient-evidence answer ends
    `model_exhausted` (or `review_incomplete` for review) with redacted open
    questions recorded; a tool call or invalid JSON in that call ends the stage
    with its named cause. The tool-enabled run never exceeds
    `max_agent_turns - 1` calls.
11. A manual rerun of the motivating command gets past investigation without
    path guessing, or ends with a named outcome whose trace explains which
    requests failed and why.

## Fixture and test strategy

- Generate synthetic Rust sources inside tests (for example, a few hundred
  generated functions and nested `impl`/`mod` blocks) rather than copying
  baho-core files; include a non-parsing file, a line longer than 1 KiB with
  multibyte characters, and a symlink pointing outside the root.
- Reuse Rig `MockCompletionModel` turns to drive outline → search →
  read_lines → answer and assert on the trace ledger, as `repair_trace.rs` tests
  already do. No live provider calls.
- Keep the existing payload-free assertions: source text and prompt text never
  appear in the trace.
- Dataset content never appears in fixtures; queries containing profiled
  literals are tested with a synthetic redactor vocabulary.

## Implementation outline

1. Add a focused test that pins span semantics this design relies on: 1-based
   lines, outer attributes and doc comments inside item spans, and no growth
   of the caller thread's source map across repeated parses.
2. Add a source-tools module with `outline`, `read_lines`, and `search_text`,
   typed errors, and per-response and per-stage budgets; unit-test against
   generated fixtures.
3. Add the v2 trace ledger and wire the tools' metadata into the hooks.
4. Replace `read_file`/`search_text` in investigator and reviewer sessions;
   update prompts and prompt versions; raise the investigator turn budget.
   Add the reserved final-answer call to the shared `ask` path for all stages.
5. Switch the coder to anchored edits and the reviewer to bounded diffs.
6. Run focused synth tests, synth fmt/check/Clippy, root fmt/check/tests, the
   deterministic smoke test, then one manual rerun of the motivating command.

## Decisions (2026-10-10)

- `syn` and `proc-macro2` are cloned locally and were read for this design.
- Coder anchored edits and reviewer diffs land in this epic.
- Budgets as proposed: 1 KiB per `read_lines` response, ~6 KiB outline page,
  48 KiB exposed source per stage, 12 investigator turns.
- At the turn limit, each stage gets one reserved tool-free call to answer or
  declare insufficient evidence. Cross-attempt continuation notes are deferred.

## Unresolved questions

None blocking. Provider handling of tool-call history in a tool-free final call
is verified during implementation as described in section 8.

## Implementation notes (2026-10-10)

Steps 1-6 are implemented in `tools/synth` (`source_tools.rs`,
`anchored_edits.rs`, `repair_agents.rs`, `repair_trace.rs`, `repair.rs`,
`tests/syn_span_semantics.rs`).

- Contracts use schema v2 and require the source budgets. Manifests and
  ordinary attempt events remain v1, so existing manifests remain readable.
  The source exposure sidecar uses schema v2; v1 readers reject it.
- Investigator turns are pinned at 12; coder and reviewer retain 6 turns.
  Each stage reserves its last call for an answer. A valid reserved answer
  follows normal validation, and the manifest reason names every stage that
  completed after its tool-turn budget.
- A dedicated Neuralwatt HTTP client limits each repair API request to 600
  seconds, accommodating Flex admission waits. There is no whole-stage timer;
  turn, tool-call, and source-byte budgets remain. Confined worker processes
  retain a separate 300-second timeout. A direct provider timeout gets the
  `api_request_timeout` cause and its limit. Rig may normalize a transport
  failure to an `http` report that does not preserve the native timeout flag;
  that path remains distinguishable from a whole-stage deadline by its cause.
- `read_lines` charges numbered text, including blank lines and newlines, to
  its response and stage budgets. Clipped final lines report partial service.
- CRLF anchors match the text shown by `read_lines`, while replacements retain
  the original line terminators. An empty replacement deletes its selected
  lines. If validation rejects an applied patch, edited and new files are
  restored.
- The trace records a payload-free response hash and length. A mock session
  replays outline, search, and reads from a synthetic `protected/source.tar`,
  checking the full tool results against model history and every recorded
  exposure against archived source hashes. Generated outline/search text
  requires the same version of the deterministic tool implementation to replay.

### Verification

Synth: formatting, `cargo check --tests`, and
`cargo clippy --all-targets -- -D warnings` pass. With nightly LLVM codegen,
`cargo test` passes (220 passed, 1 ignored, plus 3 span tests). Root formatting,
`cargo check --workspace`, and `cargo test --workspace` pass. The synth tests
that open local mock-provider sockets were run outside the outer sandbox.

The motivating command was rerun against the same evaluation, case, review,
dataset, and baseline. Attempt `repair-1791630635554636000-42102-0` ended
`model_exhausted` with reason `investigation: session_timeout (limit=120)`.
Its trace shows successful source requests, including outline and range reads
for the formerly blocked large `intent.rs`; there were no failed source
requests or path-guessing loop. A provider model call was still in progress
when the session timed out. This satisfied the named-outcome branch of
criterion 11 but did not establish repair success. After moving the timeout
to each API request and raising it to 600 seconds, the same command ran for
176 seconds in attempt `repair-1791636189372140000-46909-0` before a provider
transport error ended investigation as `model_exhausted`, reason
`investigation: http; tool_budget_exhausted=false`. Seven model calls had
succeeded; the last failed after 21 seconds, far below the request timeout.
The formerly blocked source navigation succeeded again. The trace records
one rejected synthetic probe, whose tool error is separate from source
navigation. This rerun confirms there is no whole-stage 120-second deadline,
but a completed repair remains unverified because the provider connection
failed.

### Criteria coverage

1. `outline_of_large_file_has_correct_spans_and_stays_in_cap`,
   `read_lines_serves_any_range_of_large_file_within_cap`, syn span tests.
2. `read_lines_cap_cuts_on_whole_lines_across_ranges`,
   `overlong_multibyte_line_is_cut_on_char_boundary`,
   `blank_lines_count_toward_read_response_cap`,
   `clipped_final_requested_line_is_reported_as_partial`.
3. `search_is_sorted_has_enclosing_items_and_skips_unreadable_files`.
4. `unparseable_file_gets_fallback_outline_and_still_reads`.
5. `paths_outside_scope_or_checkout_are_denied_and_expose_nothing`.
6. `error_codes_have_stable_names`, `typed_tool_errors_reach_the_model_and_the_trace`,
   plus `invalid_ranges_are_typed`, `not_found_and_not_a_file`,
   `search_query_validation`, budget test.
7. `any_mismatch_rejects_the_whole_patch_and_leaves_files_unchanged`,
   `applies_matching_edits_bottom_up_with_changing_line_counts`,
   `failed_apply_restores_files`, CRLF/deletion/rollback tests.
8. `source_ledger_replays_every_exposure_from_protected_tar`,
   `ledger_records_requests_and_exposure_without_source_text`, including
   the profiled-literal query hash/length assertion.
9. `schema_v1_reader_rejects_v2_events`, v1 manifest parsing and v2 contract
   validation tests.
10. `reserves_one_tool_free_call_after_tool_turns`,
    `tool_enabled_run_is_capped_below_max_agent_turns`,
    `reserved_call_may_declare_insufficient_evidence`,
    `open_questions_are_redacted_and_bounded`,
    `tool_call_or_bad_json_in_reserved_call_ends_with_named_cause`,
    `normal_completion_makes_no_reserved_call`, coder/reviewer mock sessions,
    review `ReviewIncomplete` mapping, and terminal manifest reason test.
11. Live reruns: named `session_timeout` under the old whole-stage limit, then
    named `http` after the per-request timeout change; source navigation
    succeeded in both attempts.
