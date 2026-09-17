# Epic 001: Deterministic CSV Grid, Table Detection, and Unique-Value Planning

Status: Implemented

## Implementation notes

All 9 implementation outline steps completed. The deterministic CSV processing pipeline is functional:

- **baho-model** (29 tests): Source revisions, sheets, cells, coordinates, grid regions, column definitions, values, provenance, diagnostics
- **baho-ingest** (12 tests): Format-independent import contracts, InputProfile, ImportRegistry
- **baho-ingest-csv** (36 tests): CSV dialect inspection, row features, candidate detection, header normalization, body/footer classification, CsvImporter
- **baho-plan** (15 tests): Versioned plan IR (filter/select/distinct), structural validation, recognition evidence
- **baho-exec** (17 tests): Typed validation, deterministic filter/select/distinct materialization with provenance
- **baho-core** (16 tests): Orchestration pipeline, narrow deterministic intent recognizer, candidate selection
- **baho-cli** (7 integration tests): Full run lifecycle with pipeline integration, stage artifact serialization
- **Synthetic fixture**: `tests/fixtures/report_with_preamble.csv` with preamble, embedded-newline header, interleaved blanks, repeated values, footer

Workspace baseline: `cargo fmt --check`, `cargo check --workspace`, `cargo test --workspace` (145 tests) all pass.

Motivating run: `000001`

User intent: `Extract all the unique floor plans`

## Summary

Build the first complete non-LLM processing path for baho. Given a CSV-like file and a narrowly recognizable request for the unique nonblank values of a named column, baho will:

1. inspect and parse the physical CSV without discarding source evidence;
2. represent it as a provenance-aware two-dimensional grid;
3. detect candidate table boundaries after an arbitrary-length preamble;
4. identify and normalize a single-row header;
5. distinguish compatible table rows from trailing notes and other footer material;
6. resolve an unambiguous column mention from the prompt;
7. construct and validate a versioned `filter`/`select`/`distinct` plan;
8. execute the plan deterministically; and
9. record the evidence, decisions, plan, diagnostics, and result in the run directory.

This epic intentionally develops the deterministic layer through fixtures and failures. It does not call an LLM or treat free-form text as executable configuration.

## Motivating evidence

Run `000001` successfully recorded the source identity and prompt, then emitted `processing.not_implemented`. Its artifact index contains only `manifest.json`, `intent.txt`, `events.jsonl`, and `diagnostics.json`; there is no parser profile, candidate table, parser configuration, plan, or output to explain how the requested result would be produced.

A bounded structural inspection of the original source found:

- 1,699 logical CSV records;
- a stable width of 15 physical columns for every logical record;
- a report preamble before the table;
- a single-row header at zero-based source row 8;
- 12 nonblank header cells, including a header physically encoded as `Floor\nPlan`;
- the first nonblank row after the header at source row 12;
- the last nonblank row at source row 1,697;
- 1,127 completely blank logical rows, including blank rows interleaved throughout the table body; and
- repeated nonblank values in the floor-plan column.

These observations are evidence about this source, not general parsing rules. In particular, the high number of internal blank rows means that a table cannot end at the first blank record or at a fixed-size blank gap.

The original document remains diagnostic input only. It must not be copied into the repository or embedded in snapshots.

## Problem

Baho currently records a request but cannot explain or perform any document processing. The first implementation needs to establish durable boundaries between physical ingestion, table interpretation, planning, and execution while being useful for the motivating request.

Several distinct questions must not be collapsed into one heuristic:

- How was the byte stream decoded and split into logical records and fields?
- What cells physically exist, including empty cells and records?
- Which rectangular region is a table candidate?
- Which source row or rows supply column headers?
- Which rows are table data, internal separators, preamble, or footer notes?
- Which column did the prompt name?
- What exact operations and blank-value semantics implement the request?

If any of these decisions is uncertain, baho must expose the competing evidence or emit a diagnostic. It must not silently guess and then present the result as understood.

## Desired behavior

For a source shaped like the motivating report, `baho run` will select the high-confidence table candidate, normalize the embedded header whitespace to the display name `Floor Plan`, resolve the plural phrase `floor plans` to that one column, and produce a plan equivalent to:

