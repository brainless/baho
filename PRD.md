# baho Product Requirements

## 1. Product summary

`baho` is a local-first workflow application for extracting structured data from CSV, spreadsheet, and PDF documents. The name means “to flow” in Hindi and Nepali.

Users describe the table or result they want. Baho imports the source without discarding useful evidence, identifies one or more table-shaped regions, applies an explicit schema, and runs deterministic operations to produce materialized views.

Language models may translate a user's request into a constrained plan. They must never generate or execute arbitrary code inside baho. Parsing, validation, transformation, and materialization are implemented and tested Rust code.

## 2. Development model

Baho is developed through a fixture-driven loop between the user, the CLI, and coding agents:

1. The user runs the CLI against a real document and supplies a plain-language statement of intent.
2. The CLI creates a self-contained diagnostic run under `.baho/runs/<run-id>/`.
3. A coding agent reads this PRD, `AGENTS.md`, the recent run record, and the relevant source document when it is available.
4. For non-trivial work, the agent writes a proposal under `epics/` so the problem, alternatives, and intended behavior can be reviewed before implementation.
5. After approval, the agent creates a safe regression fixture, writes tests, and changes the appropriate crate.
6. The user rebuilds and repeats the same CLI operation.

This loop is a product requirement. Diagnostics must make a failed or surprising run understandable without attaching a debugger or modifying the code to add temporary prints.

## 3. Initial milestone: CSV CLI

The first milestone is intentionally narrow. It includes:

- A Rust workspace with a reusable core library and a small CLI.
- CSV and delimiter-separated input only.
- Internal inspection of file encoding, dialect, record widths, blank regions, and candidate table boundaries.
- Extraction of a requested table into the common grid model as part of fulfilling one user request.
- Support for common irregularities such as preambles, footer notes, multiple tables, blank separators, repeated headers, multi-row headers, ragged rows, and summary tables.
- Cell-, row-, table-, and document-level diagnostics.
- Machine-readable and human-readable run artifacts.
- Deterministic output suitable for snapshot and regression testing.

The initial milestone does not include Excel, PDF, OCR, a GUI, a daemon, or an online LLM call. Its data model and crate boundaries must leave room for them.

## 4. Initial CLI contract

The user-facing shape is:

```text
baho run <INPUT> --prompt <TEXT> [--output <PATH>]
baho runs [--latest]
```

`run` is the single document operation exposed to users. Inspection, candidate detection, schema construction, extraction, and materialization are internal stages of that operation. They remain distinct and observable in run artifacts, but users do not need to select a processing stage before describing the result they want.

`runs` locates previous diagnostic runs and does not create a run of its own. This prevents `runs --latest` from making itself the latest run.

In the initial bootstrap, `--prompt` is recorded verbatim as development context in `intent.txt`. Successfully recording a request does not imply that the document was processed; the manifest and diagnostics state when processing is unavailable. Baho must not pretend that it understood unconstrained language when no planner is configured. Any future decision that affects extraction must be represented in structured parser configuration and recorded in the run artifacts.

CLI stdout is for the requested result or a concise summary. Progress and diagnostics go to stderr and the run directory. A successful invocation prints the run ID and run path to stderr.

## 5. Run records and observability

Every `baho run` invocation that parses successfully creates its own `.baho/runs/<run-id>/`. Read-only administrative commands such as `baho runs` do not. `.baho/` is local state and must be excluded from Git.

The run ID is a monotonically increasing count, rendered with leading zeroes for lexical ordering: `000001`, `000002`, and so on. The next invocation atomically reserves the next available number so concurrent invocations cannot share or overwrite a directory. Gaps are valid after interrupted or failed runs, and an existing run directory is never reused. “Latest run” means the greatest allocated run ID, not the directory with the newest modification time.

The bootstrap run layout is:

```text
.baho/runs/<run-id>/
├── manifest.json
├── intent.txt
├── events.jsonl
└── diagnostics.json
```

As processing stages are implemented, they add `input-profile.json`, `parser-config.json`, `candidates.json`, and `output/` when output is materialized. The manifest artifact index identifies exactly which artifacts a run produced; absent stages do not produce empty placeholder artifacts.

`manifest.json` records:

- Run schema version and run ID.
- Start/end timestamps, duration, and outcome.
- Exact baho command, subcommand, arguments, and relevant configuration.
- Baho version and Git revision when available.
- Working directory and platform information useful for reproduction.
- Input path, size, modification time, and content hash.
- Artifact paths produced by the run.

`events.jsonl` is structured tracing with timestamps, levels, targets, processing stages, and stable event names. Parser logs should explain format detection, sampled row widths, candidate-region scoring, header decisions, schema decisions, skipped records, warnings, and errors. Logs must be useful at the default verbosity; verbose mode may add bounded samples and timings.

