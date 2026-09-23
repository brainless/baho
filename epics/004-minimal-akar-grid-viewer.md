# Epic 004: Minimal Akar Grid Viewer

Status: Proposed

Motivating run: None; this is a product-sequencing decision to expose the
existing CSV workflow in a minimal desktop viewer before extending parser
intent behavior from Epic 003.

User intent: Launch a baho GUI binary with a CSV, Excel, or PDF path and view
the supported document as a minimal data grid. CSV is the only format that
must be processed in this epic; Excel and PDF must be rejected clearly and
must not be presented as successfully loaded.

Akar reference: local checkout `~/Projects/akar` at commit
`0d33253648d5f6c04b2d6a6b5ecbbaa3c97b0ea6` (`epic 027: virtualized data grid
MVP`).

## Review gate

This epic proposes the GUI boundary and implementation slice only. Do not
implement it until it has been reviewed. Epic 003 remains proposed and is not
a prerequisite for this work.

## Summary

Add an `apps/baho-gui` Rust binary, invoked as:

```text
baho-gui <INPUT>
```

For a supported CSV-like input, the application will synchronously run baho's
existing inspection, candidate detection, candidate selection, header
construction, and selected-region classification stages. It will then open an
akar window whose primary content is the selected table rendered with akar's
new virtualized data-grid component.

The first GUI is deliberately a viewer, not a second workflow engine. It does
not accept a natural-language prompt, construct or execute a plan, write an
output file, or implement parser/schema editing. It reuses a new prompt-free
core opening operation so that the CLI and GUI share the same ingestion and
table-selection behavior.

The binary accepts any input path syntactically. CSV, TSV, and supported
delimiter-separated text are the only formats processed. Recognized Excel and
PDF inputs produce a specific unsupported-format error rather than being fed
to the CSV parser. Other unclaimed inputs produce a format-detection error.

## Motivation and current state

The current workspace has a useful deterministic CSV path but no GUI crate.
The root workspace contains `apps/baho-cli` and the reusable domain crates
described in the PRD; `apps/baho-gui` is still only a planned boundary.

`baho_core::run_pipeline(path, prompt)` currently performs all stages in one
function:

1. detect CSV dialect and import a bounded analysis surface;
2. detect and select a table candidate;
3. construct the header and stream/classify the selected region;
4. recognize intent and compile a plan;
5. validate and execute the plan; and
6. return the materialized result and diagnostics.

That function cannot be used by a source viewer without supplying a fake
prompt, and its returned `CoreResult` does not expose the selected source rows
as a reusable table. Reimplementing the first stages in `baho-gui` would put
parser orchestration and domain rules in the UI crate, contrary to the crate
boundaries.

The CSV importer also documents an important memory boundary: its initial
`Document` contains only the bounded analysis surface. The full selected
region is reparsed after candidate selection. The GUI therefore cannot treat
the imported `Document` as the complete source grid.

## Akar findings

Akar's latest commit adds the component this viewer needs:

- fixed-height virtualized rows and caller-sized horizontally virtualized
  columns;
- a sticky header and two-axis scrolling;
- visible row and column ranges with a small overscan;
- stable cell identity based on caller-provided row and column keys;
- pointer activation, caller-owned row selection, and keyboard navigation;
- single-line clipped text cells with left, center, or right alignment; and
- a synchronous `data_grid_begin` / header / body / `data_grid_end` lifecycle.

The application owns the records, cell strings, column widths, sorting,
filtering, and selected-row policy. Akar owns grid geometry, virtualization,
clipping, drawing, input hit-testing, and active-cell navigation. It creates
one ordinary layout node for the viewport rather than one Taffy node per cell.

The canonical Rust integration is
`~/Projects/akar/examples/data-grid-rust`. Akar does not own a window or event
loop; `baho-gui` must own the winit application lifecycle and drive akar
synchronously. No async runtime or second event loop is required.

The referenced akar API is pre-alpha. Implementation must verify the local
checkout is compatible with the commit above before adapting the example. Any
API drift should be handled in `baho-gui`; it must not leak akar types into
`baho-core` or other domain crates.

## Desired user experience

### Successful CSV opening

Running:

```text
baho-gui path/to/source.csv
```

opens one native window. The window title identifies baho and the input file
name. The selected table occupies the available content area and provides:

