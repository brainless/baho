# Epic 005: Minimal GUI Prompt Submission

Status: Implemented

Motivating run: None; this is the next minimal workflow slice after Epic 004's
source-table viewer.

User intent: Open one supported CSV in `baho-gui`, type a request in a left
sidebar, click `Submit`, and see the materialized result in the existing data
grid without closing the window.

## Review gate

This epic proposes the GUI request lifecycle, shared run-record boundary, and
smallest useful interaction only. Do not implement it until it has been
reviewed. Implementing this epic does not approve or implement the richer
filter language proposed by Epic 003.

## Summary

Extend `baho-gui <INPUT>` with a fixed-width left sidebar containing one prompt
input, one `Submit` button, and a compact status message. The selected source
table remains open in memory for the life of the window. Each submission runs
the existing deterministic intent recognition, plan validation, and execution
stages against that original opened table and replaces the main grid with the
new materialized result when successful.

Each click is an independent request, equivalent in processing and diagnostic
evidence to a new `baho run <INPUT> --prompt <TEXT>` invocation. It creates one
new `.baho/runs/<run-id>/` record using the existing run-artifact contract.
Submitting a second prompt does not refine, mutate, or consume the first
result. It starts again from the immutable opened source table.

The minimal version supports only the prompt forms that `baho-core` recognizes
at implementation time. It does not add prompt history, conversational state,
LLM calls, background work, cancellation, result tabs, source/result switching,
or the filter semantics discussed in Epic 003.

## Current state and evidence

Epic 004 added the first Akar GUI in commits `e8b17ec`, `e75e497`, and
`158c78a`. The application:

- calls `baho_core::open_table` once before creating the window;
- adapts the resulting `OpenedTable` into a GUI-owned `GridAdapter`;
- stores only that adapter in live application state; and
- gives the data grid the full content area.

The core pipeline is already internally divided into the stages this feature
needs:

```text
run_pipeline(path, prompt)
    -> open_table(path)
    -> run_pipeline_from_opened(opened, prompt)
```

The second operation recognizes the prompt, constructs and validates a plan,
executes it against the opened rows, and returns a `CoreResult` containing the
materialized view, evidence, diagnostics, and events. It is currently private
and consumes `OpenedTable`, so it cannot yet support repeated requests against
one retained source snapshot.

The CLI's run-record implementation is private to the `baho-cli` binary. It
reserves a run ID, writes the manifest and intent, invokes `run_pipeline`,
persists structured artifacts, prints result values to stdout, and renders
terminal errors. The GUI must not depend on or invoke the CLI binary to reuse
this behavior: terminal presentation and argument parsing remain CLI concerns.

Epic 004 intentionally excluded natural-language input and stated that a later
epic could define an explicit GUI run action using the existing run-artifact
contract. This epic is that narrowly scoped follow-up.

## Desired user experience

### Initial state

Running:

```text
baho-gui path/to/source.csv
```

opens the same selected source table as Epic 004. A fixed-width sidebar appears
on the left and the existing virtualized grid fills the remaining width. The
sidebar contains:

- a prompt text input;
- a `Submit` button; and
- one compact status area.

The prompt starts empty. The initial grid is the selected source table, not an
automatically executed result. Passive opening still creates no run record.

The sidebar must remain usable at the minimum supported window size. The grid
may become narrower and retain its existing horizontal scrolling behavior. The
first version need not support resizing or collapsing the sidebar.

### Submitting a request

Clicking `Submit` with non-blank input creates one request. Leading and trailing
whitespace may be used to decide whether the input is blank, but the exact
typed prompt is retained in `intent.txt` and supplied to core recognition
without GUI-side rewriting.

For a successful request:

1. the existing recognizer interprets the prompt;
2. the resulting constrained plan is validated and executed against the
   original opened table;
3. a complete diagnostic run is persisted under a newly reserved run ID;
4. the materialized result replaces the grid contents; and
5. the status reports success and the run ID.

The prompt remains in the input after submission so it can be edited and run
again. A subsequent submission is independent and receives another run ID,
even when its prompt text is identical.

The minimal interaction is button-driven. Enter-to-submit, multiline editing,
prompt history, keyboard shortcuts, and automatic execution while typing are
out of scope.

### Failed request

