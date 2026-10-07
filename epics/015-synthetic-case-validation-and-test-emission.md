# Epic 015: Synthetic Case Validation and Test Emission

Status: Proposed

Motivating run: None. This development-process proposal follows Epic 014's Task 5 preflight and live-trial status note dated 2026-10-07. The completed live review is recorded in Epic 014; local records remain gitignored.

User intent: Turn reviewed explicit `Role` equality phrasings into safe, frozen synthetic regressions whose expected source rows come from DuckDB and whose replay exercises `baho-core::run_pipeline` without a provider or DuckDB dependency.

## Evidence and planning gate

Epic 014 reports a successful row-mapping preflight on a three-row synthetic CSV and the local trial CSV (266 data rows and five distinct Role values). Header row 0 and first data row 1 agree with Baho inspection, and every profiled equality query returned the same source row indices. Evidence is retained locally under gitignored `synthetic/out/task5-preflight/`.

The successful streamed smoke produced one accepted case and three completed model turns. The following batch produced five proposed and accepted cases, zero rejected submissions, zero empty results, and zero full-table results. All eleven batch tool outcomes succeeded. All seven completed calls across those successful runs reported Flex on Neuralwatt's fixed `deepseek-v4.1-flash-flex` Chat Completions route. An earlier sandbox access failure produced no cases and was retained separately. These observations establish successful live execution for this trial, rather than broad protocol reliability.

The five cases cover all five Role values with one phrasing each. Result sizes were 6–104 of 266 rows (approximately 2.3%–39.1%). Review found no prompt/SQL disagreement under the feature's exact-equality interpretation, but the sanitized wording “For rows whose Role matches Analyst, please return them.” is mildly ambiguous: “matches” can mean substring or pattern matching. Mixed-case literals follow the specified case-insensitive semantics. Baho's planner was not run against the candidates; planner acceptance and end-to-end equivalence remain unmeasured.

This evidence supports human semantic review, exact prompt duplicate flags, and empty/full-result flags without a numeric selectivity cutoff. Begin with up to five neutral reductions of useful batch phrasings, subject to an explicit ambiguity gate. The trial does not justify independent model consensus, broader similarity deduplication, or a meaningful held-out split. Review this proposal before implementation; generation acceptance does not establish corpus approval.

## Problem and desired behavior

Execution of candidate SQL establishes its rows, but cannot establish whether the prompt means that query. Epic 014's submission checks only require the named column and literal. A candidate needs separate semantic review, and its local dataset values and expected rows must not be promoted verbatim.

The intended next stage validates candidate metadata and oracle results, records explicit review decisions, reduces useful phrasings onto neutral synthetic fixtures, recomputes their rows, and emits frozen case data. One deterministic Rust integration harness then runs those data files through the public pipeline and reports failures by case ID. Planner implementation follows a later epic; discovering a legitimate failing case must not cause this stage to rewrite its expected rows.

The first scope remains one named, ASCII text equality predicate on `Role`, returning matching rows in source order. Boolean combinations, header-first forms, sorting, projections, aggregates, negative outcomes, model planners, planner fixes, and orchestration are out of scope.

## Ownership and proposed design

Validation and promotion commands belong in the excluded `tools/synth` package. Reuse its trusted loader and restricted DuckDB query runner; no product crate acquires Rig, DuckDB, or Tokio. Replay code and neutral fixtures belong under `crates/baho-core/tests/`, alongside the existing intent regression tests. Product logic remains in its existing owning crates.

1. Read draft candidates with explicit schema-version and size limits. Check unique IDs, generation-record linkage, feature tag, dataset identity/hash, SQL grammar, in-range unique sorted rows, and internally consistent result statistics. When the original local dataset is supplied, recompute results through the restricted runner and reject any mismatch. A missing or changed dataset prevents oracle verification rather than silently trusting saved rows.
2. Produce a local review report with stable reason codes and separate mechanical validity, semantic review, and promotion status. Human review must judge that the prompt asks for exactly the submitted equality predicate. A valid SQL result or a successful current planner result is insufficient grounds for approval. Flag unqualified “matches” for explicit review; hold or reject it unless the reviewed prompt itself unambiguously requests equality. The hidden feature description cannot resolve ambiguity in a future user prompt. Ambiguous or out-of-scope phrasing is rejected or held for another rung.
3. Re-express approved phrasings using neutral Role values on a separately reviewed small CSV. Preserve the language construction being tested, including meaningful case or quoting, while removing local source literals and records. Re-execute the rewritten SQL on that fixture. Verify row mapping and review the new prompt/SQL pair before promotion; never transfer the local expected-row list.
4. Emit a versioned accepted-case format, tentatively JSON with case ID, feature tag, relative synthetic fixture ID/hash, neutral prompt, canonical SQL, recomputed expected source rows, and an opaque origin generation-record ID. Include stable review and oracle provenance sufficient to explain promotion. Keep source paths, raw transcripts, credentials, and volatile runtime metadata out of committed cases. Reject unknown schema versions; future changes require a version bump and compatibility tests or a migration decision.
5. Add one data-driven Rust integration harness that deserializes frozen cases and invokes `run_pipeline` on their synthetic fixtures. Assert successful materialization, exact retained `provenance.source_row` values in order, and raw values/source coordinates where relevant to the contract. Report clarification, unsupported intent, errors, unexpected diagnostics, and row mismatches by case ID and feature. Determine the precise public pipeline result fields during implementation. Replay never executes stored SQL and requires no external process, network, credential, or model.