- a sticky header using baho's normalized display names;
- vertically and horizontally virtualized scrolling;
- alternating row styling and grid lines from the akar theme;
- pointer activation and single-row selection;
- keyboard navigation supplied by the akar data grid; and
- raw source cell text, including empty and malformed-looking values, without
  GUI-side coercion.

The first displayed order is source order. Row keys derive from zero-based
source row coordinates, and column keys derive bijectively from source column
ordinals. They must not derive from visible screen positions. Selection and
active-cell identity therefore remain attached to source data when the
viewport scrolls.

The viewer displays the one table candidate selected by the existing core
policy. It does not display the entire physical CSV surface in this epic:
preamble rows, internal blank separators classified as non-data, and rejected
footer rows are omitted consistently with the selected table used for
execution. This is a table viewer, not yet the PRD's eventual physical source
grid and parser/schema editor.

### Failure behavior

The application must distinguish at least these startup failures:

- input path missing or not a regular readable file;
- recognized Excel workbook (`.xlsx`, `.xls`, or `.ods`) not yet supported;
- recognized PDF input not yet supported;
- unrecognized format;
- CSV encoding or dialect failure;
- no table candidate meeting the configured threshold;
- ambiguous table selection; and
- malformed or over-limit content in the selected region.

Failures print a concise message to stderr and return a non-zero exit status.
No window is required when opening fails before GUI state exists. This keeps
the first slice small and testable while ensuring `baho-gui report.xlsx` and
`baho-gui report.pdf` never masquerade as CSV success.

The distinction between recognized-but-unimplemented Excel/PDF and unknown
input must be represented by typed core/ingest errors or stable diagnostics,
not by string matching in the GUI.

## Proposed architecture

### Prompt-free source opening in `baho-core`

Extract the source-facing stages of `run_pipeline` into a synchronous core
operation with a conceptual contract such as:

```rust
pub fn open_table(path: &Path) -> Result<OpenedTable, OpenTableFailure>;
```

Exact names may change during implementation, but the returned domain value
must provide the GUI and later pipeline stages with:

- source revision and sheet index;
- selected table ID and candidate metadata;
- ordered column definitions with source ordinals and display names;
- ordered selected data rows;
- each row's original zero-based source row index;
- raw cell values aligned to the selected columns;
- input profile and parser configuration;
- bounded candidate/classification evidence where already produced;
- diagnostics and structured core events from the opening stages.

The value is a core/domain DTO and contains no akar, winit, wgpu, layout,
color, or pixel types. Raw input evidence remains separate from interpreted
values. A short or ragged record represents missing cells explicitly; the UI
must not silently turn parser failures into nulls or shift later columns.

`run_pipeline` is then expressed as:

```text
open selected table
    -> recognize intent
    -> compile and validate plan
    -> execute
```

The existing CLI behavior, diagnostics, event ordering, plan artifacts, and
materialized output must remain compatible. This is an orchestration
extraction, not a parser redesign.

The opened table may be eagerly retained in memory for this MVP. The existing
execution path already materializes the complete selected region, and akar
virtualizes rendering rather than storage. The implementation must document
this boundary and must not claim that the GUI supports unbounded or lazy data
sources. A future source-provider API may replace the eager row vector without
changing akar's visible-range rendering loop.

### Format dispatch

Opening must pass through an explicit format-dispatch boundary rather than
instantiating `CsvImporter` unconditionally. Only the CSV importer is
registered in this epic. Detection should use bounded leading bytes plus
extensions/signatures so binary Excel/PDF inputs receive a typed unsupported
format result before CSV dialect detection.

This work may refine `ImportRegistry` or add a small core-level format kind,
but it must not add placeholder Excel/PDF crates or parsers. Format detection
belongs in `baho-ingest`; CSV heuristics remain in `baho-ingest-csv`; the GUI
only renders the resulting error.

### `baho-gui` ownership

`apps/baho-gui` owns:

- command-line argument parsing for one input path and development capture
  flags;
- synchronous startup and typed error presentation;
- the winit window, surface, device, queue, and event loop;
- akar core, layout, theme, and data-grid state;
- adaptation of an `OpenedTable` into grid column descriptors, stable numeric
  keys, and display strings;
- caller-owned selected-row state; and
- application-level screenshot/debug plumbing used to verify the UI.

It must not own delimiter detection, candidate scoring, header normalization,
row classification, provenance rules, intent recognition, plan construction,
or execution.