An unsupported, ambiguous, or otherwise invalid prompt still creates a
completed run record containing its recognition evidence and diagnostics. The
status area shows a concise actionable failure plus the run ID. The prompt
remains editable and the last successfully displayed grid remains unchanged.

Failure must not clear the grid, partially apply a plan, or terminate the GUI.
Full diagnostic detail remains in the run record; the minimal sidebar need not
become a diagnostics browser.

An empty or whitespace-only prompt is rejected locally before a run ID is
reserved. The status explains that a prompt is required.

### Repeated requests and source identity

All submissions in one window target the `OpenedTable` created at startup.
They never use the previous `MaterializedView` as input. This makes request
ordering irrelevant and preserves the CLI's one-request-against-one-source
semantics.

The opened table is an in-memory snapshot. This epic does not watch or reload
the source file. Each run record must identify the same source revision and
content hash that supplied the retained rows; it must not claim that later
bytes on disk were executed if the file changes while the window is open.
A new GUI launch is required to open a changed source.

## Proposed architecture

### Reusable request execution in `baho-core`

Expose a synchronous operation conceptually equivalent to:

```rust
pub fn execute_prompt(opened: &OpenedTable, prompt: &str) -> CoreResult;
```

Exact naming may change, but the operation must borrow rather than consume the
opened table so a session can submit multiple independent requests. It owns the
existing recognition, plan construction, validation, and deterministic
execution sequence. It must not contain GUI state, Akar types, run-directory
allocation, terminal output, or conversational memory.

`run_pipeline(path, prompt)` becomes a compatibility composition:

```text
open_table(path) -> execute_prompt(&opened, prompt)
```

Existing CLI results, diagnostic codes, structured events, plan artifacts,
ordering, and output provenance must remain compatible. The extraction must
not broaden the prompt grammar or change plan semantics.

`OpenedTable` remains the source of truth for execution. The GUI retains it for
the full session in addition to any display adapter. Avoid cloning the entire
table per submission merely to satisfy ownership; cloning bounded metadata for
the result remains acceptable where the current `CoreResult` contract needs it.

### Shared run-record lifecycle

Extract the format-neutral run reservation and artifact persistence needed by
both applications into one reusable, non-UI boundary. A small dedicated crate
is acceptable because it has two immediate consumers; the exact crate name is
an implementation choice. It must not depend on Clap, Akar, winit, wgpu, or
terminal presentation.

The shared API must support the following lifecycle:

```text
reserve a fresh run
    -> persist invocation/source identity and exact prompt
    -> execute a supplied core request against the retained source
    -> persist events, profile, parser config, candidates, recognition
       evidence, plan, diagnostics, and materialized output when present
    -> finalize the manifest as materialized or error
```

Reservation occurs before core request execution so a panic-free handled
failure can still leave a comprehensible run. Existing directories are never
reused, gaps remain valid, and concurrent CLI/GUI submissions cannot reserve
the same ID.

The abstraction should accept application-specific invocation metadata rather
than hard-code CLI arguments. A GUI submission records that it originated from
`baho-gui` and from an explicit submit action. Persisted schema changes are not
required merely to express this if the existing versioned fields can represent
it honestly. If they cannot, schema evolution and compatibility coverage are
required rather than writing false `baho run` arguments.

The shared layer returns structured run identity, outcome, and errors. It does
not print materialized cells or status text. `baho-cli` retains terminal
formatting; `baho-gui` retains sidebar formatting.

The extraction must preserve the current CLI contract, including allocation,
failure observability, artifacts, output-file behavior where supported, and
stdout/stderr behavior. The GUI must not spawn `baho`, shell out, or depend on
the `baho-cli` application crate.

### Source snapshot metadata

Opening a GUI session must retain enough immutable input identity to create
honest later run manifests: supplied and absolute path, size, modification time
when available, and content hash/source revision. Hashing or other source
inspection should not be repeated on every submission when the required
identity is already known from the opened snapshot.

If source identity currently exists only inside CLI recording, move or expose
the smallest reusable domain representation needed by both applications.
Paths and filesystem metadata do not belong in `baho-model` grid values. The
chosen owner must not introduce UI dependencies into core crates.

### GUI session state

`baho-gui` owns a session with at least:

- the immutable `OpenedTable` and its input identity;
- the current prompt string and Akar text-edit state;
- the currently displayed source or materialized grid adapter;
- the last successful materialized view, when any;
- compact idle/success/failure status;
- existing grid navigation and selection state; and
- a guard preventing one click from being processed more than once across
  redraw frames.

