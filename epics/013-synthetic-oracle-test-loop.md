# Epic 013: Synthetic Oracle Test Loop

Status: Proposed

Motivating run: None. This is a development-process epic. It follows a design discussion about expanding the natural-language grammar of the deterministic planner in small, test-driven steps.

User intent: Grow the coverage of Baho's parser/planner (`baho-core::intent`, grounding, `baho-plan`, `baho-exec`) through controlled steps of simple natural-language/SQL pairs whose expected rows come from DuckDB. Use narrowly scoped agents for proposing pairs and plain code for the mechanical steps. The first exploratory batch uses a local CSV supplied by the user; accepted regressions use small synthetic fixtures.

This epic gives high-level guidance only. Implementation detail lives in per-stage epics: Epic 014 covers the first stage, and later stages are planned after Epic 014's results are reviewed.

## Observed behavior and problem

Epics 007 and 012 extended the planner with a hand-written test per rule (`crates/baho-core/tests/epic007_grounding.rs`, `epic012_header_first.rs`). Each test pairs a tiny synthetic CSV with a prompt and asserts a plan shape and the retained source rows. This works, but:

- Coverage grows only as fast as a person can think of prompts and compute their expected rows.
- Expected rows are computed by hand or by reading the code under test, so a misunderstanding in the planner can be reproduced in the test.
- The phrasing space of real users (synonyms of structure, word order, connectors, quoting) is much wider than any hand-written list.

Writing SQL for an intended result is generally easier and more reliable for a language model than producing the result rows directly. DuckDB can execute that SQL against the CSV and return the exact rows. This separates "what does the user want" (prompt plus SQL) from "what is the answer" (DuckDB), and both are independent of the planner under test.

## Desired behavior

A repeatable pipeline that, for one grammar feature at a time:

1. Produces candidate cases of the form *(dataset, natural-language prompt, SQL)*, with expected rows computed by DuckDB from the SQL. Epic 014 starts with one text equality predicate on the supplied payroll CSV's `Role` column.
2. Reduces reviewed behavior to small synthetic datasets and turns accepted cases into Rust tests with no model involvement. Local source data and its generated cases are not promoted verbatim.
3. Lets a coding agent (or a person) change the planner until those tests pass, without being able to alter them.

Success is measured by planner coverage growing in reviewable increments, not by the number of generated cases.

## Pipeline stages

| Stage | Responsibility | Nature | Planned in |
| --- | --- | --- | --- |
| 1. Case generation | Propose prompt and SQL pairs for a dataset and a feature description; verify SQL runs; record DuckDB's rows | Agent plus deterministic harness | Epic 014 |
| 2. Case validation | Reject ambiguous, trivial, or incorrect pairs (for example consensus between independently generated SQL, selectivity limits, deduplication) | Mostly code, possibly a second model call | After Epic 014 results |
| 3. Test emission | Read accepted case files and run them against `run_pipeline`; report per-case failures | Code only. No agent | After Epic 014 results |
| 4. Planner implementation | Change the planner so the failing tests pass | Coding agent with restricted write access | Later |
| 5. Orchestration | Sequence the stages, enforce gates, stop for human review | Plain code loop. No orchestrating agent | Later |

Stages 1 and 2 may be adjusted after the first generated corpus is inspected. The order above is a hypothesis, not a commitment.

## Principles

- **DuckDB is the result oracle for submitted SQL.** Expected rows come from executing SQL, not from model-written result lists. DuckDB cannot prove that the SQL matches the natural-language prompt; candidate review and the later validation stage must check that separate claim.
- **Frozen artifacts.** Model output is non-deterministic, so accepted cases are committed as data files. `cargo test` must not need DuckDB, a network, or a model. This honors the AGENTS.md rule against live provider calls in tests.
- **One grammar feature per step.** The first rung is an explicit, single-column text equality filter. Comparison operators, Boolean combinations, header-first rules, and other forms get separate reviewed steps.
- **Agents do only creative work.** Anything mechanical (running SQL, writing test files, running cargo, sequencing) is code.
- **Separation of duties.** The case-generation agent cannot edit the planner, and the agent that edits the planner cannot edit accepted cases or expected rows. Held-out cases are expected for the implementation stage to discourage overfitting.
- **Human review gates.** Generated cases are reviewed before promotion into the test corpus, consistent with the epic-first workflow in `AGENTS.md`.
- **Intent text is context, not configuration.** The corpus checks planner behavior. It does not change the rule that language models never emit or execute code inside Baho.

