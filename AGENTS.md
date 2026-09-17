# AGENTS.md — Guide for Coding Agents

This document defines how coding agents work in the baho repository. Read it in full at the start of every session, then read `PRD.md` before inspecting or changing code.

## What baho is

Baho is a Rust application for turning irregular CSV, spreadsheet, and PDF documents into provenance-aware grids and deterministic materialized views. Users may express intent in natural language, but language models only propose constrained plans. Baho never executes LLM-generated source code.

The current priority is the smallest useful CSV CLI and the diagnostic development loop described in `PRD.md`. Do not implement later GUI, daemon, Excel, PDF, OCR, or LLM milestones unless the task calls for them.

## The primary development loop

The usual task begins with a CLI run made by the user:

1. The user runs baho with a source file and states what they intended to extract or compute.
2. Baho records the invocation and internal decisions under `.baho/runs/<run-id>/`.
3. Read the user's request, the run manifest, structured events, parser configuration, candidate tables, diagnostics, and outputs.
4. Reproduce the behavior with the original input when it is accessible.
5. For non-trivial work, write an epic describing the evidence, design, alternatives, and acceptance criteria.
6. After the epic is reviewed, reduce the case to the smallest safe regression fixture.
7. Write a failing test that expresses the intended behavior.
8. Implement the smallest coherent change in the crate that owns the behavior.
9. Run focused tests, then workspace checks appropriate to the change.
10. Report what changed, which run or fixture motivated it, and how it was verified.

Treat logs as evidence, not as infallible conclusions. Reconcile them with the source, configuration, and code.

## Starting a task

Before editing:

1. Read `PRD.md` and this file.
2. Check `git status`; preserve all user changes and unrelated work.
3. Inspect the workspace manifests and the crate that owns the behavior.
4. If a run ID was supplied, inspect that exact run. Otherwise use the greatest run ID only when the user asks for the latest or most recent run.
5. Read the original input only when it is in scope and available. Do not infer its contents solely from log samples.
6. State the behavior you intend to capture in a test.

Do not broaden a parser fix into a workspace redesign. If the evidence contradicts the PRD or requires a new product decision, explain the conflict and ask the user.

## Run artifacts

Runtime artifacts live under `.baho/`, which must be ignored by Git. Never add `.baho/`, user documents, generated outputs, or raw run logs to a commit.

Each invocation owns exactly one `.baho/runs/<run-id>/` directory. Run IDs are monotonically increasing, zero-padded counts such as `000001`; compare them numerically. Never choose “latest” using modification times, and never reuse or overwrite an existing run directory. Gaps are valid.

Useful run files include:

- `manifest.json`: invocation, versions, input identity, outcome, and artifact index.
- `intent.txt`: what the user wanted from that invocation.
- `events.jsonl`: structured internal events in execution order.
- `input-profile.json`: encoding, dialect, dimensions, samples, and anomalies.
- `parser-config.json`: every option that influenced parsing.
- `candidates.json`: table candidates and the evidence behind their scores.
- `diagnostics.json`: stable diagnostics attached to source locations.
- `output/`: materialized results, if any.

When adding instrumentation, prefer a stable event name and structured fields over prose assembled into one string. Log decisions and their inputs, not every cell. Keep samples bounded. Never log environment dumps, secrets, credentials, or full document contents.

If a command fails before a normal run can be completed, preserve a run manifest and the error whenever possible. Observability of failures is part of the CLI contract.

## Epics before implementation

The user will commonly ask an agent to read a CLI run and create an epic before any code is changed. Treat analysis and implementation as separate phases.

Epics live in `epics/` and use the next zero-padded count plus a short slug, for example `epics/001-multiple-csv-tables.md`. Determine the next number from existing epic filenames; never renumber existing epics. A useful epic contains:

- `Status: Proposed`, plus the motivating run ID and user intent.
- Observed behavior supported by specific run artifacts.
- The problem and the desired behavior, including what is out of scope.
- Relevant architecture and crate ownership.
- A proposed design and meaningful alternatives or tradeoffs.
- Observable acceptance criteria.
- Fixture and test strategy that does not expose user data.
- An implementation outline and unresolved questions.

When asked to “read the logs and create an epic,” inspect the evidence and write only the epic. Do not implement it in the same task unless the user explicitly asks for implementation too. The pause is intentional: it gives the user an opportunity to brainstorm, revise, or reject the design. Once implementation is requested, reread the approved epic and update its status consistently as work progresses. Small fixes with an already explicit design do not require a new epic unless the user requests one.

## Fixtures and user data

Real user files are diagnostic inputs, not test fixtures by default.

- Do not copy a user document into `tests/`, commit it, or publish its contents without explicit approval.
- Prefer a minimal synthetic fixture that preserves the structural cause: delimiters, quoting, blank regions, header depth, raggedness, or table placement.
- If exact bytes are essential, ask whether a sanitized fixture may be committed.
- Avoid putting sensitive cell values in test names, snapshots, failure messages, or source comments.
- Record the relationship to a local run in the final report, not as an absolute local path embedded in production code.