The Akar component call reports a click during a frame. The GUI converts that
edge into one pending submission and processes it once outside component
painting. It must not execute the pipeline repeatedly because the window is
redrawn.

Current recognition and execution are synchronous and local, so the minimal
version may execute synchronously. The work must not occur inside the nested
header/body grid rendering loops. The GUI shows a deterministic status before
and after processing where Akar's synchronous frame lifecycle permits it.
Online LLM calls or workloads requiring responsive cancellation would require
a later background-job design and are explicitly out of scope.

### Materialized-view grid adapter

Add a GUI-owned adapter from `MaterializedView` to the existing grid rendering
contract. It must:

- preserve materialized column order and display names;
- render `Value` variants deterministically without terminal-specific
  formatting;
- distinguish absent values internally even if the visual placeholder is
  empty in this minimal version;
- derive stable row keys from materialized row provenance rather than visible
  viewport positions;
- derive collision-free column keys from stable materialized column identity
  or ordinal; and
- submit only visible and overscan cells to Akar.

When the displayed adapter is replaced, reset grid scroll, active-cell, and
selection state to a valid initial state. Do not attempt to preserve a selected
cell across unrelated result schemas in this epic.

The adapter remains pure and testable without a GPU. It must not move plan or
execution logic into `baho-gui`.

### Layout and Akar integration

Replace the one-child full-window layout with a stable horizontal root whose
children are:

```text
+----------------------+-----------------------------------------+
| Prompt sidebar       | Materialized/source data grid           |
|                      |                                         |
| [text input]         | existing virtualized Akar grid          |
| [Submit]             |                                         |
| status               |                                         |
+----------------------+-----------------------------------------+
```

Create and label the sidebar, prompt, submit, status, and grid layout nodes
once with the rest of the page. Use Akar's existing text-input and button
components and caller-owned input state. Keep all Akar-specific types in
`baho-gui`.

Text-input focus and typing must not also drive data-grid keyboard navigation.
Grid navigation continues to work when the grid is focused. This focus-routing
behavior requires explicit interaction coverage because both components share
the same synchronous input snapshot.

Extend the existing development script support only as needed to type into the
prompt, click the labeled Submit button, wait for completion, and capture the
result. Do not create a general GUI automation language.

## Minimal scope

### In scope

- A fixed-width left prompt sidebar in the existing single-window GUI.
- One prompt text input, one `Submit` button, and compact status text.
- Repeated independent submissions against the original opened source table.
- Reuse of the current deterministic recognizer, plan validator, and executor.
- One complete, uniquely allocated run record per non-blank submission.
- Display of the latest successful `MaterializedView` in the existing virtualized
  grid.
- Preservation of the last displayed grid when a request fails.
- Stable result keys derived from columns and source provenance.
- Correct focus separation between prompt editing and grid navigation.
- Pure adapter/session tests and scripted visual verification with synthetic
  CSV data.

### Out of scope

- Any new natural-language grammar or operation semantics.
- Epic 003's data-grounded filters, comparisons, or Boolean predicates.
- LLM calls, provider selection, streamed responses, or agent loops.
- Conversational context, follow-up references, or executing against a prior
  result.
- Prompt history, saved prompts, autocomplete, examples, or suggestions.
- Enter-to-submit, multiline prompt editing, or keyboard shortcuts.
- Multiple documents, tabs, result history, source/result toggles, or undo.
- Background workers, async runtimes, progress reporting, cancellation, or
  concurrent submissions.
- Diagnostics, plan-review, parser-configuration, or schema-editing panels.
- Sidebar resizing, collapsing, docking, or responsive alternate layouts.
- Watching or automatically reloading a changed source file.
- Excel, ODS, PDF, OCR, or other new ingestion support.

## Alternatives considered

### Call `run_pipeline(path, prompt)` on every click

Rejected as the primary design. It would re-read and reclassify the source for
every prompt, could silently switch to changed bytes on disk, and would discard
the prompt-free session boundary introduced for the viewer. The compatibility
function remains useful to CLI callers but should compose the borrowed request
operation.

### Invoke or depend on `baho-cli` from the GUI

Rejected. Argument parsing, stdout/stderr formatting, and process behavior are
application concerns. Spawning the CLI also complicates result transfer,
failure handling, cancellation, and source-snapshot identity.