## Scope and ownership

- Tooling lives outside the product crates, in a dedicated package (proposed `tools/synth`) that no application or core crate depends on. This keeps `baho-core`, `baho-plan`, `baho-exec`, and the ingest crates free of DuckDB, async runtimes, and agent frameworks, as `AGENTS.md` requires.
- The tooling package is **excluded from the Baho workspace**, as `baho-llm` is today, and has its own lockfile and `rust-version = "1.95"`. Rig 0.43.0 declares Rust 1.95 while the Baho workspace declares 1.85. The tool depends on the exact crates.io release `rig = "=0.43.0"`; `~/Projects/rig` is reference source only. Epic 014 uses the installed DuckDB CLI, so the tool has no bundled DuckDB build. There is no reason to raise the Baho workspace minimum for a development tool.
- **Rig is the agent SDK** for the agent stages of this tooling (decided). The `llm-sdk` path dependency in `baho-llm` remains the product's provider transport for a future planner and is not used here. The repository therefore holds two LLM stacks on purpose: Rig for development-time agents, `llm-sdk` for product runtime.
- The user-supplied `.datasets/Kaggle_Small_Business_Financial_Dataset/gusto_payroll_bc.csv` is a local, untracked exploratory source for Epic 014. Generated candidates and records stay gitignored. Only reduced synthetic datasets and reviewed case files can become committed regressions; the exact accepted-case layout is set when test emission is planned.
- Test code that consumes case files lives in the crate that owns the behavior, today `crates/baho-core/tests/`.
- Planner changes remain in the crates that own them per `AGENTS.md`. This epic changes none of them.
- `baho-llm` stays transport-only. Agent prompts and policy for this tooling live with the tooling.

## Out of scope

- Any change to the planner, plan IR, or executor.
- Excel, PDF, GUI, or daemon work.
- Using real user documents or the untracked `.datasets/` Kaggle downloads as committed fixtures.
- Negative-outcome cases (clarification, refusal, unsupported prompts). A SQL oracle cannot express them, so they stay hand-written.
- Evaluating an LLM-based planner. The corpus may later serve that purpose, but it is not designed for it here.

## Alternatives and tradeoffs

- **Hand-written tests only.** Highest precision and clearest intent, but slow and prone to author bias. Retained for plan-shape assertions and negative outcomes.
- **Model writes expected rows directly.** Rejected: harder for the model and unreliable, and it removes the independent oracle.
- **Agent writes the Rust tests.** Rejected for now: adds variance and review burden to a mechanical step that one data-driven harness can do.
- **LLM orchestrator.** Rejected for now: stage sequencing is deterministic, and gates should not depend on model judgement.
- **Property or fuzz testing against DuckDB without natural language.** Useful for the executor but does not exercise the phrasing space that motivates this work.

## Acceptance criteria

- Each stage has its own reviewed epic before implementation.
- A frozen case file can be replayed on a machine without DuckDB or network access.
- The generation tooling is not a dependency of any core or application crate.
- No raw local source, generated candidate, or source-derived expected-row list is committed. Promotion requires a separately reviewed synthetic fixture and recomputed oracle rows.
- Planner expansion done through this loop is traceable: a test failure names the case, its feature rung, and the originating generation record.

## Fixture and test strategy

The first exploratory source is the user's local payroll CSV, with the agent exposed only to the `Role` column and bounded value counts. This keeps the first batch focused on explicit text equality and prevents payroll details from entering provider prompts. Once a useful phrasing is identified, create a small synthetic CSV with neutral values, rerun the SQL oracle, and review that new case before committing it. Later rungs may introduce header/value collisions, blank cells, and Boolean-shaped values when their own behavior is specified. Case files carry a schema version; later epics define validation gates and any held-out split.

## Implementation outline

1. Epic 014: build the case-generation agent (on Rig) and harness and inspect what it produces.
2. Review the corpus quality and decide the validation stage and the case file format.
3. Plan test emission, then validation, then implementer constraints and orchestration, each in its own epic.

## Deferred decision

- How the implementer stage (4) will be run. Rig is a candidate, but a general coding agent needs file editing and `cargo` execution tools that Rig does not appear to ship. Decide when that stage is planned.
