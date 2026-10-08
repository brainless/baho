# Epic 016: Epic Planner Agent (Agent 2)

Status: Completed

Motivating run: `generation-1791425654630970000-3920`, the latest Agent 1 (Synthetic Case Generator) output, plus a manual probe of `baho-cli` against its five candidates on a small synthetic CSV. No `.baho/runs/` record motivates this epic.

User intent: Build a second narrowly scoped Rig agent, using the same fixed Neuralwatt model as Agent 1, that reads the latest Agent 1 corpus together with the observed results of running its prompts through `baho-cli`, reads the existing Rust code through bounded read-only tools, and writes exactly one thing: a new proposal under `epics/` that plans one evidence-supported parser/planner increment toward the high-level goals below. The agent never changes the parser, planner, executor, tests, or any other file. A person reviews the epic before anything is implemented.

This epic is implemented by a coding agent. Its result is **new development tooling in `tools/synth`**, not a change to `baho-core`, `baho-plan`, `baho-exec`, or any other product crate.

## Observed behavior and problem

Epic 013 describes a loop: generate prompt/SQL pairs, validate them, replay them against the pipeline, change the planner. Epic 014 built the first stage. Nothing yet connects what Agent 1 produces to what Baho actually does with it, and turning that gap into a reviewed plan is manual work.

Evidence from a manual probe (synthetic four-row CSV with a `Role` column, neutral values substituted below; `baho-cli` built from the current tree):

| Prompt shape | Observed outcome |
| --- | --- |
| All five Agent 1 prompts (polite lead-in, `Keep records with…`, `For Role = '…', please return the matching rows.`, `…Role matches X.`, `I'd like the records where the Role column equals '…', please.`) | Refused: `expected action verb, got 'could'` or `column not found for term 'for role = …'`. All five refused. |
| `Role = Analyst`, `Show rows where Role = Analyst` | Works. |
| `Rows where Role = Analyst` | Refused. `rows`/`where` are keywords only after `list`, `show`, or `filter`. |
| `Role is Analyst` | **Succeeds with the wrong plan** `Role = "is Analyst"`; 0 rows. |
| `Show rows where Role = 'Analyst'` | **Succeeds with the wrong plan**: single quotes are not quoting, literal is `'Analyst'`; 0 rows. |
| `Role = Analyst?`, `… Analyst.` | **Wrong plan**: trailing punctuation joins the literal; 0 rows. |
| `Show rows where Role is not Analyst` | Refused: `unexpected token 'not'`. |
| `Show Analyst` (value only) | Refused: `column not found`. Epic 012 documents `Show <value>` as a filter; the discrepancy is unexplained. |

Cause, from `crates/baho-core/src/intent.rs`: the vocabulary is closed and small. Actions are `list`, `show`, `filter`; row keywords are `rows`, `where` (only after an action); operators are symbols and `is [not] blank`; quotes are double only; an unquoted literal consumes words to the next connector or operator. The probes show successful execution of predicates that differ from the intended predicates. Empty output alone would not establish this; the recorded literal and synthetic input supply the evidence. These cases motivate stricter intent interpretation and diagnostics.

The problem this epic addresses is process, not one grammar gap: each new grammar rung (`and`/`or`, `not`, `contains`, `starts with`, numeric comparisons, `sort by`, `limit N`) needs the same analysis. Evidence (what fails, why, how often) must be gathered mechanically, and the resulting plan must keep consistent architectural goals instead of accumulating special cases.

## High-level goals the agent must carry into every epic it writes

These come from the user's design direction and from the current code. They are given to the agent verbatim as a goals file and checked mechanically where possible.

1. **Parse → Plan → Execute stay separate.**
   - *Parse*: English → normalized words → syntax tree with source spans → typed intent. It produces a plan and evidence. It never executes.
   - *Plan* (`baho-plan` and plan-building code): the place that validates columns, coerces and validates literal types against the schema, and later may reorder filters or push projections down.
   - *Execute* (`baho-exec`): consumes only the typed plan and typed rows, streaming, bounded memory. It never sees English, prompt text, or an unparsed literal. This already holds today (`execute_plan_stream` over typed grid rows); the epic must preserve it and add a test or check that makes violations visible.