### Duplicate run persistence inside `baho-gui`

Rejected. Run allocation and artifact schemas are contracts shared across
entry points. Two implementations would drift and could race for IDs.

### Do not create run records for GUI submissions

Rejected. Clicking Submit is an explicit operation, not passive viewing. A run
record preserves the diagnostic development loop, recognition refusals,
validated plans, provenance, and reproducibility promised by the CLI contract.

### Chain each prompt from the previous result

Deferred. It introduces conversational state, result naming, invalidation,
history, and provenance composition. Independent requests against the original
source are deterministic and match current CLI semantics.

### Run request execution on a background thread now

Deferred. The implemented recognizer and executor are synchronous and local.
A correct background design would need ownership, cancellation, shutdown, and
completion delivery policies that are unnecessary for this minimal slice.

## Acceptance criteria

- `baho-gui <synthetic.csv>` opens with a left sidebar and the original selected
  source table in the remaining grid area.
- Typing a currently supported prompt and clicking `Submit` exactly once creates
  exactly one new monotonically allocated run directory.
- A successful request displays the same logical `MaterializedView`, plan,
  recognition evidence, diagnostics, and core event sequence as the equivalent
  CLI request against the same source snapshot.
- A second prompt in the same window executes against the original opened table,
  replaces the grid on success, and creates a distinct run ID.
- Repeating identical prompt text creates another distinct run and produces the
  same logical result without accumulating hidden conversational state.
- An unsupported or ambiguous prompt produces a finalized error run, shows an
  actionable status, preserves the prompt, and leaves the prior grid unchanged.
- An empty or whitespace-only prompt creates no run and reports that a prompt is
  required.
- Redraws, pointer movement, and button pressed/hover states cannot duplicate a
  submission.
- While the prompt input is focused, typed characters and editing keys do not
  navigate the grid. Grid keyboard navigation still works when grid focus is
  active.
- Result row and column keys remain stable across scrolling and do not derive
  from visible indices alone. Replacing a result resets invalid grid state.
- Materialized missing, blank, text, numeric, and Boolean values render
  deterministically and do not panic.
- Passive GUI opening continues to create no run record.
- The GUI neither shells out to `baho` nor depends on CLI presentation code.
- Existing CLI commands retain their output, exit behavior, artifact layout,
  and concurrent run-ID safety after run-record extraction.
- Scripted synthetic captures cover initial source view, typed prompt, successful
  result, and failed-request status. No user document or generated capture is
  committed.
- `cargo fmt --check`, `cargo check --workspace`, and `cargo test --workspace`
  pass. Run the warning-clean clippy command if it is part of the workspace
  baseline at implementation time.

## Fixture and test strategy

Use synthetic CSVs already representative of supported prompt behavior. Do not
copy a user document or `.baho` run into tests.

Core coverage:

- executing two supported prompts against one borrowed `OpenedTable` produces
  independent results;
- request execution matches `run_pipeline` for successful and refused prompts;
- executing a request does not mutate opened rows, columns, parser metadata, or
  source revision; and
- output provenance continues to refer to original source coordinates.

Shared run-record coverage:

- CLI and GUI invocation metadata are represented honestly;
- each reservation is unique under concurrent allocation;
- successful and failed supplied `CoreResult` values produce the expected
  artifact index and finalized manifest;
- exact prompt bytes are retained in `intent.txt`;
- a retained source identity is used without silently re-identifying changed
  bytes; and
- existing CLI run-artifact tests continue to pass through the shared layer.

GUI pure coverage:

- materialized columns, values, provenance keys, and missing cells adapt
  correctly;
- replacing a display adapter resets selection and active-cell state;
- blank submission is rejected without enqueueing work;
- one click edge creates one pending submission; and
- success replaces the display while failure preserves it and updates status.

GUI process and visual coverage:

- passive startup does not allocate a run;
- a scripted prompt and click allocate one run and display the expected result;
- a second scripted submission allocates one more run;
- a refused prompt leaves the prior result visible and shows failure status;
- layout dumps give non-zero stable rectangles for sidebar, prompt, Submit, and
  grid; and
- frame dumps confirm the narrowed grid remains clipped and virtualized.

Generated run records, screenshots, and frame/layout dumps remain ignored local
artifacts.

## Tasks

### Task 1: Characterize the existing request and run contracts

