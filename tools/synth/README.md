# Synthetic Role equality generator

This separate developer package requires Rust 1.95 or newer and the installed
DuckDB CLI. It sends only a bounded `Role` profile to Neuralwatt, using the fixed
`deepseek-v4.1-flash-flex` model and streamed Chat Completions. Provide
`NEURALWATT_API_KEY` in the process environment or in the repository root
`.env` (the environment variable takes precedence). Only that one key is read
from `.env`; it is never written to records.

From the repository root:

```sh
cargo run --manifest-path tools/synth/Cargo.toml \
  --config 'profile.dev.codegen-backend="llvm"' -- generate \
  --dataset .datasets/Kaggle_Small_Business_Financial_Dataset/gusto_payroll_bc.csv \
  --feature tools/synth/features/role-equality.md --count 5
```

From `tools/synth`, plain `cargo run` and `cargo test` use the tool-local
`.cargo/config.toml` to select LLVM. Cranelift fails to link the AWS LC native
TLS dependency on arm64. Cargo discovers configuration from the working
directory, so commands run from the repository root still need the explicit
LLVM override shown above. The Baho workspace configuration and minimum Rust
version remain unchanged. Running without arguments prints the required
`generate` command usage; dataset, feature, and count arguments are required.

Optional controls are `--max-turns`, `--per-call-seconds`,
`--whole-run-seconds`, `--variation`, and `--output`. Defaults allow 900 seconds
per model call, 3600 seconds for the run, and `4 * count + 4` model turns.
Accepted submissions are capped at the requested count (1–100).

Each invocation writes a fresh directory under gitignored `synthetic/out/`,
containing `candidates.txt` (case ID, prompt, SQL; for manual review),
`metadata.jsonl` (all other per-case fields, joined on `case_id`), and
`generation.json`. If a custom output directory
is supplied, keep it outside Git. Records include hashes, settings, bounded tool
dispatch/outcome metadata, submission rejection reasons, per-call elapsed time,
and the reported served tier. Missing and standard tiers are visible; neither
causes a retry. Model errors, turn exhaustion, and timeouts retain submitted
cases. The tool never emits tests or commits generated artifacts.

Candidates require human semantic review. The expected rows come exclusively
from independently executed, grammar-validated SQL against the private projected
DuckDB database. Verify source row mapping against Baho before using a new local
dataset for a live run, as specified in Epic 014 Task 5.

Offline verification (the HTTP test binds only a localhost fixture):

```sh
cargo fmt --manifest-path tools/synth/Cargo.toml --check
cargo check --manifest-path tools/synth/Cargo.toml
cargo test --manifest-path tools/synth/Cargo.toml \
  --config 'profile.dev.codegen-backend="llvm"' \
  --config 'profile.test.codegen-backend="llvm"'
```

Alternatively, run `cargo fmt --check`, `cargo check`, and `cargo test` from
`tools/synth` without backend overrides.

## Evaluating a generation (`synth evaluate`)

```text
synth evaluate --dataset CSV --baho-bin PATH [--generation DIR] [--review FILE] [--output DIR]
```

`--baho-bin` is the `baho-cli` package's binary, normally `target/debug/baho`
after `cargo build -p baho-cli`. Relative paths are accepted and canonicalized
once at probe time. A case whose process cannot be started records
`evidence.spawn_failure` (`spawn_not_found`, `spawn_permission_denied`, or
`spawn_failed`) and a report note; if no case could start, the command exits
nonzero after writing the report.

Without `--generation` the numerically greatest `generation-*` under `--output`
(default `synthetic/out`) is used. Each run creates `evaluation-*` under the
output root with:

- `evaluation.json`: the sanitized report; the only file a model may see. Its
  SHA-256 is the evaluation hash.
- `redaction-sidecar.json`: the private (owner-only, gitignored) mapping from
  profiled `Role` values to stand-ins, bound to the evaluation hash. Never
  expose it through agent tools.
- `input.csv` and `.baho/runs/`: the Role-only derived CSV and raw `baho-cli`
  artifacts. These hold source values and stay private.

Free-text fields (prompts, review rationale, diagnostic messages, refusal
reasons, plan columns and literals, notes, version strings) pass through
`sanitize_checked`; the serialized report is also scanned for absolute paths.
Case IDs, hashes, enum values, and diagnostic codes are tool vocabulary. Keep a
custom `--output` outside Git.