```json
{
  "schema_version": 1,
  "source": {
    "revision": "<input sha256>",
    "table_id": "table-0"
  },
  "steps": [
    {
      "op": "filter",
      "predicate": {
        "op": "is_not_blank",
        "column": "column-1"
      }
    },
    {
      "op": "select",
      "columns": ["column-1"]
    },
    {
      "op": "distinct",
      "columns": ["column-1"],
      "keep": "first"
    }
  ]
}
```

The persisted representation should use the final plan types rather than relying on this illustrative JSON verbatim.

`distinct` is the primitive table operation. `unique values` is intent vocabulary compiled into a composition of existing operations, not an expression that returns an untyped array. Blank exclusion is represented by an explicit filter. Distinct values retain first-occurrence source order unless the plan contains an explicit sort.

## Scope

### In scope

- UTF-8 CSV and delimiter-separated input supported by the initial CLI milestone.
- Bounded dialect and record-width inspection.
- A physical grid model with raw cell text and source coordinates.
- Candidate detection for one or more table-shaped regions.
- Deterministic selection when one candidate is clearly strongest.
- A single-row header after a variable-length preamble.
- Header cells containing embedded newlines or surrounding whitespace.
- Sparse headers in which some physical columns are unnamed.
- Internal blank rows and trailing blank rows.
- Footer/note rows that are structurally incompatible with the table body.
- A narrow deterministic intent recognizer for unique/distinct nonblank values from one unambiguously named column.
- A versioned plan IR containing the operations required by this use case.
- Structural and typed validation before execution.
- Stable-order materialization with provenance.
- Structured run artifacts, events, and diagnostics for every stage.
- Failure artifacts when parsing, detection, intent resolution, validation, or execution cannot complete.

### Out of scope

- LLM calls, prompts, provider selection, or plan review by an LLM.
- Excel, PDF, OCR, GUI, or daemon work.
- Arbitrary natural-language understanding.
- Arbitrary code, scripts, SQL strings, or host-language expression evaluation.
- Multi-row or merged-cell header interpretation in this first slice.
- Repeated page headers inside a table.
- Joining or unioning multiple candidates.
- Inferring or filling down visually merged body values.
- Locale-aware or domain-specific value normalization.
- Case-insensitive or whitespace-insensitive value deduplication unless represented by a future explicit normalization operation.

The data model and stage boundaries must permit these cases to be added later without changing the meaning of version 1 plans.

## Architecture and ownership

This work establishes only crates with immediate responsibilities:

| Crate | Responsibility in this epic |
|---|---|
| `baho-model` | Source revisions, sheets, cells, coordinates, grid regions, column definitions, values, provenance, and diagnostics. |
| `baho-ingest` | Format-independent inspection/import contracts and imported-document results. |
| `baho-ingest-csv` | CSV dialect inspection, logical record parsing, row features, table/header candidates, row classification, and CSV-specific diagnostics. |
| `baho-plan` | Versioned serializable plan and expression IR plus structural validation. |
| `baho-exec` | Schema-aware validation and deterministic execution/materialization. |
| `baho-core` | Orchestration of ingestion, deterministic intent recognition, candidate selection, planning, validation, execution, and artifact-ready results. |
| `baho-cli` | Arguments, terminal rendering, run lifecycle, and serialization of run artifacts. |

`baho-llm` remains unchanged and is not a dependency of parsing, planning, execution, or core orchestration.

Library crates expose typed errors with source chains. `anyhow` remains at the CLI/application boundary. Core crates must not depend on clap, a UI framework, a mandatory async runtime, or provider clients.

## Canonical physical model

The model must distinguish source evidence from interpretation. At minimum it needs concepts equivalent to:

- `SourceRevision`: stable input identity including the content hash.
- `Document`: imported source metadata and ordered sheets.
- `Sheet`: a named or indexed two-dimensional source surface.
- `CellAddress`: sheet, zero-based row, and zero-based column.
- `Cell`: raw text, optional interpreted value, and provenance.
- `GridRegion`: source bounds for a rectangular candidate.
- `ColumnDefinition`: stable ID, ordinal, source header cells, display name, and optional interpreted type.
- `TableCandidate`: bounds, header decision, body-row classification, score, and structured evidence.
- `MaterializedView`: immutable columns and rows plus source provenance.