- Add or tighten tests that establish current `run_pipeline` success, refusal,
  event ordering, plan evidence, provenance, and CLI artifact behavior.
- Record the exact Akar revision and verify its text-input, button, focus, and
  data-grid APIs against the local checkout before UI changes.
- Confirm the behavior to preserve in focused tests before refactoring.

### Task 2: Expose repeatable core request execution

- Extract the private prompt-to-result portion into a public borrowed
  `OpenedTable` operation.
- Keep `run_pipeline` as open-then-execute composition.
- Avoid full table cloning per submission.
- Add parity, repeatability, immutability, refusal, and provenance tests.

### Task 3: Extract reusable run recording

- Define a UI-neutral invocation and immutable source-identity representation.
- Move run reservation, manifest finalization, event/artifact persistence, and
  result serialization into one shared boundary.
- Allow callers to supply an already opened snapshot and resulting core request
  without forcing another parse.
- Preserve failure records and atomic run-ID allocation.
- Adapt `baho-cli` to the shared boundary while keeping terminal formatting in
  the CLI.
- Run the existing CLI end-to-end and concurrent allocation tests.

### Task 4: Add the materialized-view grid adapter

- Adapt materialized columns, typed values, missing cells, and provenance into
  the GUI grid contract.
- Define deterministic value formatting and collision-free keys.
- Reset grid navigation state when replacing adapters.
- Cover the adapter and state transition logic with GPU-free unit tests.

### Task 5: Add persistent GUI session and submission state

- Retain `OpenedTable` and source identity after startup.
- Add caller-owned prompt text/edit state, pending-submit guard, compact status,
  and last successful display state.
- Reject blank prompts locally.
- On Submit, execute and record exactly one request, then replace or preserve
  the grid according to the outcome.
- Ensure repeated requests always target the original opened table.

### Task 6: Build the minimal sidebar

- Change the stable page layout to a fixed-width sidebar plus flexible grid.
- Add and label the Akar text input, Submit button, and status nodes.
- Route focus so prompt editing and grid navigation do not consume the same key
  events.
- Keep request execution outside the grid cell rendering loops and prevent
  redraw-driven duplicate submissions.

### Task 7: Extend deterministic GUI verification

- Extend the script runner only enough to focus/type a prompt and click Submit.
- Add synthetic successful, repeated, blank, and refused submission scenarios.
- Inspect screenshots plus layout/frame dumps for sidebar layout, focus behavior,
  result replacement, failure preservation, and bounded grid drawing.
- Keep all generated artifacts out of Git.

### Task 8: Run regression and workspace verification

- Run focused core, run-record, CLI, and GUI tests first.
- Run `cargo fmt --check`, `cargo check --workspace`, and
  `cargo test --workspace`.
- Re-run the equivalent CLI and GUI requests on the same synthetic source and
  compare structured plans, diagnostics, outputs, and run artifacts.
- Update this epic's status consistently after implementation and record any
  platform limitation affecting visual verification.

## Remaining decisions for review

These choices are intentionally small but should be confirmed before coding:

- The fixed sidebar width and minimum supported window width.
- The exact deterministic formatting of numeric and Boolean materialized
  values in the GUI.
- The shared run-record crate/module name and whether existing persisted
  invocation fields can honestly encode a GUI submit action without a schema
  version change.
- Whether status displays only `Run <id> materialized/failed` plus the first
  error, or another equally bounded single-message form.

None of these decisions should expand the epic into prompt history,
conversational execution, richer intent recognition, or asynchronous work.

## Implementation verification

Task 8 verification used the same synthetic CSV and prompt in isolated CLI and
GUI working directories. The persisted plan and recognition evidence,
diagnostics, candidates, parser configuration, input profile, materialized
output, source hash, artifact index, and ordered core event sequence matched.
The manifests differed only in the expected application-specific invocation
metadata (`baho run` versus `baho-gui submit`). A scripted macOS capture showed
the fixed sidebar, retained prompt, success status, and materialized grid.

`cargo fmt --check`, `cargo check --workspace`, and all focused Epic 005 crate
tests passed. The full workspace test run reached and passed the Epic 005
crates, but the unrelated `baho-llm` Xiaomi configuration test could not create
a macOS SystemConfiguration dynamic-store object in the sandbox. Visual
verification remains platform-dependent: it requires a windowed GPU adapter;
there is no headless/offscreen fallback on macOS.