The GUI depends on `baho-core` for document opening and on akar's Rust crates
for presentation. `baho-core` must remain usable without akar, wgpu, winit, or
an async runtime.

### Grid adapter

The adapter constructs one `DataGridColumn` per selected source column. For
the first slice:

- column keys are a checked, collision-free encoding of source column
  ordinals rather than hashes of display names;
- row keys are a checked, collision-free encoding of source row indices;
- every visible cell is looked up by logical row and source-column ordinal;
- present cells display `raw_text` exactly;
- structurally missing cells display an empty string while remaining distinct
  in the domain data;
- all columns are left-aligned because schema typing is not yet established;
- widths use one deterministic policy with a minimum and maximum bound,
  computed once at startup from header text and a bounded row sample; and
- header clicks report interaction but do not sort in this epic.

Only `response.visible_rows` and `response.visible_columns`, including akar's
overscan, are submitted each frame. Rendering work and text shaping must be
proportional to visible cells, not total rows times total columns.

The adapter should be testable without a GPU: key construction, raw-cell
lookup, ragged-row handling, width policy, and selected-row transitions are
pure application logic.

### Window and frame lifecycle

Follow akar's construct/compute/paint contract and its full-screen data-grid
example:

1. Parse the input and create the `OpenedTable` synchronously.
2. Create the winit window and wgpu surface/device/queue.
3. Construct a stable page and one full-size data-grid layout node once.
4. On each redraw, begin the akar frame and compute layout.
5. Call `data_grid_begin` with persistent caller-owned state.
6. Paint visible header cells within the header scope.
7. Paint visible source cells within the body scope.
8. End the grid, handle keyboard navigation, and finish/present the frame.
9. Feed winit events through `akar_winit::process_window_event` and request
   redraws through the developer-owned loop.

No layout nodes are created per record or cell, and no expensive parsing,
column measurement, or display-string allocation occurs in the redraw loop.

## Diagnostics and run records

Opening a viewer is not a `baho run` operation. It has no prompt, plan, or
materialized output, so this epic does not create a `.baho/runs/<run-id>/`
directory merely because a file was viewed.

Core opening diagnostics remain available in the returned `OpenedTable` for
future UI presentation, but the MVP need not add a diagnostics panel. Fatal
opening diagnostics are rendered to stderr. Non-fatal diagnostics may be
summarized on stderr during startup and must not be silently discarded by the
core API.

This decision avoids creating a second, underspecified run-record lifecycle.
A future epic may define viewer sessions or an explicit “run this request” GUI
action that uses the existing run artifact contract.

## Akar dependency and visual development loop

The repository already uses a sibling local checkout for `llm-sdk`; the GUI
may follow that development pattern for akar's Rust crates. Before
implementation, record the actual akar revision and ensure it contains the
data-grid API from commit `0d33253`. Do not edit the akar checkout as part of
this epic.

Because akar is pre-alpha, keep its types confined to `apps/baho-gui`. A
future dependency-source decision may replace sibling path dependencies with
a pinned Git revision or published packages without changing core APIs.

Coding agents implementing or debugging this epic should use akar's own
agent-oriented visual debugging capabilities as needed rather than relying on
manual observation alone. The expected loop is to capture what akar rendered,
drive non-idle states with scripted input, inspect layout and recorded draw
calls, and compare captures when investigating a regression. In particular,
agents may use:

- akar's intermediate-texture screenshot capture, which captures the rendered
  surface without OS window chrome;
- scripted pointer, scroll, and keyboard input for reproducible interactive
  states;
- `--dump-layout` to inspect stable labeled layout rectangles;
- `--dump-frame` to inspect draw calls, scissors, culling, z-order, and input
  state; and
- akar's `akar-diff` utility for same-machine visual diffs or pixel-change
  checks when a stable baseline is useful.

The implementation should reuse or adapt the relevant support from akar's
`demo-rust` and `data-grid-rust` examples. It must not modify the akar checkout
to add baho-specific debugging behavior. When a visual issue cannot be
explained from a screenshot, agents should consult the frame dump and layout
dump before changing application or parser behavior.

The GUI should expose a small development-only-compatible command surface
modeled after akar's examples:

```text
baho-gui <INPUT> [--screenshot <PNG>] [--delay <SECONDS>] [--exit]
                [--dump-layout] [--dump-frame <JSON>] [--script <FILE>]
```

