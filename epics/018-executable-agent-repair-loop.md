# Epic 018: Executable Investigation and Agent Repair Loop

Status: In progress — live trial unavailable pending working process confinement.

Motivating run: `plan-1791532155115582000-44931`, evaluation
`evaluation-1791470942646632000-53201-0`.

User intent: Improve baho incrementally from reviewed NL/SQL pairs using an
investigator, a coder, deterministic checks, and an independent reviewer. Let
agents test their predictions and preserve the process whether a patch passes
or fails. Keep Epic 017 as a generated proposal; this epic does not implement it.

## Evidence and problem

Epic 014's generation and Epic 016's evaluation provide a usable starting point.
The motivating planner run completed investigation, drafting, and review, then
published Epic 017 after a review pass. Its small operator-recognition design is
plausible, but its acceptance contract includes an unreachable expected row:
the fixture's entirely blank record is skipped by `find_body_rows` in
`crates/baho-ingest-csv/src/candidates.rs`. It also describes an already-refused
prompt as a newly introduced refusal. Source citations and independent prose
review did not establish those claims.

The missing feedback is execution. Existing planner tools read source and
evaluation reports but cannot run a proposed fixture. `tools/synth/src/cli_run.rs`
already supports bounded CLI execution and artifact parsing; `evaluate.rs`,
`evaluation.rs`, and `contracts.rs` provide oracle rechecks and separate row,
plan, and semantic-review assessments. However, evaluation projects the input to
Role alone, row comparison currently uses sets, and recorded repository revision
does not prove which source produced a supplied binary. These distinctions matter
when accepting an automatically produced patch.

## Desired behavior and first scope

Add an explicit development command, tentatively `synth repair`, that takes a
reviewed evaluation, one case ID, and a committed baseline revision. One invocation
performs one bounded investigation, coding attempt, check stage, and review. Its
result is an accepted patch artifact, a rejected attempt, or a named inconclusive
outcome. No generated Epic is required between these stages.

The initial target remains the evaluator's supported ASCII Role text-equality
feature. Require a human-confirmed prompt/SQL pair and a reproducible wrong plan;
safe refusals and clarification are not automatically repair targets. Related
cases supply regression coverage, not permission to expand the feature. This
scope tests the complete loop before automatic batch scheduling.

This is development-time source editing, never LLM-generated execution inside
the baho product. Approval of this epic would authorize attempts within this
bounded feature; product-policy, schema, dependency, and architectural changes
return `needs_design`. Accepted means eligible for human integration, not merged.
The existing PRD epic workflow remains applicable to broader changes.

## Ownership and architecture

The harness, role prompts, probe tools, build isolation, and attempt records belong
to the excluded `tools/synth` package. Reuse Rig and the existing provider adapter;
do not change models as part of this experiment. Product crates gain no agent,
DuckDB, or runtime dependency. For the first repair scope, coder writes are confined
to `crates/baho-core/src/` and additive tests under `crates/baho-core/tests/`.
Repairs needing other ownership boundaries return `needs_design`.

Reuse the current intake, redaction, restricted SQL runner, and artifact readers
where their contracts apply. General synthetic CSV probing is a separate path
from private Role projection. Keep `plan-epic` available for larger design work;
do not rewrite its historical records or require completing all of Epic 015.

## Tools and evidence contract

| Tool | Inputs and result |
| --- | --- |
| `replay_case` | Case ID and harness-issued build ID; privately replays the frozen pair and returns sanitized plan, diagnostics, and oracle comparison. |
| `probe` | Bounded synthetic CSV, prompt, optional supported SQL, and build ID; returns candidate/body membership, typed plan when available, diagnostics, and exact synthetic source rows in order. |
| `run_checks` | Build ID and predefined suite ID; returns command identities, exit status, timeout, and bounded output. No agent-supplied shell. |
| Read/search | Existing bounded repository tools, extended to read the attempt's allowed files and diff. |
| `apply_patch` | Coder only; edits allowed files in its isolated checkout and invalidates the candidate build ID. |

Every observation names fixture/prompt hashes, build ID, and immutable result ID.
Row 0 is the header; data rows retain physical/logical source coordinates through
the full pipeline. An empty cell is not treated as an entirely blank record.
Probe results distinguish observation from asserted desired behavior.