2. **Header knowledge feeds the parser.** Valid column names are the best disambiguator. Today `header_phrases(columns)` already drives complete-header matching and the header-first rule (Epic 012), and value grounding uses observed values (Epic 007). New work should extend, not bypass, this: for example use column types or known value sets only where determinism and the header-first rule survive.
3. **Operators and keywords are normalized early to enums.** `ComparisonOperator` is already a `baho-plan` enum, but English-to-enum mapping is scattered and symbols-only. A normalization stage maps words to enums (`is`/`equals` → `Equal`, `is not` → `NotEqual`, and later `Contains`, `StartsWith`, sort direction, limit) before the plan is built. The planner and executor never see English.
4. **Matching semantics are decided up front and testable.** Text equality is case-insensitive full-lowercase exact comparison (`TextMatchPolicy::UnicodeLowercase`, Epic 008): `OWNER` equals `owner`, outer whitespace is not trimmed, `ß` is not `ss`. Numeric comparison uses the strict-decimal policy of Epic 008. Any new text operator (`contains`, `starts with`) must state its fold policy before design, and each policy needs a table-driven semantics contract test. The agent may propose a change to these semantics only as an explicit, flagged decision for the user.
5. **Closed vocabulary, no guessing.** Accessory words (`could you`, `please`, `the matching rows`, `column`) are removed only by an explicit allowlist of phrases with stable rule IDs, retaining the stripped spans in evidence. Rules are position-aware and apply only to syntax positions, never blindly across headers or literals. Never skip unknown words. Never strip negation, connectors, or comparators. Any word left unconsumed produces a refusal naming it. Ambiguous words such as `matches` (Epic 015 flagged it) are refused or clarified, not silently mapped.
6. **Silent wrong answers come first.** Confirmed wrong-plan successes outrank new vocabulary, whether their results are empty or nonempty. Row mismatch is evidence to investigate; empty output alone is not a wrong-plan verdict.
7. **Each rung extends the grammar without reworking it.** `and`/`or`/`not`, `contains`, `starts with`, numeric comparisons, `sort by`, and `limit N` must each fit as a small, separately reviewable addition. This usually means a new `Expression` or `PlanStep` kind in `baho-plan` with a plan-version bump and compatibility coverage, or a new rule in a normalization table. The epic must say which, and how the existing precedence rules (`not` > `and`/`but` > `or`) are kept.
8. **Product invariants hold.** Language models only propose through constrained plans; no LLM or code execution in parsing or execution; deterministic ordering and diagnostics; stable diagnostic codes; zero-based internal indices; crate ownership as in `AGENTS.md`.

## Desired behavior

Two developer commands in the existing excluded `tools/synth` package, never invoked by `cargo test` (except offline fixtures), the CLI, or the GUI.

### 1. `synth evaluate`: deterministic, no agent

```text
synth evaluate --dataset <csv> [--generation <dir>] --baho-bin <path> [--review <file>] [--output <dir>]
```