Run files must be deterministic where practical. Volatile fields belong in the manifest, not in result snapshots. Logs must never contain environment dumps, credentials, or entire source documents. Cell samples should be bounded and clearly marked. The source file is referenced and hashed, not copied into `.baho/`, unless the user explicitly requests a copy.

## 6. Epic workflow

Substantial behavior changes are designed in `epics/` before implementation. A run-driven epic connects observed behavior to a reviewed design instead of moving directly from logs to code.

Epic files use a count-based name such as `epics/001-multiple-csv-tables.md`. An epic records its status, motivating run ID, user intent, observations, problem statement, constraints, proposed design, alternatives considered, acceptance criteria, test strategy, implementation outline, and open questions.

When the user asks an agent to inspect logs and create an epic, that is a brainstorming task. The deliverable is the epic document; implementation waits for user review unless the user explicitly requests both design and implementation in the same task. Small, already-specified maintenance changes may proceed without a new epic.

## 7. Core concepts

The canonical model preserves physical source evidence separately from interpreted values:

- `Document`: an imported source and its metadata.
- `Sheet`: a named or indexed two-dimensional source surface.
- `CellAddress`: sheet, zero-based row, and zero-based column.
- `Cell`: raw representation, optional interpreted value, and provenance.
- `GridRegion`: a rectangular or sparse table candidate within a sheet.
- `ColumnDefinition`: name, source selector, parser chain, target type, null rules, and constraints.
- `Schema`: ordered columns plus header and row-selection rules.
- `Diagnostic`: stable code, severity, processing stage, scope, location, and message.
- `MaterializedView`: the immutable result of applying a validated operation plan to a source revision.

Imported data must not be silently coerced. A failed conversion retains the raw cell and emits a diagnostic. Original row and column coordinates remain available after filtering, parsing, and materialization.

## 8. Planned workspace boundaries

```text
crates/
├── baho-model/          # Shared values, grids, provenance, schemas, diagnostics
├── baho-ingest/         # Import interfaces and format detection
├── baho-ingest-csv/     # CSV dialect analysis and grid extraction
├── baho-plan/           # Serializable, versioned operation-plan IR
├── baho-exec/           # Deterministic validation and execution
├── baho-llm/            # Optional LLM provider configuration and transport adapters
└── baho-core/           # Stable façade for CLI, GUI, and future daemon
apps/
├── baho-cli/
└── baho-gui/            # Added after the core workflow is established
```

The boundaries are directional: applications depend on `baho-core`; `baho-core` coordinates lower-level crates; model and execution crates do not depend on a UI, windowing library, async runtime, CLI framework, or LLM provider. Optional provider dependencies and their async transport remain isolated in `baho-llm`; prompts and agent policy do not belong there.

The GUI will use akar and its developer-driven synchronous frame loop. A future daemon must be able to use the same `baho-core` API without linking akar, wgpu, or winit.

## 9. Deterministic operation plans

User operations are represented by a versioned, serializable plan containing a restricted expression tree and known operations such as select, filter, sort, derive, rename, split, aggregate, join, and union.

Before execution, a plan is validated for:

- Supported plan version and operation names.
- Existing inputs and columns.
- Compatible input and output types.
- Valid function arguments.
- Resource and output-size limits where appropriate.

The same source revision, parser configuration, and operation plan must produce the same logical result. LLM provider details and conversational state must not be required by the executor.

## 10. Later milestones

- Excel/ODS ingestion through a dedicated adapter while retaining sheet and cell provenance.
- PDF ingestion into positioned page content before table inference.
- OCR as an interchangeable provider of positioned text, sharing the PDF table-inference path.
- Akar GUI with a virtualized source grid, parser/schema editor, diagnostics, operation list, and materialized-view tabs.
- Optional agent-assisted intent-to-plan generation with explicit plan review and validation.
- A daemon wrapping `baho-core` for unattended or server-side execution.

## 11. Quality requirements

- No panics for malformed or adversarial input in normal CLI paths.
- Errors identify the processing stage and retain their source chain.
- Parsing behavior is covered by small, focused fixtures and regression tests.
- Tests cover empty files, unusual encodings, quoted newlines, ragged records, large fields, multiple tables, multi-row headers, and malformed input.
- Large inputs are streamed or bounded; inspection must not require loading an entire CSV merely to profile it.
- Public data structures and persisted run artifacts carry explicit schema versions.
- Outputs and diagnostics use stable ordering.
- User documents and `.baho/` artifacts are never committed unless the user explicitly approves a sanitized fixture.

## 12. Initial success criteria

The CSV milestone is successful when the user can run `baho run` on an unfamiliar CSV, give a coding agent the resulting run ID, and the agent can understand the request and parser decisions well enough to add a failing regression test and implement the improvement in the correct crate. Re-running the original command must make the behavioral change and its diagnostics evident.