## Planner records (`synth plan-epic`)

The planner uses Neuralwatt’s `glm-5.3-flex` model on the Flex tier
(selected by model ID, without a `service_tier` override), streaming, and high reasoning effort by default. Override it with
`--reasoning-effort low|medium|high` for a controlled comparison; thinking remains
enabled. Neuralwatt maps both `medium` and `high` to this model’s high
level ([provider mapping](https://portal.neuralwatt.com/models/glm-5.3)).
`synth generate` continues to use
`deepseek-v4.1-flash-flex`. Each planner record identifies its model.

`plan-<id>.json` is written beside `evaluation.json`. New planner records use schema version 3 and include stage outcomes, stage prompt
hashes, and the configured reasoning effort. Historical version-1 and version-2 JSON records
remain available as written; they are not migrated. Other fields are sanitized
and bounded:

- Outcomes: `epic_written`, `no_epic` (only from an explicit `finish_no_epic`
  call; its sanitized explanation, at most 1000 bytes, is `explanation`),
  `malformed_tool_call` (the stream ended with tool-call markup written as
  text), `no_terminal_action` (plain text ended the stream with neither
  the current stage’s terminal submission nor `finish_no_epic`), `max_turns`, `timeout`,
  `provider_error`, `tool_budget`, `draft_scope_mismatch`, `publication_failed`,
  `stage_max_turns`, `stage_tool_budget`, and `review_failed` when the final semantic review still finds issues. Stage failures publish no epic. There is no automatic re-prompt after a malformed call.
- `final_text_excerpt` (only for `malformed_tool_call` and
  `no_terminal_action`): sanitized final text capped at 300 bytes.
- `provider_error` (only for outcome `provider_error`): `class`
  (`http_status`, `transport`, `decode`, `request`, `timeout`, `tool`,
  `other`), optional `http_status`, and `message`, scrubbed of literals, paths,
  and credential-shaped values and capped at 300 bytes. Headers and request
  payloads are never recorded.
- `stages[].terminal_rejections`: bounded, sanitized investigation/draft/review
  correction feedback; accepted investigation and semantic reviews are also
  retained, with exact draft hashes. Draft Markdown and source excerpts are not
  stored in the record.
- `calls[]` token usage: `input_tokens`, `output_tokens`, `total_tokens`, and
  `cached_input_tokens`, taken from the usage Rig 0.43.0 attaches to each finished
  model turn (`ModelTurnFinished.usage`). Each field is present only when the
  provider's stream reported it; nothing is estimated, and older records lack them.
- `rejected_submissions[].reasons`: the `code` and the sanitized `message`
  returned to the agent for each violation. A `forbidden` message names the
  violation kind and 1-based line numbers, never the matched text.

Draft submission and final publication scan prose for profiled values as whole words (plural `s`/`es`
included); a value that is merely a substring of an ordinary word is accepted.
Evaluation and tool surfaces keep the strict substring scan.

## Context packs (`plan-epic --context NAME`)

`tools/synth/context/<name>.json` (JSON: no new dependency) lists `entries`, each
`{path, anchor, lines_before, lines_after}` (anchor is a literal that must match
exactly one line) or `{path, start_line, end_line}`. The harness resolves them at
run time through the same reader as `read_file` (allowlist, denylist, symlink
recheck, redaction, original line numbers) and puts the result at the top of the
first user message, in manifest order, before the evaluation. An unresolved or
ambiguous anchor, a disallowed path, or a pack over 64 KiB fails before any model
call. The plan record gains `context_pack` (name, SHA-256, bytes, resolved line
ranges; never contents). Citations from the pack count like any other. Default is no
pack. Re-run `plan-epic` with a pack only after moving earlier drafts of the same
evaluation aside, or the agent may re-propose them.

## Investigation, drafting, and semantic review

`plan-epic` runs separate investigation, drafting, and independent semantic
review stages with the same fixed model. Each stage starts with a fresh
conversation history and receives the evidence and artifacts it needs from
preceding stages. The investigator and reviewer use the same bounded, sanitized,
read-only repository tools as the writer.

The investigator submits a version-2 structured proposal before drafting starts.
Alongside prompt traces, caller/test citations, competing interpretations, and
justified scope, it supplies the defect, desired behavior, design, compatibility
obligations, acceptance criteria, fixture strategy, and remaining uncertainties.
Investigation
is judged by supporting evidence rather than an arbitrary minimum number of
reads. Prepared context remains a starting point; it does not replace checking
claims against callers, binding rules, and compatibility tests.

The writer turns that proposal into the required epic format without publishing
it. It reuses the investigation and may make targeted repository reads to resolve
a gap or verify a claim. It can narrow scope and motivating IDs to a nonempty
subset, but cannot expand the investigated scope or change proposal kind. A
separate reviewer checks the exact draft for unsupported claims, scope expansion,
missing compatibility cases, and inconsistencies between design, fixture, and
acceptance criteria. If the review finds issues, the writer gets one revision
opportunity followed by a fresh final review. The harness publishes only the
exact draft that passed review. Unresolved findings or a failed stage leave
`epics/` untouched. Passing model review still produces a `Status: Proposed`,
human-unreviewed epic; it does not authorize implementation.

Planner defaults are 120 model turns and 180 tool calls for the complete run.
Override these with `--max-turns` (1–200) and `--max-tool-calls` (1–1000).
These controls and `--whole-run-seconds` apply across all stages, including
revision and final review. Global budgets do not restart at stage boundaries. Each stage also has a
per-invocation cap: investigation gets 40 model turns and 60 tool calls,
drafting/revision gets 25 turns and 40 calls, and review/final review gets 20
turns and 30 calls. Revision and final review each receive their stage cap while
remaining subject to the same global limit. These caps reserve room for later
stages rather than letting investigation spend the whole run budget. Byte
limits remain enforced separately. These planner defaults do not change the
case-generator's limits. `--stage-max-turns N` and
`--stage-max-tool-calls N` override the corresponding cap for every role;
the global caps still apply. `stage_max_turns` and `stage_tool_budget` identify
local stage exhaustion, separately from shared `max_turns` and `tool_budget`.
The CLI explains the relevant controls and offers the saved proposal path when
available.
Per-call timeout continues to bound individual model calls. Plan records identify
stages and their prompt hashes so investigation, writing, and review costs and
failures can be inspected separately.

A `tool_budget` outcome means the run exhausted a configured tool-call or byte
limit. The command reports which stage stopped and the budget reason. A draft
that has not completed independent review cannot be published, even when earlier
stages succeeded; this outcome alone does not show that the model's plan failed
semantic review. Use the recorded stage and global limits to diagnose the exhaustion rather
than treating an unfinished review as a verdict on epic quality.

## Saved investigation proposals and retries

After investigation is validated, the harness saves `plan-<id>-proposal.json`
beside the evaluation before starting the writer. The CLI displays this path.
The checkpoint contains the sanitized version-2 investigative proposal and its
provenance hashes, not raw source excerpts or private document contents. The
checkpoint envelope has its own schema version 1; new plan records use version 3.
Keep these local artifacts outside Git along with the evaluation.

If writing or review fails, rerun the same original CLI command with
`--proposal <displayed-path>` added. Keep its evaluation and context selection:

```text
synth plan-epic <same original arguments> --proposal <displayed-path>
```

This resumes from the saved proposal and skips the investigator's model calls.
Before writing, the harness checks the evaluation hash, resolved context hash,
investigator prompt hash, and full-file hashes for every file cited by the
investigation. It also applies the current typed investigation validation,
including citation, evidence-eligibility, and redaction checks. An invalid or stale
proposal is rejected; rerun without `--proposal` to investigate again. These
checks detect changes to the identified inputs and cited files; they do not
guarantee detection of every repository change.

The resumed writer and independent reviewer still have current repository tools
and fresh histories. They receive the validated saved proposal as data. A retry
starts a new run with its own recorded budgets; it does not reuse an incomplete
draft or a previous review verdict. Only a draft passing the new review may be
published.

`read_evaluation` summary derives per-case `evidence_eligibility` from the existing
version-1 report. Only human-confirmed prompt/SQL pairs with a different
structured materialized plan (`confirmed_wrong_plan`) or a refusal with supporting diagnostics
(`confirmed_refusal`) can motivate an implementation.
Missing plan/refusal evidence, unreviewed/ambiguous/rejected pairs, oracle/spawn
failures, and relevant stand-in mapping limitations remain `observation`.
An unrelated limitation does not disqualify the batch. Agreement on rows alone
cannot establish correct plan semantics. A confirmed refusal establishes observed
coverage, not a parser defect; the proposal must justify changing that coverage.
Clarification remains observation-only because it may be correct safety behavior.

`submit_draft` requires `evidence` with `schema_version: 1`, `kind`:
`implementation` or `investigation`, and 1–100 unique `motivating_case_ids`.
Every motivating ID must exist and appear in the epic. Every implementation
motivator must be eligible. The body declares the matching `Proposal kind:`.
The harness stamps the accepted evidence contract into the generated Markdown
after the draft passes semantic review.
This is a new required tool contract: old calls without evidence are rejected;
existing report/review/sidecar schemas and historical epics/records are unchanged.
No migration or retroactive acceptance is performed.

Rerunning an all-unreviewed evaluation legitimately produces an investigation
or `no_epic`. To propose implementation, review the original prompt/SQL pairs and
rerun `evaluate --review FILE`; oracle execution never upgrades semantic review.
Investigation criteria concern reproduction and review, not parser changes.

The near-grammar feature requests a fixed-value, fixed-opening `=`, `is`,
`equals` triplet, then separate quote/punctuation probes. This instruction guides
generation; it is not a deterministic guarantee that a model followed the grouping.
Inspect `candidates.txt` before using the batch. Do not combine failures from
several independent factors into one parser increment.

For a manual quality comparison, keep evaluation, code revision, context pack,
and model fixed; vary only the intended workflow or reasoning-effort setting.
Move generated drafts aside before each run. Record material planning errors,
unjustified scope additions, human edits needed before approval, cost, and elapsed
time. Check each draft against this rubric:

| Check | Pass condition |
| --- | --- |
| Evidence | Observations, reviewed intent, defects, and limitations are distinct; implementation motivators are eligible. |
| Scope | One independently reviewable mechanism; deferred ideas have no implementation contract. |
| Fixture | A self-contained tiny synthetic CSV and prompt/plan/source-row matrix agree; contrasting values expose polarity mistakes; SQL is recomputed on that fixture before promotion. |
| Compatibility | Ambiguities have cases at the new recognition position, including quoted syntax words and literal prefixes; aliases reuse an explicit literal/type policy with a distinguishing typed case. |
| Code | IR definitions, constructors, semantic tests, scope conditions, and shared-helper callers support the claims; each changed entry path has labeled success and relevant refusal/compatibility coverage. |
| Clarity | Required sections/goals remain, but facts and decisions are stated once; criteria are observable. |

Offline regression tests freeze synthetic evidence and check the gate (including
same rows with a different plan), version/ID/kind rejection, and atomic publication.
Mock-model tests additionally exercise investigation-before-drafting, independent
review, revision limits, shared budgets, and publication only after a passing
review. They establish workflow gating, not model prose quality. Formatting,
valid citations, token savings, and
accepted submissions cannot prove semantic correctness or improved epic quality;
the rubric and human correction count measure that separately. No automated
fixture promotion, implementation agent, or live quality benchmark is added.

Version 8 asks the planner to justify
every changed entry path before designing it, then reconcile binding rules,
literal-prefix compatibility, precedence, and typed refusals against one matrix.
The equality context pack includes longest-header binding, header enumeration,
and an existing compact ambiguity test. This is planning guidance; improved
output quality still requires reviewing a new draft against the same evaluation.

The whole-run timeout is shared by all model stages and checked before final
publication. Atomic publication can additionally wait up to ten seconds for the
repository lock; that wait makes no provider calls.

### Repair activity logs

Each `repair` attempt writes incremental, payload-free model and tool activity to
`agent-events.jsonl` in its attempt directory. Events record the stage, internal
call identifier, start/end timing, token usage when returned, tool success, and
budget denials. A start event survives a session timeout; the terminal events
identify interrupted calls and the precise timeout, turn limit, output limit,
provider error category, or JSON protocol failure. The 120-second default remains
a deadline for the entire agent stage, including tools.

The terminal manifest's `reason` and CLI output include the failing stage and
cause. Tool-budget denials are recorded separately even if the session later ends
on another limit. No prompts, model answers, tool arguments/results, provider
error bodies, or credentials are stored in this activity log. Each stage retains
up to 2,000 activity events plus its terminal summary, which reports omitted
event counts. This new sidecar uses its own schema version 1; existing attempt
manifest and contract schemas remain unchanged and historical records are not
rewritten. Transport retries inside a provider call are not separate Rig calls.