1. Resolve the Agent 1 output directory. Without `--generation`, choose the greatest generation timestamp, compared **numerically** from the ID, never by modification time. Reject ties and malformed generation IDs instead of guessing.
2. Read `candidates.txt`/`metadata.jsonl`/`generation.json`; verify the dataset hash with Agent 1's trusted loader. Assign stable case IDs and retain generation and candidate-content hashes.
3. Re-run each stored SQL through Agent 1's restricted DuckDB runner. Compare the computed rows with the stored rows; an oracle mismatch is `oracle_error`, and that case is not run against Baho. SQL execution establishes the answer to SQL, not agreement between SQL and English.
4. Run each oracle-valid prompt, including unreviewed cases, through the separately built `baho-cli` subprocess against a `Role`-only derived CSV. Preserve the header and logical-record row order and verify the row mapping. The tool does not link Baho crates. The subprocess working directory is a fresh evaluation directory under gitignored `synthetic/out/`, including its `.baho/runs/`. Bound process duration and captured output. Record the binary hash and version, repository revision and dirty flag, and the exact run ID for each case. A repository revision does not prove the binary was built from that revision.
5. Read versioned machine-readable run artifacts. Collect the outcome, stable diagnostic codes, bounded messages, typed plan evidence when available, and interpretation text. Compare retained source rows with oracle rows in memory; retain only counts of expected, actual, missing, and extra rows in the report. Do not persist row lists in evaluation reports or agent records. Raw CLI artifacts remain private and inaccessible to Agent 2.
6. Produce the versioned, sanitized `evaluation.json` described below. Stable case ordering and logical evidence are deterministic for fixed inputs and binary; evaluation IDs, timings, and run IDs are volatile metadata. Aggregate outcomes separately from review status and plan comparison.

### Evidence contract

Each case records three independent assessments:

- **Prompt/SQL review:** `unreviewed`, `confirmed`, `ambiguous`, or `rejected`. Default to `unreviewed`; neither Agent 1 tags nor an oracle recheck can upgrade it. An optional versioned `--review <file>` imports explicit human decisions bound to the generation ID, case ID, and candidate-content hash. A mismatched review file fails validation. Record the review decision and its bounded, sanitized rationale. This command does not implement Epic 015's validation workflow.
- **Observed result:** `rows_match`, `wrong_rows`, `refused`, `clarification`, `error`, or `oracle_error`. `rows_match` means only agreement on this dataset, not semantic correctness. Reports describe row-agreement rates, not an unqualified planner pass rate.
- **Plan comparison:** `equivalent`, `different`, or `unavailable`. For the current single-equality SQL grammar, compare column, operator, typed literal, and text-match policy with structured Baho plan fields through a narrowly defined comparison contract. Use existing case-insensitive equality semantics rather than exact spelling of text literals. Do not infer plan equivalence from prose or matching rows. Unsupported or absent plan evidence is `unavailable`.

A confirmed wrong-plan success requires a `confirmed` prompt/SQL review, a materialized Baho outcome, and `different` plan evidence. An unreviewed mismatch remains a hypothesis about prompt interpretation, SQL intent, or Baho behavior. If structured plan artifacts are insufficient, report that limitation without changing product crates in this epic.

The report carries evaluation/generation IDs, hashes, artifact-schema versions, case IDs, review provenance, outcome and comparison fields, bounded sanitized evidence, aggregates, and harness-computed lexical facts. Lexical facts are hints, not trusted feature labels. Counts describe this candidate batch, not the frequency of phrasing among real users. Evidence citations use evaluation and case IDs; source-code claims cite repository-relative paths and line numbers.

Evaluation remains separate from Epic 015's frozen neutral replay. Share stable case identifiers and compatible definitions where useful, but do not add an integration dependency or promote local candidates into regressions.

### Redaction contract

The trusted evaluator builds one consistent neutral mapping for the profiled `Role` values. Repeated values share a stand-in; case variants preserve their relationship and case shape. Preserve word count where possible. Apply the mapping to every model-visible surface: prompts, normalized predicates, interpretation text, diagnostic messages, review rationale, goals, epic titles, file reads, search snippets, errors, and transcript metadata. Store the mapping only in a private gitignored sidecar accessible to the trusted harness, never through agent tools. `plan-epic` fails clearly if that sidecar is missing or does not match the evaluation hash.

Redact source paths and credentials as well as literals. Filter repository content before returning it through tools, including existing epics. Search only the sanitized view and return sanitized snippets; preserve line numbering for source citations. Do not expose raw provider payloads or raw tool arguments. Apply bounds after sanitization and scan every outgoing surface; reject a response that cannot be sanitized safely. Never send the mapping to the provider.