Optional SQL remains within the existing restricted Role-equality grammar. A
multi-column synthetic CSV can be observed without SQL, or compared on Role when
supported; other SQL and plan semantics are explicitly unavailable. Do not infer
blank, numeric, or refusal expectations from an equality oracle. Existing
regression tests and observed baseline behavior cover those compatibility checks.
No unrestricted SQL, arbitrary code snippets, or parser-only surrogate execution.

Original inputs stay behind the trusted harness with the existing sanitization
rules. Only neutral synthetic fixtures expose exact row lists to agents. Bounds
cover fixture bytes/rows, process time, output bytes, model turns, and tool calls;
all are configured before the attempt and logged. Compiled candidate code runs
in a constrained worker without credentials, network, or the user's source tree.
Private dataset replays require the same confinement, with sanitized outputs.
If confinement is unavailable, stop rather than run candidate code with host access.

## Attempt lifecycle

1. **Freeze and reproduce.** Resolve the explicit revision and pin case, review,
   evaluation, and oracle hashes. Create a clean isolated checkout and build the
   CLI there. Record source-tree, toolchain, and binary hashes. Leave the user's
   current checkout, including dirty work, untouched. Recheck SQL, replay the
   target, and run baseline checks. Already-fixed cases stop as `no_change`;
   invalid inputs, baseline failures, and environment failures are inconclusive.

2. **Investigate.** Give the investigator source tools and executable baseline
   probes. Its short structured handoff contains the reproduced result IDs,
   causal hypothesis, one proposed mechanism, allowed files, and compatibility
   checks. It supplies a neutral reduced pair that reproduces the same mechanism.
   SQL computes synthetic expected rows; the supported SQL shape supplies expected
   predicate semantics. The agent cannot submit hand-written oracle row lists.
   An inability to preserve or establish intent returns `needs_design`.

3. **Freeze regression and code.** The harness freezes the reduced case and emits
   a deterministic regression assertion outside coder write access. Confirm it
   fails on the baseline because of the intended semantic mismatch, not build or
   fixture failure. The coder receives the contract and may iterate within its
   fixed budget, adding tests and changing allowed production files. Existing
   tests cannot be deleted or weakened; evaluator, SQL, case expectations,
   manifests, dependencies, build configuration, and harness files are protected.
   One submitted final patch ends the coding stage.

4. **Check.** Freeze the patch hash and build a fresh candidate from the pinned
   tree. Require the target and reduced fixture to match both oracle rows and
   supported plan semantics. Compare ordered source-row lists, including duplicate
   detection, rather than relying solely on existing set/count comparisons.
   Replay the fixed comparison batch: baseline-correct reviewed cases must remain
   correct; other nontarget cases must preserve their recorded behavior unless
   they also become oracle-correct. Run protected compatibility tests, additive
   tests, owning-crate tests, and workspace formatting/check/tests. Unsupported
   comparisons never silently count as success.

5. **Review.** A fresh reviewer receives the frozen contract, actual diff, and
   before/after evidence. It can probe both builds and request fixed checks, but
   cannot edit code or expectations. Review checks scope, NL/SQL meaning of the
   reduction, overfitting, preserved tests, and counterexamples. Additional desired
   behavior that lacks an established contract triggers `needs_design`; a model
   guess is not promoted into oracle truth. Blocking findings reject the patch.
   There is no automatic reviewer-to-coder revision cycle in this increment.

6. **Finalize.** Acceptance requires all deterministic gates and reviewer pass
   for the same patch hash. Retain the patch, frozen synthetic regression, and
   evidence as an integration bundle; synthetic promotion remains human-reviewed.
   Rejection discards the isolated working copy but preserves its patch and logs.
   Never reset or revert the user's checkout. Timeouts, crashes, missing tools,
   unavailable comparisons, and incomplete review cannot produce acceptance.

## Records and recovery

Reserve a unique attempt directory under gitignored `synthetic/out/repair/` before
work starts. Persist a schema-version-1 manifest and append-only structured events,
along with the contract, synthetic probes, patch, check reports, and review verdict.
Separate protected raw artifacts from sanitized model-visible transcripts.
Do not log credentials, environment dumps, private source contents, or source row
lists in public reports.

