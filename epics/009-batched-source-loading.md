# Epic 009: Batched Source Loading

Status: Proposed

Motivating run: None supplied. The user observed a short delay when opening a
3.2 MB CSV in the GUI. No source file, timing trace, or run artifacts were
provided; the observations below come from the current code paths.

User intent: Open and browse large CSV tables without retaining every source
cell in the GUI, and make the CLI and GUI use one shared, variable-size row
loading path. Keep table and header selection correct for irregular CSVs and
leave a format-independent contract for future spreadsheet adapters.

## High-level purpose

Separate *identifying a table* from *reading its rows* and *rendering its
cells*. Both applications should use the same selected-table metadata and
bounded row reader. The GUI should request only rows needed for its visible
grid and a small overscan/cache. The CLI should consume batches for execution
instead of first copying the entire source table into memory. Complete results
and operations that inherently need growing state may still use memory or
explicit temporary storage.

The intended outcomes are lower retained source memory, fewer copies, and a
responsive GUI open and scroll path. A faster initial open also requires
deciding when full-file diagnostics and hashing run; batching alone does not
remove those scans.

## Current behavior and problem

- `baho-core::open_table` detects a CSV dialect, imports bounded candidate
  evidence, selects a table, constructs its header, then calls
  `read_selected_region` and retains all selected data rows in `OpenedTable`.
- Candidate detection examines the retained prefix (1,000 records by default)
  and scores possible header/body pairs. Candidates currently span columns
  `0..header_width`; vertically stacked tables can be considered, while
  horizontally adjacent tables are not segmented. A row-zero header rule
  would regress preambles and multiple-table cases.
- CSV inspection streams over the whole file to count and diagnose records,
  though it retains only a bounded sample. Dialect detection reads a bounded
  prefix. Import computes a full-file hash. CLI run recording also hashes the
  input before executing the same core open path.
- `GuiSession` keeps `OpenedTable` and `GridAdapter` clones its cell values.
  The grid already submits only `visible_rows × visible_columns`, but it
  constructs a key vector for every row each frame. A synchronous dropped-file
  open runs on the window event path.
- The CLI creates a complete `GridInput` from `OpenedTable`, and `baho-exec`
  clones rows into a working vector. Grounding already has a separate
  `SelectedTableStream`, showing that selected rows can be reread, but it
  depends on the retained row list and is not the common source API.

These facts explain likely sources of delay and memory use; they do not
identify the dominant cost of the user's particular file without a trace.

## Desired behavior and scope

1. Keep candidate selection before header construction. Expose selected
   region, header, columns, source identity, and diagnostics as common core
   metadata. Do not assume that the first CSV record is a header. Any future
   explicit flat-CSV mode is a separate parser-policy decision.
2. Define a format-independent selected-row contract in `baho-ingest` or
   `baho-core`: open a source snapshot, read the next caller-sized batch, and
   provide stable source row/column coordinates and raw cell distinctions
   (`Missing` versus an explicit empty field). The CSV adapter owns CSV record
   boundaries, classification, and seeking/indexing details.
3. Make the GUI hold metadata plus a bounded row cache. Request the visible
   row range with overscan and render only the returned visible cells. Preserve
   stable row identity, keyboard navigation, selection, and horizontal scroll.
4. Make CLI execution consume the same batches. Stream operations where the
   semantics allow it. Distinct, sort, aggregate, and complete materialized
   outputs may require growing state; define an explicit bound or spill policy
   instead of claiming constant memory for them.
5. Preserve the CLI's complete run record and source hash. Decide separately
   whether GUI opening can show a provisional table before a full scan and
   hash finish. The GUI must not label incomplete diagnostics or an unverified
   source revision as complete.

Out of scope: implementing Excel/ODS/PDF import, redesigning table scoring,
adding side-by-side CSV table detection, changing plan semantics, or storing
user source documents in `.baho/`. The shared contracts should accommodate
future spreadsheet adapters without moving CSV heuristics into core.

## Proposed design

### Selection and source access

Introduce an opened-source handle containing the selected `GridRegion`,
`HeaderDecision`, column definitions, parser configuration, source identity,
and bounded evidence. Selection remains an observable stage. A row cursor
returns batches up to a requested row count and a configured byte/field
limit. It yields typed errors and diagnostics with source coordinates.
Consumers do not need a `Vec<OpenedRow>` to get the header or start reading.

Sequential reading is the baseline for CLI execution and for building an
optional CSV row index. For GUI jumps and backward scrolling, use a bounded
cache plus a sparse, source-revision-bound record index or checkpoints. The
index must respect quoted newlines and logical CSV records. Its exact format
and interval are implementation details to benchmark; never infer offsets by
counting newline bytes. If the index is incomplete, the GUI may show a
determinate loading state while extending it. Source changes invalidate the
index and cached rows rather than mixing revisions.