Stand-ins are explanatory evidence, not replay fixtures. If a case depends on punctuation, Unicode, a keyword/header collision, or another property the mapping cannot preserve, mark that limitation and withhold semantic conclusions from the altered text. A separately reviewed synthetic reproduction is needed before proposing a behavior fix for that property.

### 2. `synth plan-epic`: the Rig agent

```text
synth plan-epic --evaluation <evaluation.json>
```

Runs Agent 2 on the same fixed provider, model, and route as Agent 1 (Neuralwatt, `deepseek-v4.1-flash-flex`, streamed Chat Completions, `rig = "=0.43.0"`). No provider flags. It reports the served tier per call and does not retry at another tier. Reading `NEURALWATT_API_KEY` follows Epic 014.

**Job:** read the evidence and the relevant code, and write one proposed epic for one evidence-supported increment. It may list deferred ideas, but only the chosen increment has an implementation outline and acceptance criteria. If evidence is insufficient, finish with `no_epic` and a bounded explanation rather than inventing a roadmap.

**Inputs given to the agent**
- The sanitized evaluation report, including review status, distinct row and plan comparisons, and redaction limitations. Unreviewed cases support investigation proposals, not asserted parser defects.
- The goals file above, the `AGENTS.md` epic structure, Epic 013's principles, and the sanitized list of existing epic titles (full text is available only through sanitized read tools).
- A system prompt that states the epic is a proposal, requires evidence references, and forbids implementation instructions disguised as results.

**Tools (all provided by the harness, implemented as Rig `PortableTool`s)**
- `read_evaluation`: sanitized summary and pages of per-case results, with stable evidence IDs.
- `list_files` / `read_file(path, start_line, end_line)` / `search_text(literal, scope)`: read-only. Allowlisted roots: `crates/*/src`, `crates/*/tests`, `crates/*/Cargo.toml`, `apps/*/src`, `epics/`, `PRD.md`, `AGENTS.md`, `tools/synth/features`, `tools/synth/prompts`, `tools/synth/goals`. Paths are canonicalized after resolving symlinks and rejected if they leave an allowlisted root. Hard-denied: `.env*`, `.baho/`, `.datasets/`, `synthetic/out/` (except through `read_evaluation`), `target/`, `.git/`. All results pass through the redaction contract, including paths, snippets, and error messages. Each call and the whole run have byte and call budgets.
- `submit_epic(slug, markdown)`: the **only** write path, and the agent never has a filesystem handle. The harness validates the markdown, picks the file name, and writes it. See Output contract.

**Not available:** shell, network, code execution, DuckDB, `baho-cli`, any write other than `submit_epic`, and the dataset. The agent cannot run the planner; it reasons from the evaluation report and the source it reads.

**Bounds.** `max_turns`, per-call and whole-run time limits, tool-call byte budgets, and a cap of at most one accepted epic per run. Flex admission waits are expected, as in Epic 014. Turn exhaustion or timeout before an accepted submission is a normal recorded outcome with no epic written. Once a submission is accepted, stop the loop and record `epic_written`; a later provider failure must not misreport that committed result. A rejected submission returns its reasons so the agent can fix and resubmit within the budget.

## Output contract

The agent submits only a slug matching `[a-z0-9-]{3,48}` and bounded Markdown. The harness validates the complete submission before writing. The submitted document begins with a title and `Status: Proposed`; the harness adds an HTML provenance comment containing Agent 2, evaluation ID, and unreviewed status.

The harness allocates the next zero-padded number after the greatest existing `epics/NNN-*.md`. Allocation and publication are serialized across cooperating tool processes using an exclusive repository-scoped lock. Recheck the directory under the lock and publish the complete file atomically with no-overwrite semantics. Never renumber or overwrite; a collision caused by another writer requires fresh allocation. At most one submission can be accepted per run.

Mechanical validation before publication:

- Required sections cover motivating evidence and user intent, observed behavior, problem and desired behavior, out of scope, architecture and crate ownership, design and alternatives, acceptance criteria, fixture/test strategy, implementation outline, and unresolved questions.
- The document cites the actual evaluation/generation IDs and existing case IDs. An explicit goal-coverage table includes goals 1–8, with a decision or `not affected` and a reason for each.
- The document declares exactly one chosen increment. Deferred ideas have no implementation steps or acceptance criteria. Unreviewed or ambiguous evidence is labeled as such; an investigation epic is allowed when no behavior change is justified.
- An explicit path inventory separates **existing evidence files** from **proposed new files**. Every mentioned repository file path belongs to one category. Existing evidence files must exist within the allowed roots; proposed files may be absent but must be normalized relative paths within the stated owning crate or tooling directory. Reject traversal, absolute paths, and denied roots. This permits new regression fixtures and tests without claiming they already exist.
- The submission passes the redaction scan: no profiled source literals, source-derived row lists, source paths, absolute local paths, credentials, or credential-pattern matches. It does not propose copying local dataset contents into fixtures.

These checks enforce structure, traceability, and bounded scope. They cannot prove that an increment is coherent, that a claim about code is true, or that goal coverage is meaningful. Human review checks those claims, prompt/SQL agreement, priority, matching semantics, and the synthetic regression design before implementation.

## Records and observability

Each `plan-epic` run writes a record beside the evaluation (gitignored): model, route, Rig version, prompt and goals-file hashes, evaluation hash, the repository revision and dirty flag, per-call elapsed time and served tier, a bounded tool transcript (tool name, sanitized path or search term, byte counts, outcome; no file contents or provider payloads), rejected submissions with reason codes, and the final outcome (`epic_written` with its path, `no_epic`, `max_turns`, `timeout`, `provider_error`). Credentials, authorization headers, and source cell values never appear in records. Re-running with identical inputs makes differences traceable; the generated text may differ.

## Scope and ownership

- New code lives in `tools/synth` (excluded from the workspace, own lockfile, Rust 1.95, Rig `=0.43.0`). It reuses Agent 1's dataset loader, restricted DuckDB runner, Neuralwatt client, streaming/tier handling, and hook transcript. No product crate depends on it and none gains Rig, DuckDB, Tokio, or an LLM dependency.
- `baho-cli` is invoked only as an external process, built separately from the workspace. The harness fails with a clear error if the binary is absent or its `--help`/version output is not recognized.
- Agent prompts and the goals file live with the tooling (`tools/synth/prompts/`, `tools/synth/goals/`). `baho-llm` remains transport-only.
- The only file the running tool creates in a tracked repository directory is the epic, in `epics/`, which a person reviews and commits. Evaluation output and records are gitignored under `synthetic/out/`.
- Out of scope: any change to the parser, planner, plan IR, executor, or `baho-cli`; implementing the epic that the agent proposes; new Agent 1 feature files (the agent may propose them in the epic text); Epic 015's validation, review, and frozen replay; orchestrating agents in a loop; Excel, PDF, GUI, or daemon work.

## Design and tradeoffs

- **Mechanical evidence, creative planning.** Running prompts, comparing with DuckDB, and gating output are plain code (`evaluate`). Only turning evidence plus code into a plan is agent work. This matches Epic 013's rule that agents do creative work only.
- **Output restricted by construction.** The agent cannot write arbitrary files because the only write tool takes markdown and a slug, and the harness owns the path. A prompt-injection attempt in source comments or a candidate prompt can at worst produce a bad draft epic, which fails validation or review.
- **Run the CLI, don't link it.** Using the binary mirrors what the user does, includes the CLI's run-artifact behavior, and keeps the tool out of Baho's crate graph. A linked `baho-core` call would be faster but would hide CLI-level failures.
- **Role-only derived CSV.** This keeps payroll columns out of CLI run artifacts and the model while preserving row indices; Epic 014's preflight already established the row mapping. A later multi-column rung will need a different column-projection decision.
- **Sanitized evidence throughout.** Consistent substitution preserves value relationships across evidence and repository reads. Some lexical properties cannot be preserved; those limitations remain explicit and require synthetic reproduction. Submission scanning supplements the outgoing-content boundary.
- **Evidence-bound scope.** Choose one increment, prioritizing confirmed wrong-plan successes, then reviewed refusals in this batch, then new constructs supported by evidence. An investigation is preferable when case meaning or plan evidence is unresolved. Five equality cases do not justify later sorting or numeric work.