`--script` only needs the interactions used by checked-in GUI fixtures:
scrolling, clicking a known grid cell, keyboard movement, delay, and
screenshot. It does not need to become a general automation framework. Debug
outputs and generated screenshots belong under ignored local artifact paths,
not in `.baho/` or committed user-data locations.

## Scope

### In scope

- A new `baho-gui` workspace application and binary.
- One positional input path.
- CSV/TSV/delimiter-separated text opening through the existing parser.
- A reusable prompt-free core opening stage shared with `run_pipeline`.
- Existing deterministic candidate selection and selected-region semantics.
- A full-window akar virtualized grid with sticky headers and two-axis scroll.
- Raw text display, stable source-derived row/column identity, single-row
  selection, cell activation, and akar keyboard navigation.
- Typed, distinct unsupported-format behavior for Excel/PDF.
- Testable startup failures and pure grid-adapter behavior.
- Deterministic screenshot, input-script, layout-dump, and frame-dump support
  sufficient for agent-led visual verification.
- Synthetic CSV fixtures only.

### Out of scope

- Excel, ODS, PDF, OCR, or image ingestion.
- Displaying a workbook's sheet tabs or a PDF's pages.
- The entire physical source surface, preamble/footer overlays, or candidate
  switching.
- Natural-language input, Epic 003 filtering, LLM calls, plan review, or
  execution controls.
- Parser configuration, schema editing, diagnostics panels, operation lists,
  or materialized-view tabs.
- Opening files through a native picker, drag and drop, recent-files lists, or
  multiple windows/documents.
- Sorting, filtering, column resizing/reordering/pinning, or saved layout.
- Inline cell editing, clipboard operations, range selection, formulas, or
  arbitrary custom cell components.
- Async/background loading, progress UI, cancellation, lazy paging, or a
  daemon.
- Creating `.baho` run artifacts for passive viewing.
- Accessibility completion; the component's keyboard navigation must not be
  described as full accessibility support.

## Alternatives considered

### Pass a dummy prompt to `run_pipeline`

Rejected. It would execute an unrelated operation, could fail during intent
recognition even when source opening succeeded, and would make the GUI display
a materialized projection instead of the selected source table.

### Reimplement parsing orchestration inside `baho-gui`

Rejected. It would duplicate candidate selection, header construction, row
classification, diagnostics, and future parser changes in the presentation
crate.

### Display the bounded imported `Document` directly

Rejected. The importer intentionally retains only the bounded analysis
surface. Presenting it as the whole document would truncate larger CSVs and
misrepresent the current ingestion contract.

### Display every physical CSV record

Deferred. A physical-source grid is valuable for future parser/schema editing,
but it requires a random-access or paged source surface, visual distinction
between parser roles, and policy for ragged widths. The current selected table
is already materialized for execution and provides the smallest coherent GUI.

### Add Excel/PDF placeholder importers

Rejected. Placeholder importers blur unsupported-format diagnostics and create
crates with no implemented behavior. Explicit typed refusal is sufficient.

### Copy akar's fake-data example and replace only its records

Rejected as an architecture. The example is the right reference for window,
frame, data-grid, and debug-tool integration, but baho's application state
must be derived from a core-owned opened table with source provenance and
typed startup failures.

### Depend on akar through its C ABI from Rust

Rejected for this Rust application slice. Akar's Rust example provides the
canonical in-repository integration and avoids an unnecessary FFI layer. The
C ABI remains akar's cross-language contract, while dependency volatility is
contained in `apps/baho-gui`.

## Acceptance criteria

- `baho-gui <synthetic.csv>` opens a native window whose content is the table
  selected by the same parser configuration and candidate-selection policy as
  the CLI.
- The sticky header uses normalized baho column display names and body cells
  show raw source text in source-row order.
- A synthetic CSV with a preamble, ragged rows, internal blank separators, and
  a footer proves that displayed rows and source-derived keys follow the
  selected-table contract rather than visible screen positions.
- Vertical and horizontal scrolling render only akar's visible/overscan row
  and column ranges. A counted test demonstrates that fixed-viewport render
  submissions do not grow with total logical row count.
- Pointer selection and keyboard movement update the active source row/cell
  without changing parser data or losing identity after scrolling.
