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

`plan-<id>.json` is written beside `evaluation.json`. Optional fields, all
sanitized and bounded (schema version unchanged; older records lack them):

- Outcomes: `epic_written`, `no_epic` (only from an explicit `finish_no_epic`
  call; its sanitized explanation, at most 1000 bytes, is `explanation`),
  `malformed_tool_call` (the stream ended with tool-call markup written as
  text), `no_terminal_action` (plain text ended the stream with neither
  `submit_epic` nor `finish_no_epic`), `max_turns`, `timeout`,
  `provider_error`. There is no automatic re-prompt after a malformed call.
- `final_text_excerpt` (only for `malformed_tool_call` and
  `no_terminal_action`): sanitized final text capped at 300 bytes.
- `provider_error` (only for outcome `provider_error`): `class`
  (`http_status`, `transport`, `decode`, `request`, `timeout`, `tool`,
  `other`), optional `http_status`, and `message`, scrubbed of literals, paths,
  and credential-shaped values and capped at 300 bytes. Headers and request
  payloads are never recorded.
- `calls[]` token usage: `input_tokens`, `output_tokens`, `total_tokens`, and
  `cached_input_tokens`, taken from the usage Rig 0.43.0 attaches to each finished
  model turn (`ModelTurnFinished.usage`). Each field is present only when the
  provider's stream reported it; nothing is estimated, and older records lack them.
- `rejected_submissions[].reasons`: the `code` and the sanitized `message`
  returned to the agent for each violation. A `forbidden` message names the
  violation kind and 1-based line numbers, never the matched text.

`submit_epic` scans prose for profiled values as whole words (plural `s`/`es`
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