Alternatives considered:
- *A coding agent writes each epic by hand.* This is the status quo. Repeatable and slow; the new agent standardizes the evidence and goals, and a person still reviews.
- *Give the agent edit rights to a draft branch.* Rejected: larger blast radius, and the separation of duties in Epic 013 forbids it.
- *Skip the code and plan from evidence only.* Rejected: it cannot tell a missing keyword from a structural limit, such as the plan IR having no sort step.
- *In-process evaluation through `baho-core`.* Rejected for the reasons above; Epic 015's replay covers that path.
- *One command that evaluates and plans.* Rejected: evaluation is useful alone, and the stages are easier to test separately.

## Acceptance criteria

- `evaluate` produces a versioned report from actual CLI artifacts with stable case ordering, separate review/result/plan fields, hashes, counts, and sanitized evidence. Historical probe outcomes are context; changed binary behavior is reported as observed rather than forced to match them.
- Generation selection is numeric and rejects ties and malformed generation IDs; an older generation with newer modification time is not selected.
- Review defaults to `unreviewed`. Imported decisions are hash-bound; a stale review fails. Oracle rechecks cannot upgrade review status. Oracle mismatch prevents CLI execution for that case.
- Tests show that equal rows with different predicates remain `rows_match` plus `different`, an empty but equivalent result is not called a wrong plan, and absent structured evidence yields `unavailable`. Confirmed wrong-plan classification requires all three conditions in the evidence contract.
- Reports and agent records contain counts, never source-derived row lists. Raw CLI artifacts and the redaction sidecar remain private. No model-visible surface contains source literals, source paths, credentials, or unrelated source-column data.
- Redaction tests cover repeated values, case variants, diagnostics, interpretation text, review rationale, existing epic contents/titles, search snippets, errors, and transcript fields. Mapping limitations are reported; a missing or mismatched sidecar fails before a model call.
- Read tools reject traversal, symlink escape, denied roots, and oversized reads. Search and reads use the sanitized view with usable source line numbers.
- Rejected submissions write nothing. A valid submission creates one complete proposed epic. Tests cover concurrent allocation, collisions, no overwrite, and refusal of a second accepted submission.
- Output validation covers missing sections/goals, invalid evidence IDs, multiple chosen increments, invalid path inventory, missing existing files, valid proposed files, denied proposed paths, forbidden content, oversized bodies, and bad slugs.
- Rig mock tests cover successful reads/submission, correction after rejection, insufficient evidence yielding `no_epic`, forbidden reads, turn exhaustion, timeout, missing credentials, and terminal success after acceptance. No test calls a live provider.
- Records include fixed model/route, served tier per call, revision/dirty flag, sanitized transcript, and accurate final outcome. Missing or standard tier is visible without retrying at another tier.
- Tooling stays outside the workspace dependency graph. Root workspace checks remain unaffected.
- Before Agent 2 is wired, a person reviews the evaluator report and confirms that evidence fields and limitations support planning. After one live planning run, append a sanitized review note covering tool reliability, the chosen increment, goal coverage, evidence quality, and unsupported claims found during review.

## Fixture and test strategy

- Hand-authored synthetic CSV and generation/review files cover mixed-case values, repeated values, empty matches, coincidentally equal row sets from different predicates, stale review hashes, and oracle mismatches.
- A stub CLI writes realistic versioned run artifacts, including structured plans and provenance; tests must exercise artifact parsing rather than scripted terminal messages alone. An optional ignored test runs the real binary on synthetic data.
- A temporary mini-repository contains denied paths, symlink escapes, oversized files, and synthetic sensitive markers in epics, titles, comments, and errors. Test sanitized reads and searches as well as rejection paths.
- Concurrent submission tests verify allocation and complete atomic publication. Mock-model tests exercise the loop and redaction without real source data or provider calls.
- Live evaluation and planning are manual developer actions. Generated text is reviewed by a person and never asserted verbatim in CI.