- Empty, zero-column, and zero-area states do not panic. An empty or
  non-table CSV reports a typed opening failure instead of opening a false
  successful grid.
- Missing files, unreadable paths, malformed/over-limit CSVs, ambiguous table
  candidates, and unsupported encodings fail with actionable errors.
- Excel/ODS and PDF inputs are identified as recognized but unsupported and
  are never passed through the CSV pipeline.
- The existing CLI fixture behavior, plan artifacts, diagnostic codes, event
  ordering, and materialized results remain covered after the core
  orchestration extraction.
- Passive GUI opening creates no `.baho/runs` directory.
- A deterministic synthetic fixture is exercised through screenshot and
  scripted scroll/selection/keyboard captures. The captures are inspected
  visually; frame dumps confirm header/body clipping and bounded cell draws.
- No user document, raw run artifact, screenshot of user data, or generated
  debug output is committed.
- `cargo fmt --check`, `cargo check --workspace`, and
  `cargo test --workspace` pass. The GUI implementation also runs the
  appropriate warning-clean clippy check if the workspace has adopted it by
  implementation time.

## Fixture and test strategy

Use small synthetic fixtures that cover the contracts rather than real user
documents:

1. A regular table for basic headers, raw cells, selection, and navigation.
2. A report-shaped CSV with preamble, internal blanks, a ragged record, and a
   footer to prove selected-region behavior and source-row identity.
3. A wide table whose columns require horizontal scrolling.
4. A tall generated table used to compare render-submission counts at fixed
   viewport dimensions.
5. Empty, malformed, ambiguous-table, Excel-signature/extension, and
   PDF-signature inputs for startup failures.

Test layers:

- `baho-core` unit/integration tests for `open_table`, typed failures,
  diagnostics, provenance, and parity with `run_pipeline`.
- Existing `baho-cli` end-to-end tests to protect run artifacts and output.
- `baho-gui` pure tests for key mapping, width policy, ragged lookups, visible
  range adaptation, and selected-row state.
- GUI process tests for argument validation, failure exit status, and absence
  of `.baho` writes.
- Local GPU visual tests using `--screenshot`, `--script`, `--dump-layout`,
  and `--dump-frame`. Do not make live-GPU availability a normal CI
  requirement.

Screenshot fixtures must use synthetic, non-sensitive values and a fixed
viewport, theme, bundled font source, and scale factor where the platform
allows it. Generated PNG/JSON artifacts remain gitignored unless a later
review explicitly approves stable baselines.

## Implementation outline

1. Record pre-change workspace checks and the exact local akar revision. Read
   the current data-grid example and API again if the checkout has moved.
2. Add typed format classification/refusal coverage so Excel/PDF cannot be
   claimed by the CSV importer.
3. Extract the prompt-free selected-table opening stages from
   `run_pipeline`, introduce the core domain result/error types, and preserve
   existing CLI behavior with characterization tests.
4. Add focused core tests for source rows, raw values, raggedness,
   diagnostics, candidate ambiguity, and large selected-region behavior.
5. Add `apps/baho-gui` to the workspace with its CLI arguments and sibling
   local akar dependencies confined to that application crate.
6. Implement the pure opened-table-to-grid adapter and its tests.
7. Implement the synchronous winit/wgpu/akar lifecycle with one stable
   full-window grid node, visible-range cell submission, selection, and
   keyboard navigation.
8. Add bounded screenshot/script/layout/frame debug support based on akar's
   data-grid example.
9. Run the GUI against the synthetic fixtures, inspect initial, scrolled,
   selected, keyboard-active, wide, and failure states, and reconcile visual
   evidence with layout/frame dumps. Use akar's `akar-diff` where a
   same-machine before/after comparison helps identify or guard a visual
   regression.
10. Run focused tests followed by workspace formatting, checking, and tests;
    document any platform-specific visual verification limitation.

## Remaining design questions

These do not block approval of the behavior above, but implementation should
record the chosen answers:

- Whether the development dependency remains a sibling path to akar or is
  replaced by a pinned Git/source policy before the GUI is committed. The
  core/UI boundary must support either choice.
- Whether non-fatal parser diagnostics should be summarized only on stderr or
  in a one-line status area below the grid. A diagnostics panel remains out of
  scope either way.
- The exact deterministic column-width constants and bounded sample size.
  They should optimize initial readability without implying auto-fit or
  adding work to the render loop.