The accepted-case layout and review record fields are provisional until this proposal is reviewed. Emission means writing case data for the shared harness, rather than generating separate Rust source for every phrasing. Publication into the committed corpus is an explicit reviewed promotion action; generation alone cannot perform it.

## Alternatives and tradeoffs

- Human semantic review is the initial proposal for this small rung. A second independent SQL translation could expose disagreements, but identical translations do not prove meaning. The trial found no definite disagreement and one wording ambiguity. Introduce it only if a larger reviewed corpus demonstrates a need; its provider, cost, and disagreement policy would need a separate explicit design.
- Exact normalized prompt duplicates can be flagged mechanically. Equal SQL or row sets should be grouped for review rather than automatically removed, because distinct phrasings of one predicate are the intended coverage. Broader similarity rules await corpus evidence.
- Empty and full-table results are flagged for review. None occurred in the trial, and its observed selectivity range gives no basis for a numeric cutoff. Neutral fixtures should contain matching and nonmatching rows so an equality regression distinguishes filtering from returning everything.
- A data-driven harness keeps replay uniform and review focused on inputs and oracle rows. Individual handwritten tests remain appropriate for plan-shape assertions and negative outcomes.
- Failing accepted cases may initially need an explicitly invoked evaluation target rather than joining the default passing suite. Choose that integration policy during review, preserving failures visibly until the later planner stage resolves them.

## Acceptance criteria

- This proposal and its remaining format/evaluation decisions are reviewed before implementation; Task 5's live note is complete.
- Invalid schema, broken linkage, changed fixture hash, malformed SQL, invalid row ordering/range, inconsistent statistics, and oracle mismatches receive deterministic reasons and cannot be promoted.
- Semantic review and review of the reduced synthetic pair are recorded explicitly; mechanically valid candidates cannot self-approve.
- Every accepted case uses only reviewed neutral synthetic data with rows recomputed by DuckDB. No local candidate, source row list, source path, or payroll literal is copied into committed cases.
- Given unchanged reviewed inputs, emission produces identical accepted-case bytes and ordering; it refuses accidental overwrites of frozen cases with changed expectations.
- Replay on a machine without DuckDB or credentials exercises the public pipeline and reports the case ID, feature, origin record, and observed outcome for every failure. No test makes a provider call.
- Tests distinguish a correct equality filter from returning all rows, preserve header/data source mapping, and cover mixed-case ASCII equality and repeated matching values.
- Tooling stays outside the workspace dependency graph. The chosen evaluation integration policy preserves a passing normal baseline while exposing legitimate planner gaps explicitly.

## Fixture and test strategy

Use hand-authored neutral CSVs and candidate/review records for validation tests, including invalid grammar, tampered oracle rows, schema versions, duplicate IDs, absent approval, and changed hashes. Test oracle recomputation in the tool's development tests; use the installed DuckDB CLI only there. Use synthetic marker values in redaction tests, never real source values.

For replay, begin with a tiny one-header CSV containing mixed-case repeated Role values and at least one nonmatching row. Freeze independently recomputed rows and fixture hashes. Cover successful replay and clearly attributed unsupported, clarification, and mismatch reports through neutral fixtures. A regression test must express the intended equality behavior before any later planner fix.

Run focused tool and replay tests first, followed by their owning suites. Rust changes require root `cargo fmt --check`, `cargo check --workspace`, and `cargo test --workspace`; run the excluded tool's checks separately using its documented compiler overrides if still required. No implementation or tests are added by this proposal.

## Implementation outline

1. Review this proposal, finalize schemas and evaluation integration, and select up to five phrasings for neutral reduction with explicit ambiguity decisions.
2. Define the versioned accepted-case and local review schemas with frozen-artifact and privacy rules; inspect the current public pipeline API and existing integration tests.
3. Implement deterministic candidate validation and independent oracle rechecks in `tools/synth`, with focused synthetic tests.
4. Implement explicit review import and synthetic promotion/emission with recomputed rows and deterministic output. Obtain review of the initial neutral cases.
5. Add the shared pipeline replay harness and initial frozen synthetic cases under the agreed evaluation policy; report planner gaps without changing planner behavior or oracle expectations.

## Unresolved questions requiring evidence or review

- Which reviewed neutral wording should replace or exclude the ambiguous “matches” example while preserving useful coverage?
- Does a larger corpus later justify independent translation or broader duplicate rules? This trial supports human review and exact duplicate flags.
- Should future empty/full results become separate positive boundary fixtures? None were observed in this batch.
- When a larger corpus exists, what held-out allocation is useful? No split is proposed for this five-case trial; implementer access controls remain a later-stage decision.
- Should known unsupported accepted phrasings run in an explicit evaluation target until the planner epic lands, or should emission wait for coordinated passing implementations?
- What accepted-case layout and minimum review/oracle provenance best support future replay without exposing local source data?