Every behavior fix should normally include a regression test. Snapshot tests are appropriate for grids, plans, candidates, and diagnostics when volatile metadata is removed and ordering is stable.

## Crate ownership

Keep dependencies directed and responsibilities narrow:

| Crate | Owns | Must not own |
|---|---|---|
| `baho-model` | Values, coordinates, grids, provenance, schemas, diagnostics | File I/O, CLI, UI, provider clients |
| `baho-ingest` | Import traits, source detection, shared ingest contracts | Format-specific heuristics, UI |
| `baho-ingest-csv` | CSV dialect analysis, profiling, candidates, CSV extraction | Operation execution, CLI presentation |
| `baho-plan` | Versioned plan and expression IR, structural validation | LLM calls, arbitrary code execution |
| `baho-exec` | Typed validation and deterministic materialization | Parsing source formats, UI, providers |
| `baho-core` | Application façade and orchestration | CLI formatting, akar/winit rendering |
| `baho-cli` | Arguments, terminal output, run artifact lifecycle | Parser algorithms and domain rules |
| `baho-gui` | Akar frame loop and presentation | Domain logic duplicated from core |

If the workspace is still being bootstrapped, preserve these boundaries without creating placeholder crates that have no immediate use.

Core crates must remain usable by both the GUI and a future daemon. They must not depend on akar, wgpu, winit, a CLI framework, or a mandatory async runtime. Keep `anyhow` at application boundaries; libraries should expose typed errors with source chains.

## Parser rules

- Preserve raw input and provenance when producing interpreted values.
- Do not silently coerce malformed cells to null or drop malformed rows.
- Represent recoverable problems as diagnostics at the narrowest useful scope.
- Use stable diagnostic codes; prose may improve without breaking consumers.
- Separate physical inspection, candidate detection, candidate selection, header construction, schema parsing, and operations into observable stages.
- Make heuristics explicit and configurable. Record chosen values and relevant scoring evidence in run artifacts.
- Keep result ordering deterministic, including diagnostics and equally scored candidates.
- Bound sampling and memory use. Inspection of a large CSV should be stream-oriented.
- Treat intent text as context, not executable configuration. A structured, validated plan or parser configuration must explain actual behavior.

## Local dependency sources

The user keeps clones of open-source dependencies under `~/Projects/`. These local checkouts are the preferred source for understanding dependency APIs, behavior, implementation details, and examples.

Before relying on non-trivial behavior from a dependency:

1. Look for its checkout under `~/Projects/` using likely repository names.
2. Read its manifest, documentation, source, and tests locally.
3. Treat the checkout as reference material unless the baho manifest intentionally declares a path dependency.
4. Do not edit dependency checkouts as part of a baho task.

If a needed dependency is not cloned, tell the user which repository is missing and why its source is needed. Do not silently clone it, fetch an alternate copy, or substitute a different library merely because its source happens to be present. Registry documentation may help with orientation, but it does not replace notifying the user when local source inspection is required.

The akar source is expected at `~/Projects/akar`. Before GUI work, read akar's own `AGENTS.md`, current development documentation, and the relevant examples. Akar is synchronous and developer-loop driven; do not impose an async runtime or a second event loop on it.

## Testing and verification

Use the narrowest fast feedback loop first:

1. Run the focused test for the affected behavior.
2. Run the owning crate's tests.
3. Run workspace formatting, checks, and tests when the change warrants them.
4. Re-run the motivating CLI command when the source is available.
5. Compare structured artifacts, not only terminal presentation.

Before handing off Rust changes, the expected baseline is:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

Add `cargo clippy --workspace --all-targets -- -D warnings` once the workspace adopts a warning-clean policy. Do not claim a command passed unless it was run. If a check could not run, give the concrete reason.

Tests should assert behavior and diagnostics rather than private implementation details. Parser tests should include source coordinates and raw values where those are part of the contract. Fuzz or property testing is encouraged for dialect detection, quoting, record boundaries, and expression evaluation once those components exist.

## Code and change discipline

- Prefer clear domain types over loosely structured JSON inside core code. Persisted artifacts may use JSON through versioned serializable types.
- Use zero-based row and column indices internally; presentation may render spreadsheet-style coordinates.
- Avoid process-global mutable state. Multiple documents and runs must be safe in one process.
- Do not panic on input-controlled data.
- Avoid premature parallelism; deterministic diagnostics and bounded memory matter more initially.
- Comments should explain a non-obvious invariant or tradeoff, not restate code.
- Do not change persisted schemas without changing their explicit schema version and adding compatibility coverage or a documented migration decision.
- Do not add an LLM SDK to parsing or execution crates.
- Do not add arbitrary scripting, expression evaluation through a host language, or runtime code compilation.

## Completion report

At the end of a task, report:

- The user-visible behavior that changed.
- The owning crate and important files changed.
- The regression fixture or run evidence used, without exposing sensitive contents.
- The verification commands and their outcomes.
- Any remaining ambiguity, unsupported case, or missing local dependency checkout.

The task is complete when the behavior is represented by a test, the implementation satisfies it, relevant diagnostics remain understandable, and the available checks pass.