CSV inspection should remain streaming and bounded. It may collect bounded samples and row features without retaining the entire input. Once a candidate is selected, the implementation may materialize the required region as a grid for this milestone. This avoids making unbounded whole-file loading part of the import contract.

Every physical row and cell inside a materialized region retains its original address. Semantic row selection does not renumber away provenance.

Unnamed physical columns remain addressable by stable generated IDs and receive a diagnostic or generated display name. Header text must never be filled across adjacent blank columns without explicit evidence.

## Deterministic processing stages

### 1. Physical inspection

Inspect a bounded prefix and, where needed, stream through the file to record:

- encoding decision;
- delimiter and quoting decision;
- logical record count when a complete pass is made;
- sampled and aggregate record-width evidence;
- blank-record locations or bounded summaries;
- malformed record evidence; and
- limits reached during inspection.

Parsing must use CSV logical records, not physical newline splitting, because quoted fields may contain newlines.

### 2. Row features

Compute deterministic row features that later stages can inspect without reading prose logs. Useful features include:

- physical width;
- nonblank cell count and density;
- text/numeric/blank shape by column;
- normalized cell tokens for bounded header analysis;
- similarity to nearby row shapes;
- repeated-value and uniqueness evidence appropriate for header scoring; and
- whether the row is wholly blank.

Features and samples must be bounded in persisted artifacts even when aggregate counts cover the complete input.

### 3. Candidate table detection

Generate candidates instead of committing immediately to one interpretation. A candidate includes a proposed header, physical column span, body span, semantic data rows, score components, and rejection/selection evidence.

Candidate scoring should consider, at minimum:

- a header-like row followed by multiple body-compatible rows;
- stable physical width or stable occupied column span;
- column-wise shape consistency across body rows;
- header density and textuality relative to following rows;
- distance between a header and the first compatible data row;
- internal blank rows that may be separators rather than terminators; and
- sustained incompatible rows that suggest a footer or a new region.

Scores, thresholds, tie-breaking rules, and lookahead limits belong in a versioned parser configuration. Equal scores use source order for deterministic ordering, but a tie or insufficient score must not silently select a candidate.

### 4. Header construction

For this epic, a header is one physical row. Header normalization for matching and display will:

- preserve the original raw text and coordinate;
- trim leading and trailing whitespace;
- collapse internal whitespace, including embedded newlines, to one space; and
- use a documented case-folded token form only for matching.

Duplicate normalized labels and unnamed columns remain distinct through stable IDs derived from source ordinals. They produce diagnostics and require disambiguation during intent resolution.

### 5. Body and footer classification

A blank row is not sufficient evidence for either the start or end of a table. The classifier examines subsequent rows within a configured lookahead and scores compatibility against the candidate's width and per-column body shape.

The selected candidate records separate concepts:

- physical body bounds, which can include blank separator rows;
- semantic data rows used by operations;
- ignored internal blank rows;
- trailing blank rows; and
- rejected footer/note rows with reasons.

A footer note is excluded only by explicit structural evidence, such as a sustained change in occupied span, density, column-shape compatibility, or a new competing header. Text content such as `note` or `total` may contribute bounded evidence but must not be the sole rule. If a footer row remains plausibly data, baho emits an ambiguity diagnostic instead of silently discarding it.

### 6. Deterministic intent recognition

The first recognizer deliberately accepts only a narrow grammar equivalent to:

```text
<extract/list/show> [all/the] <unique/distinct> <column mention>
```

Matching is token-based and deterministic. Operation vocabulary maps `unique` and `distinct` to the same plan template. Column matching compares prompt tokens with normalized header tokens and may account for a simple terminal singular/plural difference, so `floor plans` can match `Floor Plan`.

The recognizer records candidate column scores and evidence. It creates a plan only when:

- exactly one selected table is available;
- the operation is supported;
- one column exceeds a documented match threshold; and
- it is separated from the next candidate by a documented ambiguity margin.

Otherwise it emits a stable unsupported- or ambiguous-intent diagnostic and does not materialize a result. Intent text itself never changes parser configuration.

### 7. Planning and validation

`baho-plan` exposes a closed, versioned, serde-tagged enum rather than arbitrary JSON operations. The first version need only include the coherent primitives required here:

- `filter` with `is_not_blank`;
- `select`; and
- `distinct` with explicit first-occurrence retention.