Record stage/outcome, input hashes, parent attempt ID when explicitly retried,
source and binary identities, model/prompt versions, tool results, budgets,
timings, usage, and reasons for rejection or inconclusive termination. Write the
terminal manifest atomically. An interrupted attempt is never accepted; a retry
gets a new ID and revalidates inputs rather than inheriting a verdict. New schemas
reject unknown versions; existing generation/evaluation/planner schemas remain
unchanged. Any necessary changes to those contracts need an explicit version and
compatibility decision during implementation.

## Alternatives and out of scope

- Adding probes to the Epic writer would improve evidence but retains a prose
  deliverable between reproduction and verification. Reuse probes there later.
- An unrestricted coding agent is easier to wire but can change its own acceptance
  target. The bounded patch/check interface keeps authority in the harness.
- Passing one example is insufficient: structured plan checks, baseline regressions,
  and independent counterexamples are required. They provide bounded evidence,
  not proof of correctness for every prompt.
- Automatic batch scheduling, parallel patches, experiment-branch advancement,
  main-branch integration, repeated repair cycles, model benchmarking, general SQL,
  new grammar features, and implementation of Epic 017 are out of scope.

## Acceptance criteria and test strategy

1. A synthetic full-pipeline probe distinguishes an empty Role cell on a populated
   row from a wholly blank record, reporting actual body membership and source rows.
2. A mock-agent successful attempt proves reproduction, baseline regression failure,
   candidate success, required checks, independent review, and exact-hash acceptance
   occur in order. No test makes a live provider call by default.
3. Same rows with a wrong plan, reordered/duplicated source rows, a regression in
   another protected case, or a stale build cannot pass. Unknown schemas and
   unsupported semantic comparisons produce named nonaccepting outcomes.
4. Tests reject attempts to alter oracle inputs, protected tests, manifests, denied
   files, or the harness, including symlink/path escapes. Candidate workers cannot
   access provider credentials or modify baseline/acceptance artifacts.
5. Reviewer rejection, model exhaustion, process timeout, compile failure, missing
   DuckDB, interrupted persistence, and cleanup failure leave attributable records
   and never alter the user's checkout. Cleanup failure is recorded separately
   from the patch verdict.
6. Accepted synthetic regressions replay through the public pipeline without a
   provider or DuckDB at ordinary test time. Oracle recomputation belongs to the
   development harness; frozen results and typed predicates belong to tests.
7. Before a live agent trial, a deterministic smoke test uses a tiny known patch
   and neutral fixtures to exercise real baseline/candidate builds and artifact
   comparisons. Then run one manually initiated live attempt and report semantic
   errors, acceptance/rejection, human corrections, cost, and elapsed time.

## Implementation outline

1. Implement and verify executable synthetic probes first, using the blank-record
   example as the initial regression. Inspect local dependency sources as required
   by AGENTS.md before relying on new dependency behavior.
2. Add versioned attempt contracts, protected regression emission, isolated builds,
   and fixed checks; prove the lifecycle with stub agents and failure injection.
3. Wire investigator, coder, and reviewer as separate Rig histories under the
   harness state machine. Run focused synth tests, its formatting/check/Clippy,
   and root formatting/check/tests, followed by the deterministic smoke test.
4. Conduct one explicitly initiated live trial. Use the resulting evidence to
   decide whether batch orchestration is justified.

## Unresolved questions

None for the proposed first milestone. Broader feature eligibility, automatic
integration, and batch scheduling require subsequent proposals.

## Live trial record (2026-10-09)

Task 4 was explicitly initiated after the implementation tasks. The required
confinement preflight, using `sandbox-exec` to run only `/usr/bin/true`, exited 71
with `sandbox-exec: sandbox_apply: Operation not permitted`. A retry requesting
execution outside the tool sandbox was interrupted before yielding an approval or
command result. No candidate build, private replay, or provider call was started.
The live trial is **inconclusive: worker unavailable**; it is neither accepted nor
rejected on semantic grounds. Semantic errors and human corrections were not
observable. Model cost was zero because no model was called. The preflight returned
immediately; the interrupted escalation waited approximately 1,605 seconds, which
was approval wait time rather than trial execution. Batch orchestration is not
justified by this trial. A future manual trial needs a functioning confined worker
and a new attempt ID; it must revalidate the frozen inputs.