Header decisions are common domain data in `baho-model`, coordinated by
`baho-core`. CSV-specific candidate detection and header normalization stay
in `baho-ingest-csv`. A future spreadsheet importer can produce the same
selected-region and header contract from sheet coordinates using its own
inspection rules.

### Execution and presentation

Refactor `baho-exec` to consume batches or a row iterator through a format-free
input interface. Preserve plan validation, typed parsing, provenance,
deterministic ordering, and stable diagnostics. Operations that require a
complete set of rows must state their memory budget and either spill to
temporary storage or fail with a typed resource-limit diagnostic. Avoid
rebuilding full raw and typed copies before an operation starts.

The GUI grid remains virtualized for drawing. Replace its all-row cell copy
with a visible-range cache and avoid rebuilding all row keys each frame.
Viewport sizing and navigation need a row-count contract: exact when a full
scan/index is complete, and explicitly provisional while it is not. Bounded
prefetch may load more than the visible range but must not grow with total
table size.

### Full-file work and observability

Keep CLI hashing and complete inspection before finalizing a run, as required
by `PRD.md`. Reduce duplicate passes where possible, with identical profile,
diagnostic, and hash results. For the GUI, measure open-stage timings before
choosing between a background worker, incremental work across frames, or a
synchronous scan. Akar retains one synchronous frame loop; no mandatory async
runtime enters core. If opening becomes progressive, show the state and
publish final diagnostics/source identity only after verification.

Record bounded, structured events for selection, scan completion, index
progress, batch reads, cache behavior, and execution resource limits where
they help diagnose a run. Do not log cells or add per-frame run artifacts.
Persisted artifact changes require versioning and compatibility coverage.

## Alternatives and tradeoffs

- **Use the first row as the header:** fast for regular CSVs, but wrong for
  preambles and multi-table files. Keep the current candidate policy; an
  explicit simple-file mode can be proposed separately.
- **Only remove the GUI's duplicate cell copy:** worthwhile early reduction,
  but it leaves full selected-table retention and CLI copies intact.
- **Read a batch by rescanning from byte zero:** simple and memory bounded,
  but backward scroll and distant jumps can become slow. Start with a
  sequential cursor and add measured sparse indexing for random access.
- **Build a complete row offset index at open:** makes later navigation cheap
  but still imposes an initial full scan and index memory. Progressive or
  sparse indexing balances open latency against jump latency.
- **Make every operation fully streaming:** some plan steps require global
  state. Use explicit per-operation bounds or temporary storage instead.

## Acceptance criteria

1. GUI and CLI select the same table and header as the current pipeline for
   synthetic preambles, vertically stacked tables, blank regions, ragged
   rows, and footer notes. Ambiguous or missing candidates keep actionable
   diagnostics. Horizontally adjacent tables remain an explicit limitation.
2. The shared source reader returns caller-sized batches in stable source
   order, preserves raw values and coordinates, distinguishes missing from
   empty cells, and handles quoted newlines and malformed/oversize fields.
3. GUI retained source-cell memory and per-frame source-row work are bounded
   independently of total file rows for a fixed viewport/cache policy.
   Counted tests confirm that draw submissions stay within visible plus
   overscan ranges; scrolling and keyboard navigation retain row identity.
4. CLI and GUI both use the batch reader. For streamable plans, execution
   begins without materializing all source rows, and output/diagnostics match
   the current deterministic behavior. Global operations report and obey an
   explicit resource policy.
5. CLI manifests contain a complete source hash and accurate profile and
   diagnostics, including failed runs. A progressive GUI never presents
   provisional values as verified and detects a changed source before using
   cached rows for execution.
6. A generated large synthetic CSV shows bounded peak retained source memory
   and measurable open/scroll timings recorded in the implementation review.
   Define performance thresholds from a baseline before claiming a speedup.
7. Existing run artifact schemas remain readable, or affected schemas are
   explicitly versioned with compatibility tests.

## Fixture and test strategy

Use only synthetic files: a regular CSV, a preamble with two stacked tables,
ragged and blank rows, quoted newlines, a distant scroll target, malformed and
oversize fields, and a generated large table. Unit tests cover batch sizes
(including 1, partial final batches, and invalid limits), cursor/index
coordinates, and revision invalidation. Core tests compare candidate/header
metadata and streamed rows with current expected behavior. Executor tests
compare streaming and existing materialized results and diagnostics for each
supported plan step. GUI tests count cache size and visible cell submissions;
manual synthetic-file runs inspect initial, distant, and backward scroll.
CLI integration tests compare structured artifacts, not just stdout. Do not
copy or commit the user's 3.2 MB file or its contents.

## Tasks

1. **Baseline and contracts.** Measure stage timings and peak retained memory
   on a generated CSV; characterize current selected-table, header,
   diagnostics, and CLI output behavior with focused tests. Define the
   selected-source metadata, batch result, resource limits, and revision
   semantics in the owning crates.