## Implementation outline

1. Inspect existing CLI artifact schemas and the trusted Agent 1 loader. Specify the report, optional review file, redaction sidecar, and supported normalized predicate comparison before coding. State behavior captured by the synthetic contract tests.
2. Implement and test numeric generation selection, oracle recheck, projected CSV/row mapping, bounded subprocess execution, artifact parsing, review import, row/plan comparisons, and sanitized report publication.
3. Run `evaluate` and review its report before wiring Agent 2. Resolve inadequate fields or ambiguous classifications within the tooling scope; record unsupported product-artifact cases explicitly.
4. Write the goals file and single-increment system prompt. Implement sanitized read tools, evidence/path validation, concurrent epic allocation, and atomic no-overwrite submission under tests.
5. Wire Rig, transcript hooks, bounds, redaction-sidecar loading, and terminal outcomes using Agent 1's client and tier handling. Run mock tests, tool checks, and appropriate workspace checks.
6. Run one live `plan-epic` smoke test and append the review note. Review the generated proposal through the ordinary epic workflow. Implementation of its chosen increment is a separate task after approval.

## Tasks

Each task is delegated to one subagent in order. A task builds on the previous ones, adds tests, and keeps `tools/synth` outside the workspace.

- **T1 (done): Contracts and generation intake.** Specify the report, review file, redaction sidecar, and plan-comparison contracts (as typed serde structures with schema versions). Implement numeric generation selection, generation loading with Agent 1's trusted loader, stable case IDs and hashes, and the oracle recheck.
- **T2 (done): CLI execution and artifact parsing.** Role-only derived CSV with row mapping, bounded `baho-cli` subprocess runner in a fresh evaluation directory, binary/version/revision capture, and parsing of run artifacts (outcome, diagnostics, plan evidence). Includes the stub CLI used by tests.
- **T3 (done): Comparison, review import, and redaction.** Row-count comparison, plan comparison, hash-bound review import, outcome classification, and the neutral-mapping redaction module with private sidecar and outgoing-surface scanner.
- **T4 (done): `synth evaluate` command.** Wire T1–T3 into the command, aggregates, and sanitized `evaluation.json` publication, with the acceptance tests for evaluation.
- **T5 (done): Goals, prompt, and read tools.** Goals file, single-increment system prompt, and the sanitized `read_evaluation`, `list_files`, `read_file`, `search_text` tools as Rig `PortableTool`s with allowlist, denylist, symlink, and budget enforcement.
- **T6 (done): Epic submission.** `submit_epic` validation (sections, goal table, evidence IDs, path inventory, redaction scan), serialized number allocation, atomic no-overwrite publication, and one-accept-per-run, under tests.
- **T7 (done): `synth plan-epic` agent.** Rig wiring using Agent 1's client and tier handling, bounds, sidecar check, records and transcript, terminal outcomes, and mock-model tests.
- **T8 (done, manual): live smoke test.** Live `plan-epic` run and review note. Needs the user's approval to send repository source to the provider.

## Unresolved questions

- Is sending repository source files, not user data, to the Neuralwatt provider acceptable? It is the same provider Agent 1 uses, but Agent 1 received only a `Role` profile. The tool's allowlist can exclude paths if any should stay local.
- `Show <value>` refuses on the current build although Epic 012 documents it as a filter. Is this a bug to investigate first, or an intended limit? Agent 2 should surface it, but a person decides.
- Which version-bump policy applies when a rung adds an `Expression` or `PlanStep` kind (`contains`, `sort`, `limit`)? The epic only requires that the agent name the bump and compatibility coverage.
- Should later rungs make `evaluate` multi-column, and how should the Role-only derived CSV generalize without exposing unrelated columns?

