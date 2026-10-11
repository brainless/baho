# Synthetic evaluation and repair

`synth` is a separate developer package for the Role text-equality case. It
requires Rust 1.95 or newer and the installed DuckDB CLI. Its commands are
`generate`, `evaluate`, and `repair`. Repair is the agent workflow for changing
code; it produces an isolated candidate for human review and does not edit the
working tree or create an epic.

Run commands from `tools/synth` so its `.cargo/config.toml` selects LLVM. On
arm64, Cranelift cannot link the AWS LC native TLS dependency. When running
from the repository root, pass
`--config 'profile.dev.codegen-backend="llvm"'` to `cargo run`.

## Generate candidate cases

```sh
cargo run -- generate \
  --dataset ../../.datasets/<local-role-dataset>.csv \
  --feature features/role-equality.md --count 5 \
  --output ../../synthetic/out
```

Generation sends only a bounded `Role` profile to Neuralwatt, using the fixed
`deepseek-v4.1-flash-flex` model and streamed Chat Completions. Set
`NEURALWATT_API_KEY` in the process environment or repository root `.env`;
the process value wins. Only this key is read from `.env`, and it is never
written to records.

Optional arguments are `--output` (default `synthetic/out` relative to the
current directory), `--max-turns`, `--per-call-seconds`, `--whole-run-seconds`,
and `--variation`. The count must be 1–100. Defaults allow 900 seconds per
model call, 3600 seconds for the run, and `4 * count + 4` model turns.
When running from `tools/synth`, use the explicit `../../synthetic/out` path
shown above to place records under the repository's Git ignore rule.

Each invocation writes a fresh `generation-*` directory with `candidates.txt`
(case ID, prompt, SQL for manual review), `metadata.jsonl`, and
`generation.json`. Records contain hashes, settings, bounded tool activity,
rejections, elapsed times, and reported served tiers. Missing or standard
tiers do not cause a retry. Submitted cases survive model errors and timeouts.
The tool never emits tests or commits generated artifacts.

Review candidate prompts and SQL for semantic correctness. Expected rows come
from independently executed, grammar-validated SQL against the private
projected DuckDB database. Verify source row mapping against Baho before using
a new local dataset for a live run (Epic 014, Task 5).

## Evaluate a generation

```sh
cargo run -- evaluate \
  --dataset ../../.datasets/<local-role-dataset>.csv \
  --baho-bin ../../target/debug/baho \
  --generation ../../synthetic/out/generation-<id> \
  --review <review-file.json> --output ../../synthetic/out
```

Build `baho-cli` first if needed. The `--generation` and `--review` arguments
are optional for evaluation. Without `--generation`, the numerically greatest
`generation-*` in `--output` is selected. Evaluation creates a new
`evaluation-*` directory containing:

- `evaluation.json`: sanitized case report and the evaluation hash source;
- `redaction-sidecar.json`: private, owner-only mapping of profiled values to
  stand-ins, bound to the evaluation hash;
- `input.csv` and `.baho/runs/`: private derived input and raw Baho run
  artifacts.

Only sanitized evaluation data may reach an agent. Free text is checked for
profiled values, and the serialized report is checked for absolute paths. Keep
the sidecar, input, raw run artifacts, and any custom output directory outside
Git. If no case starts, evaluation writes its report and exits nonzero.

## Repair a reviewed case

```sh
cargo run -- repair \
  --evaluation ../../synthetic/out/evaluation-<id>/evaluation.json \
  --case <case-id> \
  --baseline <full-40-character-commit-hash> \
  --dataset ../../.datasets/<local-role-dataset>.csv \
  --review <review-file.json> \
  --output ../../synthetic/out/repair
```

All arguments except `--output` are required. The default output is
`synthetic/out/repair` relative to the current directory. Repair accepts a
human-confirmed wrong-plan case whose review, generation, dataset, and
redaction sidecar still match the evaluation. Intake validation happens before
credential loading or provider traffic. Repair uses Neuralwatt
`glm-5.3-flex`; the same `NEURALWATT_API_KEY` precedence applies.

Each attempt reserves a unique `repair-*` directory. It exports the pinned
commit to an isolated checkout, checks the baseline, investigates the failure,
freezes a small synthetic regression, applies bounded edits to the isolated
candidate, runs the target and protected comparisons plus fixed Cargo suites,
and asks a separate reviewer to inspect the candidate. Build scripts and Baho
processes run in the confined worker without credentials or network access.
The user's working tree is not modified. The generated regression and candidate
patch remain in the attempt's `protected/` directory for review; an accepted
attempt is still not integrated automatically.

The CLI prints the outcome and attempt path, then the manifest reason when
present. `manifest.json` records the terminal stage, outcome, build IDs, patch
hash, and a short reason. Outcomes include `accepted`, `rejected`, `no_change`,
`needs_design`, `regression_failure`, `build_failure`, `check_failure`,
`model_exhausted`, and `environment_failure`; consult the manifest for the
complete outcome vocabulary. `events.jsonl` records stage transitions.
`agent-events.jsonl` records bounded, payload-free model and tool activity,
including timing, token usage when returned, budget denials, safe request
metadata, and source spans exposed to the model. These logs exclude prompts,
answers, raw tool arguments and results, provider error bodies, and credentials.
Keep the complete attempt directory private and outside Git.

## Offline checks

From `tools/synth`:

```sh
cargo fmt --check
cargo check
cargo test
```

The HTTP tests use a localhost fixture; they do not make live provider calls.
