# Epic 011: Progressive CLI and GUI Results

Status: Implemented for current row-producing plans

Motivating run: None supplied. The user reported that a large-file CLI filter
appears to wait for the whole file before returning rows.

User intent: Return matching rows as batches are processed in the CLI and show
them progressively in the GUI. This is a presentation change, not a new filter
language or an API stream registry.

## Evidence and problem

Epic 009 introduced a selected-source batch reader. The core pipeline feeds
its rows to `baho-exec::execute_plan_stream`, which evaluates a filter for each
row but retains all accepted rows before returning a `MaterializedView`. The
CLI previously printed that final view after run recording. The GUI previously
ran prompt execution on the frame loop and switched views only when execution
finished. Full-file inspection, hashing, grounding, and schema work may still
precede execution; this epic does not claim to remove those costs.

## Design

- The executor invokes a borrowed-row callback after a row passes all plan
  steps and resource checks. The existing final-result API remains available
  as a wrapper with an empty callback. Output order and final artifacts remain
  deterministic.
- Core exposes callback variants of the run, prompt, and clarification paths.
  Core forwards retained rows without owning stdout or GUI state.
- CLI prints rows from the callback. It flushes the first row, every 64 rows,
  and at completion, so piped stdout can receive data before the scan ends.
  A later execution error may leave partial stdout; stderr and the run outcome
  identify the failure. The final run record is still written after execution.
- GUI executes a prompt on a worker thread, sends retained rows through a
  bounded channel, and appends them to a provisional result grid on the frame
  loop. It shows a processing status until the final result and run record are
  ready. The worker holds no GUI renderer state. Existing clarification and
  failure outcomes remain distinct.

## Boundaries and alternatives

This slice uses the existing row-producing filter/select/distinct plans.
Aggregates and derived columns are not implemented. The worker queue is bounded
so a faster producer cannot accumulate unlimited pending GUI messages. The
final materialized output may still grow to the executor's configured limit.
CLI and GUI do not share an API stream ID; that future design remains in Epic
010. Progressive opening, hashing, and grounding are separate future work.

An alternative was to emit one partial `MaterializedView` per batch. That
would repeatedly copy prior rows and blur the distinction between provisional
and final views. Per-row callbacks preserve the existing final view contract.

## Acceptance and verification

- A stream callback observes accepted rows in source order and sees the same
  values, provenance, and columns as the final view.
- CLI output is delivered during execution and is not duplicated at finish.
- GUI can display a received row while its worker remains active, then show the
  final recorded view after completion.
- Existing materialized results, diagnostics, and artifact tests continue to
  pass. Synthetic tests cover callback order and GUI partial/final states.

The implementation review should report the workspace Rust checks and any
known limitation in time to first result. No user document becomes a fixture.

## Follow-up timing on a 3.1 MB CSV (2026-10-02)

The user supplied a local 30,000-row CSV and the bare-value prompt `biology`.
Run 000070 was profiled with temporary elapsed-time probes, then the probes
were removed. Opening and classifying the table took about 316 ms. Before the
first retained row, grounding consumed about 1.17 s, deferred-literal
preparation another 0.36 s, and compared-column parsing another 0.37 s. The
bare-value grounding scan examines all selected rows and columns to find a
unique source-bound interpretation. Typed comparison evidence makes a further
complete selected-table pass. Execution then rereads rows and can emit the
first match; progressive output cannot bypass those prerequisites.

The deferred-literal preparation pass was unnecessary for this text predicate:
it loaded every selected row even though the predicate contained no deferred
numeric literal. Core now skips that pass when none is present. A synthetic
regression test removes the source after opening and verifies that resolving
a text predicate does not try to reread it. In a warm `cargo run`, the first
stdout line was observed at 2.50 s after the change; the direct binary took
2.07 s. These are single measurements, not a performance guarantee. Run
000075 retained the same 30,000 processed rows and 1,735 output rows as the
profiled pre-change run. Further latency work should measure and review the
grounding and typed-evidence passes without weakening their semantics.