Structural validation rejects unknown versions, empty selections, invalid step shapes, and invalid references between steps. `baho-exec` then validates table and column existence and compatible value types before evaluating anything.

Plan steps reference stable table and column IDs. The plan also binds to the source revision so it cannot accidentally execute against a different document.

### 8. Execution

Execution is deterministic and stable:

- `is_not_blank` excludes absent values and strings that are empty after the parser's documented blank check; it does not normalize nonblank values;
- `select` preserves requested column order;
- `distinct keep:first` retains the first surviving row for each exact typed value tuple; and
- output order is first occurrence unless an explicit future `sort` step changes it.

Materialized cells retain the source addresses of the rows that supplied the retained values. Later duplicates need not all be copied into the result, but diagnostic/debug structures may record bounded duplicate counts.

## Run artifacts

A successful processed run adds these versioned artifacts and indexes them in `manifest.json`:

- `input-profile.json`: encoding, dialect, dimensions, bounded samples, width/blank summaries, and anomalies.
- `parser-config.json`: every threshold, limit, normalization rule, and tie-breaker that influenced parsing.
- `candidates.json`: ordered table candidates, score components, selected candidate, header evidence, row classifications, and bounded rejection evidence.
- `plan.json`: the recognized, validated operation plan and recognition evidence.
- `output/`: deterministic materialized output plus machine-readable provenance metadata.

`diagnostics.json` remains the stable collection of user-actionable problems. `events.jsonl` records stage transitions and decisions, while large arrays and detailed evidence belong in the dedicated artifacts.

The manifest outcome must distinguish at least a materialized result from a recorded-but-unfulfilled or failed request. Persisted schema changes require explicit schema-version changes.

Suggested stable event names include:

- `input_inspection_started`
- `input_profiled`
- `table_candidates_detected`
- `table_candidate_selected`
- `header_selected`
- `body_rows_classified`
- `intent_recognized`
- `plan_validated`
- `materialization_completed`
- `processing_failed`

Events should record compact fields such as counts, IDs, scores, source bounds, configuration versions, and elapsed times. They must not record full rows, the entire document, environment dumps, credentials, or unbounded values.

Initial diagnostic codes should be stable and stage-specific, including cases equivalent to:

- `csv.unsupported_encoding`
- `csv.dialect_ambiguous`
- `csv.malformed_record`
- `table.not_found`
- `table.ambiguous`
- `header.not_found`
- `header.duplicate_name`
- `header.unnamed_column`
- `footer.ambiguous`
- `intent.unsupported`
- `intent.column_not_found`
- `intent.column_ambiguous`
- `plan.invalid`
- `execution.limit_exceeded`

Exact names may be refined during implementation, but tests must assert the chosen codes rather than prose messages.

## Fixture and test strategy

Create a small synthetic CSV fixture that preserves only the structural causes observed in run `000001`:

- a multi-row report preamble;
- a 15-column stable width;
- a single sparse header row;
- an embedded newline in the `Floor Plan` header;
- a gap between header and first data row;
- repeated floor-plan values;
- multiple blank separator rows inside the body;
- trailing blank rows;
- a structurally incompatible footer note; and
- no real names, addresses, unit identifiers, employers, or other source values.

Use neutral values such as `Type A`, `Type B`, and `Type C`. Expected output is the nonblank values in their first-occurrence order.

Tests should be layered:

1. `baho-model` tests preserve raw values, zero-based coordinates, and provenance.
2. `baho-ingest-csv` tests logical CSV records with quoted newlines, stable widths, header scoring, internal blank rows, and footer rejection.
3. Candidate snapshots remove volatile metadata and retain stable score evidence and ordering.
4. `baho-plan` round-trip and invalid-plan tests cover schema versioning and references.
5. `baho-exec` tests filter/select/distinct semantics, exact-value equality, first-occurrence ordering, and retained provenance.
6. `baho-core` tests successful recognition and each refusal path: unsupported phrasing, missing column, duplicate header, ambiguous column, and ambiguous table.
7. `baho-cli` integration tests assert the complete artifact index, stable diagnostics, event ordering, materialized output, and finalized failure manifests.

The original source may be used for a local reproduction after tests pass, but its bytes and sensitive values must not enter fixtures, snapshots, test names, source comments, or committed artifacts.

