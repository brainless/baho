# Epic 016 implementation note: investigator proposal handoff

Date: 2026-10-09
Status: Implemented; generated Epic quality awaits a manual rerun.
Related epic: [016 — Epic Planner Agent](../epics/016-epic-planner-agent.md)

## Motivation

Review of the generated Epic 017 exposed unsupported planning claims and scope that could be made smaller. The first staged rerun, `plan-1791514926340583000-9731`, investigated and drafted but exhausted the shared tool-call budget during review, so no Epic was published. Investigation also grouped word equality, quote handling, and punctuation into one proposal despite their being independently useful changes.

The user requested separate investigator and writer agents: one investigates and hands over a concrete proposal; another turns it into a proper Epic, checking specific gaps with additional tools. The aim is a smaller, clearer plan whose behavior and compatibility requirements follow from evidence.

## Implemented workflow

1. **Investigator:** Reads the sanitized evaluation and relevant repository code/tests, selects one useful mechanism, and submits a complete structured proposal. It supplies the defect, desired behavior, design, compatibility obligations, acceptance criteria, fixture strategy, and uncertainties, alongside motivating cases, prompt traces, citations, competing interpretations, and deferred scope. It does not format the Epic.
2. **Writer:** Starts with a fresh conversation and the accepted proposal. It formats the required Epic and may make targeted repository reads to verify or correct claims. It may narrow scope and retain a nonempty subset of motivating cases; it cannot expand the motivating case set or change proposal kind. Case restrictions are validated mechanically; semantic scope restrictions are reviewed by the independent reviewer.
3. **Reviewer:** Starts with a fresh conversation and checks the exact draft against evidence and current repository tools. Blocking findings permit one writer revision followed by a fresh final review. The harness publishes only the exact draft that passed review.

These are separate agent instances with independent histories, using the same configured planner model. A published Epic remains `Status: Proposed` and requires human review before implementation.

## Agent budgets

| Agent invocation | Model turns | Tool calls |
|---|---:|---:|
| Investigation | 40 | 60 |
| Draft or revision | 25 | 40 |
| Review or final review | 20 | 30 |

Each invocation gets its own allowance while consuming the shared run budget: 120 model turns and 180 tool calls by default. Global time and tool-output byte limits still apply. Revision does not reset the shared budget.

`--max-turns` and `--max-tool-calls` control global caps. `--stage-max-turns` and `--stage-max-tool-calls` override the corresponding cap for every role. Local exhaustion is recorded as `stage_max_turns` or `stage_tool_budget`; global exhaustion retains `max_turns` or `tool_budget`. An unfinished review never publishes an Epic.

## Saved proposal and resume

Before writing begins, the harness atomically saves a validated, sanitized checkpoint beside the evaluation as `plan-<id>-proposal.json`, without replacing existing artifacts. The CLI displays its path. It contains the investigative proposal and provenance hashes, without raw source excerpts or private document contents. Generated checkpoints remain local artifacts outside Git.

After writer or review failure, repeat the original command with the same evaluation/context arguments and add:

```text
--proposal <displayed-proposal-path>
```

Resume skips investigator model calls after checking the evaluation hash, context hash, investigator prompt hash, full hashes of cited source files, and current typed investigation validation. Stale or unsafe proposals are rejected before model calls. A resumed run gets fresh writer/reviewer histories and new run budgets; earlier drafts and review verdicts are not reused.

Freshness checks cover identified inputs and cited files, rather than every repository file. Changes to uncited code or guidance can require a new investigation even when the checkpoint passes these checks. Independent review still checks the resulting draft with current repository tools.

## Files and schemas

The change belongs to the standalone `tools/synth` development tool:

- `src/investigation.rs` and `prompts/planner-investigation.md`: complete investigative proposal and validation.
- `src/draft_review.rs`, `prompts/planner-system.md`, and `prompts/planner-review.md`: writer narrowing and independent review.
- `src/planner.rs`: stage orchestration, budgets, checkpoint persistence, resume validation, and publication gating.
- `src/main.rs`: proposal resume and stage-budget CLI options.
- `src/planner_tools.rs`: planner prompt version `epic-planner-v11`.
- `README.md` and Epic 016: workflow and retry documentation.

Investigation schema is version 2, checkpoint envelope schema is version 1, and new planner records use version 3. Historical records remain unchanged.

## Verification and remaining evaluation

Implementation verification passed all 152 synth library tests, binary/doc tests, synth formatting and compile checks, Clippy with warnings denied, and binary build. Workspace formatting, compile checks, and tests also passed. Regression coverage includes checkpoint recovery after writer failure, resume without investigator calls, rejection of changed or unsafe evidence before model calls, writer case narrowing, per-agent/global budget enforcement, and publication only after independent review.

No live provider call was made for implementation verification. These tests establish workflow behavior, not improved Epic prose or design quality. The next manual run should assess causal accuracy, scope coherence, compatibility coverage, human corrections needed, and investigation/writing/review cost.
