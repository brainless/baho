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