## Review note: live runs (T8)

Recorded 2026-10-08 by a coding agent from the gitignored `plan-*.json` records; the epics those runs produced are not summarized here beyond what is stated.

**Runs.** Five `plan-epic` records exist across three evaluations of two generations. Outcomes: `max_turns` (40 model calls), `provider_error`, `no_epic`, `epic_written` (one rejected submission, code `path_unlisted`, then accepted), and a final `epic_written` with no rejections (16 calls) after the evidence changed. Every recorded call was served at the `flex` tier.

**Tool reliability.** The tools worked end to end: reads, search, rejection feedback, correction, and atomic publication of one epic. Failure modes seen live and since handled in code: tool-call markup written as text (`malformed_tool_call`), prose endings (`no_terminal_action`), and an explicit `finish_no_epic`. The `provider_error` run predates the structured `provider_error` field, so its class was not recorded. The first batch exhausted its turn budget.

**Chosen increment and evidence quality.** The first epic produced (accessory-phrase refusal classification) was a defensible but low-value relabelling, because the evaluated batch contained only refusals and no plan comparison evidence. The agent could not see silent wrong plans: evidence quality, not agent reasoning, was the limit. A second batch from `tools/synth/features/role-equality-near-grammar.md`, reviewed by hand, produced 7 confirmed wrong-plan successes in 10 cases, and the agent then proposed an increment aimed at them (Epic 017).

**Goal coverage.** Both written epics carried a complete goals 1-8 table; this is enforced mechanically, so its presence says nothing about its quality.

**Unsupported or weak claims found in review.** In the second epic, a count was wrong (said four cases needed both rule families; three do) and a risk was missing (word operators becoming literal boundaries). Both were corrected by hand in Epic 017. Mechanical validation cannot catch either; human review remains required.

**Follow-ups.** Provider-error class for the failed run is unknown. Clippy now passes on `tools/synth` and the all-spawn-failed exit path has a test.

**Follow-up review (2026-10-08, prompt `epic-planner-v3`).** The earlier Epic 017 draft was deleted and regenerated. It had a scope contradiction (a "position-aware" operator rule implemented through the global `is_boundary_token`), a goal-2 verdict that contradicted its own risk, and design choices left as open questions. Validation now also requires a Risks and regressions section, at least three `path:line` citations that resolve inside inventoried files, at most four unresolved questions each with a `Recommended:` default, and goal decisions that begin with `Affected` or `Not affected`. The prompt adds a verification and scope-mechanism quality bar. The regenerated draft (32 calls, no rejections, after one `provider_error` transport failure mid-run) cited correct lines and avoided the boundary-token trap. Hand fixes remained for fixture/criteria mismatch and an unlisted risk (other `parse_literal` callers). Mechanical checks still cannot prove that claims about code are true.

**Context pack and token usage (2026-10-08, prompt `epic-planner-v4`).** `plan-epic` records per-call token usage (Rig's `ModelTurnFinished.usage`; the provider reports input, output, total, and cached input tokens) and accepts `--context <name>` to inject a harness-resolved pack (`tools/synth/context/parser-equality.json`, 58,503 bytes) ahead of the evaluation. Same evaluation, one run each, both `epic_written`, zero rejected submissions, all flex tier:

| | no pack (Epic 018) | pack (Epic 019) |
|---|---|---|
| model calls | 16 | 10 |
| tool calls (excl. submit) | 33 | 20 |
| input tokens (cached) | 555,363 (516,608) | 379,824 (342,016) |
| output tokens | 29,598 | 26,893 |
| elapsed (sum of calls) | 998 s | 666 s |

Both chose the same increment (word operators `is`/`equals` plus trailing punctuation and single quotes). A 10-citation spot check of Epic 019 against the tree found no wrong line. Caveat: Epic 019 re-proposes Epic 018 (it names the earlier drafts as duplicates), so a pack run should not follow a run of the same evaluation without pruning the earlier output. One sample each; not statistically meaningful.