2. **Remove the GUI cell duplicate.** Adapt the existing grid to borrow or
   reference opened rows while preserving its current visible-range behavior.
   This gives an early, testable memory reduction without changing selection.
3. **Build the shared CSV batch reader.** Separate candidate/header discovery
   from row materialization. Implement sequential batches, bounded evidence,
   provenance, typed errors, and revision checks in `baho-ingest-csv`, exposed
   through the common ingest/core API. Consolidate the grounding scan onto
   this reader.
4. **Add GUI range loading.** Introduce a bounded visible-range cache and
   source-revision-bound sparse indexing/checkpoints for jumps. Remove
   all-row cell retention and per-frame all-row key construction. Verify
   scrolling, selection, and keyboard behavior with synthetic data.
5. **Stream CLI and core execution.** Feed batches into a format-independent
   executor path. Preserve typed parsing, grounding, deterministic results,
   diagnostics, and provenance. Implement explicit bounds or spill behavior
   for global operations and materialized outputs.
6. **Address initial open latency.** Combine avoidable full-file passes and
   measure again. Implement the reviewed GUI scan/hash scheduling and loading
   state while keeping CLI run records complete. Add source-change handling
   and structured progress/completion events.
7. **Verify integration.** Run focused crate tests and the workspace Rust
   baseline (`cargo fmt --check`, `cargo check --workspace`,
   `cargo test --workspace`). Compare synthetic CLI artifacts and GUI frame
   counts, timing, and peak retained memory with the baseline.

## Task 1 baseline (2026-09-29)

Generated locally: 110,000 data records with four short columns plus one
header, 3,517,799 bytes. The source is under `/tmp` and is not a repository
fixture. The current CLI `run` path retained 110,000 selected `OpenedRow`s
before execution. A single materializing run (`List names`, stdout discarded)
took 1.193 seconds and reached 100,040,704 bytes maximum resident memory on
macOS, measured with `resource.getrusage(RUSAGE_CHILDREN)`. Resident memory is
a process-level peak, including output and run recording, rather than a direct
measurement of retained source cells.

Core open-stage event elapsed times, measured from `open_table` entry in a
separate run of the same generated file, were: input profiled 194 ms,
candidates detected 198 ms, candidate selected 198 ms, header selected
198 ms, and body rows classified 330 ms. These are cumulative times. The
remaining CLI time includes planning, execution, output, and run artifacts.
The data supplies a baseline for later comparisons, not a speedup threshold;
repeat measurements under identical build and host conditions before making
performance claims.

## Task 7 verification (2026-09-29)

With the same 3,517,799-byte synthetic CSV and `List names` prompt, a current
debug CLI run completed in 2.168 seconds and reached 22,773,760 bytes peak
resident memory (`resource.getrusage(RUSAGE_CHILDREN)` on macOS, stdout
discarded). The run materialized 110,000 rows, recorded the complete SHA-256
source hash, produced the expected nine indexed artifacts, and recorded no
diagnostics. Its cumulative core open-stage times were 196 ms for profiling,
candidate selection and header selection, and 329 ms for body classification.
Compared with the Task 1 run, process peak resident memory fell from
100,040,704 bytes (about 77%); elapsed time rose from 1.193 to 2.168 seconds.
These are single runs, so they establish neither a stable speedup nor a
regression threshold. The core open-stage times are similar to baseline.

GUI synthetic tests confirmed 1,200 row identities with no opened source rows
retained, at most 42 cached rows for a 10-row distant viewport, and the same
bound after backward scrolling. A visible range of two rows by two columns
submitted four cells. No pre-change GUI frame-count, timing, or process-memory
baseline was recorded, and a real window frame trace was not collected;
therefore a measured GUI frame or latency comparison remains open.

`cargo fmt --check` and `cargo check --workspace` passed. `cargo test
--workspace` first exposed an obsolete run-recording test expectation for a
source changed after open. After updating that test to expect a refusal while
preserving the original hash, `RUSTC_WRAPPER= cargo test --workspace --quiet`
and `RUSTC_WRAPPER= cargo test -p baho-gui --quiet` passed. The wrapper was
disabled because `sccache` returned an operation-permitted error in this
environment. Compiler warnings came from the external local `llm-sdk` and one
unused executor test closure variable.

## Open questions for review

- Should GUI opening display the selected header as soon as bounded discovery
  finishes, with row count and full diagnostics marked provisional, or wait
  for a verified full scan? This choice controls the first visible response.
- What memory budget and temporary-storage policy should apply to distinct,
  sort, aggregates, and large materialized outputs? The limit must be
  configurable or explicitly documented and produce typed diagnostics.
- Should the sparse CSV index be kept only in the session or cached locally
  across opens? A persistent cache needs versioning and source verification.
- Should a simple-file mode that treats the first record as a header be a
  later explicit parser option? It is not required for this epic.