## Acceptance criteria

1. A synthetic CSV with an arbitrary-length preamble and an embedded-newline header is imported as a provenance-aware grid using logical CSV records.
2. The detector chooses the intended table and single header using persisted, inspectable score evidence rather than a hard-coded row number.
3. Internal blank rows do not terminate the table, while a structurally incompatible footer note is excluded with recorded evidence.
4. The raw header remains available and its display form is normalized to `Floor Plan`.
5. The prompt `Extract all the unique floor plans` resolves deterministically to one column and produces a versioned filter/select/distinct plan without an LLM.
6. Blank cells are excluded by an explicit plan step; no malformed or incompatible value is silently converted to null.
7. Distinct values retain exact value semantics, stable first-occurrence ordering, and source provenance.
8. Unsupported or ambiguous table, footer, header, column, or prompt cases produce stable diagnostics and no misleading materialized result.
9. Run artifacts contain the input profile, complete parser configuration, candidate evidence, validated plan, output/provenance, diagnostics, and structured stage events, all listed in the manifest.
10. Inspection sampling and persisted evidence are bounded; the implementation does not require reading an entire large file into memory merely to profile it.
11. `.baho/`, the motivating source, and generated outputs remain untracked.
12. Focused tests and the workspace baseline pass:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

## Implementation outline

1. Add the model crate and its versioned grid, provenance, column, table-candidate, materialized-view, and diagnostic types.
2. Add format-independent ingest contracts.
3. Add CSV streaming inspection, dialect configuration, row features, candidate/header detection, and row/footer classification with focused fixtures.
4. Add the versioned plan IR and structural validation.
5. Add typed execution validation and filter/select/distinct materialization.
6. Add core orchestration and the narrow deterministic intent recognizer.
7. Refactor the CLI run lifecycle to call core and serialize stage artifacts while preserving finalized failure runs.
8. Add end-to-end fixture coverage and compare structured artifacts rather than only terminal text.
9. Re-run the motivating command locally and compare the new profile, candidate, plan, diagnostics, and output with the source evidence.

Implementation should proceed in vertical, compiling increments. Crates should not be created as empty placeholders before their first types and tests are needed.

## Alternatives considered

### Treat the first nonblank row as the header

Rejected. The motivating source begins with nonblank report metadata, and similar exports commonly contain titles, timestamps, parameters, and page labels before the table.

### End a table at the first blank row or fixed blank gap

Rejected. The motivating source contains many blank records interleaved through valid table data. Blank gaps are evidence, not boundaries by themselves.

### Deduplicate the physical column directly

Rejected. It would include headers or blanks unless interpretation is hidden in implementation, would not explain table boundaries, and would not establish reusable planning and execution layers.

### Implement `unique()` as a scalar/list expression

Rejected for the plan IR. A relational `distinct` operation composes with filter and select, preserves a table-shaped result, and makes null and ordering behavior explicit. `unique` remains accepted intent vocabulary.

### Let an LLM choose the table, column, and operation now

Deferred. It would obscure weaknesses in the deterministic layer and make fixture-driven improvement harder. A later LLM planner can consume the same table catalog and plan schema, but its output must pass the same validators and executor.

### Use one opaque parser confidence score

Rejected. Candidate selection must persist named score components and the observations behind them so future runs can explain and refine failures.

## Open questions for review

1. Should a successful materialization set the manifest outcome to `materialized`, `completed`, or another stable term? **Resolved: `materialized`.**
2. Should CLI stdout default to CSV output for a one-column result, or print only a concise summary unless `--output` is supplied? **Resolved: prints one value per line for single-column results.**
3. What default candidate-score threshold and ambiguity margin should ship initially? These should be established from synthetic fixtures, not tuned solely to run `000001`. **Resolved: min_score_threshold=0.3, ambiguity_margin=0.1.**
4. Should wholly blank internal rows be represented in the candidate's semantic row-classification list, or summarized as ranges while their physical cells remain available from the grid? **Resolved: classified as `BlankSeparator` in the semantic row-classification list.**
5. For version 1, should duplicate and unnamed headers always block intent resolution only when they affect the requested column, or block selection of the entire table? **Resolved: block at intent resolution when they affect the requested column.**

