# Epic 006: Explicit-Column Boolean Filters

Status: Implemented (decisions locked 2026-09-22; all tasks complete 2026-09-23)

Motivating run: None; this is the first implementation slice split from Epic
003.

User intent: `List rows where Job = unemployed or Annual Income < 10000`

Compact motivating form: `List job unemployed or annual income < 10000`

## Review gate

This epic proposes a constrained grammar, plan evolution, and execution
semantics. Do not implement it until the grammar, literal parsing, and unknown
value behavior have been reviewed and its status has been updated.

Review gate cleared 2026-09-22. Locked decisions are recorded in
[Locked decisions](#locked-decisions).

## Summary

Add deterministic filtering when every atomic condition names a source column
explicitly. Baho recognizes exact complete header phrases, parses a small set
of comparison and Boolean operators, compiles the result into a closed typed
predicate IR, validates it against the selected table, and evaluates it without
LLM calls or generated code.

This epic deliberately does not infer columns from observed values or semantic
terms. A clause such as `unemployed` without an explicit `Job` column is outside
scope. Epic 007 may ground such omitted references or request clarification
after this predicate foundation exists.

## Current behavior

The current recognizer accepts an initial retrieval action, an optional
distinct modifier, and one exact or terminal-`s` column phrase. It compiles to
select-only or filter-is-not-blank/select/distinct plans. It cannot represent:

- a row-filtering request that retains all columns;
- equality or ordered comparison;
- more than one referenced column;
- recursive `and`, `or`, or `not`; or
- typed literals.

The plan IR contains only `is_not_blank`. The executor assumes a filter
predicate has one column reference, and the current CSV orchestration presents
nonblank cells to execution as text. Numeric comparison therefore requires a
reviewed typed parsing boundary rather than ad hoc conversion inside a Boolean
branch.

## Desired behavior

The canonical, recommended request form is:

```text
List rows where Job = unemployed or Annual Income < 10000
```

It means:

```text
retain all source columns
where
    Job equals the text "unemployed"
    or Annual Income is numerically less than 10000
```

The compact form may also be recognized when it has exactly one complete parse:

```text
List job unemployed or annual income < 10000
```

Here `Job unemployed` is syntactic sugar for `Job = "unemployed"`. It is not
value-based column discovery: `Job` must first match one complete header.

Existing retrieval prompts such as `List income` retain their Epic 002 meaning
and project that column. `List rows where ...` is a distinct row-filter action
whose default output is every source column in source order.

## Constrained grammar

A first grammar should be equivalent to:

```text
request          := row_action predicate
row_action       := ("list" | "show" | "filter") ["rows"] ["where"]
predicate        := or_expression
or_expression    := and_expression ("or" and_expression)*
and_expression   := unary_expression
                    (("and" | "but") unary_expression)*
unary_expression := ["not"] atomic_condition
                    | "(" predicate ")"
atomic_condition := exact_column comparison_operator literal
                    | exact_column text_literal
comparison_operator := "=" | "!=" | "<" | "<=" | ">" | ">="
```

Operator precedence is fixed and recorded:

```text
parentheses > not > and / but / but not > or
```

`but` is an alias for `and`, so the ordinary unary `not` rule parses `A but not
B` exactly as `A and (not B)`; it does not introduce a separate grammar branch
or plan operation. Tokens assigned to actions, headers,
operators, connectors, and literals must have non-overlapping spans.

Quoted text is recommended whenever a literal contains a connector, operator,
parenthesis, or words that could extend a header. Unquoted implicit-equality
text ends at the next unquoted Boolean connector or the end of its parenthesized
clause. Recognition refuses when more than one complete parse survives.

## Exact column binding

This epic supports only complete normalized header matches. It does not use
observed cell values, fuzzy spelling, partial-token similarity, or semantic
aliases to select a column.

Matching uses longest complete header first. Given columns `Income` and `Annual
Income`, the phrase `Annual Income < 10000` binds to `Annual Income`. If two
columns share the same normalized complete header, recognition refuses with a
column-ambiguity diagnostic. A shorter header must not be accepted by leaving
otherwise meaningful tokens unconsumed.

The existing conservative header normalization contract may be reused, but
every accepted parse must retain the raw prompt span, normalized tokens, stable
column ID, and display name as evidence.

## Typed literals and source values

The first implementation supports text and exact decimal literals:

- `=` and `!=` accept text or decimal literals compatible with the bound
  column;
- `<`, `<=`, `>`, and `>=` require a decimal literal and a numeric column;
- decimals use an exact decimal representation, not `f64`; and
- the initial numeric grammar accepts an optional sign, ASCII digits, and an
  optional `.` fractional part.

Thousands separators, exponent notation, dates, currencies, percentages,
locale guessing, and unit conversion are out of scope. Thus `10000` is valid in
the initial grammar while `10,000` is refused with a literal diagnostic until a
separate grouping rule is reviewed.

Typed comparison requires a schema-parsing stage before execution. It must:

- preserve every raw cell and its source coordinate;
- record the parser policy used for each compared column;
- produce exact decimal interpreted values only on successful parsing;
- distinguish blank, missing, malformed, and valid values; and
- emit stable diagnostics for malformed input without coercing it to null,
  zero, or text.

Type inference may eliminate an incompatible plan, but it must not manufacture
a type merely because the requested operator needs one. Materially mixed or
weakly inferred compared columns cause refusal in this first version.

## Boolean and unknown semantics

Predicate evaluation has three results: `true`, `false`, and `unknown`.
Missing, blank, malformed, and type-incompatible cells evaluate to `unknown`
for comparisons. A filter retains only rows whose complete predicate evaluates
to `true`.

Negation and composition use deterministic three-valued logic:

| Expression | Result |
|---|---|
| `not unknown` | `unknown` |
| `false and unknown` | `false` |
| `true and unknown` | `unknown` |
| `true or unknown` | `true` |
| `false or unknown` | `unknown` |

Short-circuiting may optimize evaluation but must not change the result or the
diagnostic contract. Parsing diagnostics are produced before Boolean execution,
so evaluation order does not determine whether malformed source evidence is
reported. Diagnostics should be bounded and deterministic rather than emitting
unbounded copies of the same column-level problem.

## Plan IR and compatibility

This feature requires plan schema version 2. Version 1 remains readable and
executable with its existing semantics and expression set. Version 2 adds
typed literals and recursive predicates conceptually equivalent to:

```text
Predicate =
    IsNotBlank(column)
    | Compare(column, operator, literal)
    | And(predicates)
    | Or(predicates)
    | Not(predicate)

Literal = Text(string) | Decimal(exact_decimal)
```

The persisted shape must be reviewed as Rust types before implementation.
Version 2 validation must recursively collect and validate every column
reference, enforce nonempty Boolean operands, enforce a bounded expression
depth and node count, and type-check every comparison. Extending the Rust enum
must not cause new expressions to be emitted under schema version 1.

Any change to the outer `plan.json` recognition-evidence envelope receives its
own artifact schema version. Historical version 1 plans and older run artifacts
are never rewritten or reinterpreted.

## Recognition evidence and diagnostics

Evidence should record, in bounded deterministic form:

- normalized prompt tokens and original token indices;
- action, header, operator, connector, literal, and parenthesis spans;
- each bound stable column ID and display name;
- literal kind and parser policy, without replacing the raw prompt text;
- the unambiguous predicate tree; and
- the plan schema version emitted.

Initial diagnostic categories should distinguish:

- unsupported predicate grammar;
- missing or ambiguous explicit column;
- ambiguous complete parse;
- invalid or unsupported literal;
- incompatible column and literal types;
- excessive expression depth or size; and
- malformed or materially mixed compared-column values.

Exact stable code names should be finalized with the implementation design and
tested as public behavior.

## Architecture and ownership

- `baho-model` owns exact decimal values, parsed/raw value separation, source
  coordinates, and any shared column type metadata.
- `baho-ingest-csv` owns strict CSV value profiling and parsing evidence, not
  prompt recognition or Boolean operations.
- `baho-plan` owns plan schema version 2, typed literals, recursive predicate
  types, structural validation, and serializable recognition evidence.
- `baho-exec` owns typed validation and deterministic three-valued predicate
  evaluation while preserving row order and provenance.
- `baho-core` owns the constrained grammar, exact header binding, canonical
  row-filter intent, and plan compilation.
- `baho-cli` and `baho-gui` present results and diagnostics but do not duplicate
  recognition or execution rules.
- `baho-llm` remains outside this path.

## Out of scope

- Inferring an omitted column from a cell value.
- Asking the user to resolve a missing or ambiguous column.
- Semantic roles such as mapping `earning` to income.
- Fuzzy text matching, spell correction, stemming, or embeddings.
- Date, time, currency, percentage, or unit-aware comparison.
- General expression operands, arithmetic, functions, SQL, or generated code.
- Joins, aggregates, grouping, and cross-table predicates.
- Automatic correction of a literal such as `unemplyed` to `unemployed`.

## Alternatives considered

### Implement grounding and Boolean execution together

Rejected for this slice. It couples syntax, table search, ambiguity policy,
interactive clarification, typed comparison, and execution in one change.
Explicit headers provide a useful filter while establishing the plan and
executor contract needed by later grounding.

### Use two-valued Boolean logic

Rejected. Treating an absent or malformed value as ordinary false makes its
negation true and can include rows for which the source provides no supporting
evidence.

### Parse numbers on demand inside the executor

Rejected. Parsing policy must be observable and reusable by validation and
grounding while raw input and diagnostics remain preserved.

## Fixture and test strategy

Use small synthetic CSV fixtures covering:

- `Job = unemployed or Annual Income < 10000`;
- the compact implicit-equality form;
- longest-header binding with `Income` and `Annual Income`;
- duplicate normalized headers causing refusal;
- `not`, `and`, `or`, `but`, and `but not` precedence;
- parenthesized predicates;
- blank, missing, malformed, and mixed numeric cells;
- decimal boundary equality and negative values;
- invalid grouped or locale-sensitive numeric literals;
- expression depth and node-count limits;
- preservation of all source columns, row order, raw evidence, and provenance;
  and
- unchanged Epic 001 and Epic 002 version 1 behavior.

Tests should be layered across grammar parsing, header binding, value parsing,
plan serialization and validation, execution, orchestration, and run artifacts.

## Acceptance criteria

1. An explicit-column Boolean request is accepted only when every atomic
   condition binds to exactly one complete source header.
2. Longest complete header matching is deterministic and evidenced.
3. `not`, `and`, `but`, `but not`, and `or` follow the documented precedence,
   with parentheses available for explicit grouping.
4. Numeric comparison uses exact decimals and recorded parsing rules.
5. Blank, missing, malformed, and incompatible comparisons evaluate to
   `unknown`; only a final `true` retains a row.
6. A predicate-only row request retains all source columns in source order.
7. Plan schema version 2 is emitted for the new predicates while version 1
   plans retain their existing behavior.
8. Recognition evidence and diagnostics are bounded, deterministic, and use
   stable source coordinates where applicable.
9. No semantic role, observed-value grounding, LLM, or generated code affects
   recognition or execution.
10. Synthetic regressions cover the behavior and the workspace baseline passes:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

## Implementation outline

Do not begin until the review gate is cleared.

1. Finalize grammar examples, normalization, literal syntax, and diagnostic
   codes.
2. Add exact-decimal and parsed-column contracts while retaining raw evidence.
3. Add plan schema version 2 predicate and literal types with recursive limits
   and compatibility tests.
4. Write failing parser and exact-header-binding tests in `baho-core`.
5. Implement strict typed parsing and diagnostics in the owning ingest/model
   boundaries.
6. Implement recursive reference/type validation and three-valued execution.
7. Compile canonical row-filter intents into version 2 plans.
8. Version and persist recognition evidence and plan artifacts as required.
9. Add orchestration and CLI/GUI regressions without adding grounding or
   clarification.
10. Run focused tests followed by the workspace baseline.

## Locked decisions

1. **Resolved:** Both prompt forms ship in this slice. The canonical `=` form
   and the compact implicit-equality form are recognized; the compact form
   requires exactly one complete parse to survive.
2. **Resolved:** Predicate-presence rule. `rows` and `where` remain optional
   keywords. A request is a row filter if and only if a predicate follows the
   action. A request with no predicate (`List income`) retains its Epic 002
   projection meaning.
3. **Resolved:** Version 2 reuses the existing conservative header
   normalization contract (`trim`, lowercase, collapse whitespace runs) as in
   `normalize_for_match` in `crates/baho-core/src/intent.rs`.
4. **Resolved:** An internal `ExactDecimal` type in `baho-model` (i128
   mantissa plus u32 scale, canonical string serialization) provides exact
   decimal values. No new dependency is introduced.
5. **Resolved:** A compared column is refused with `parse.column_mixed` when
   zero nonblank cells parse successfully or when the malformed share of
   nonblank cells exceeds 10%. Malformed cells below that threshold evaluate
   to `unknown`.
6. **Resolved:** Malformed values produce one bounded diagnostic per column
   per failure kind (`parse.value_malformed`) carrying a total count and at
   most 3 sample cell locations.
7. **Resolved:** Maximum predicate depth is 16 and maximum predicate node
   count is 64.

Diagnostic codes finalized as public behavior:

| Code | Meaning |
|---|---|
| `intent.predicate_unsupported` | unsupported predicate grammar |
| `intent.column_not_found` | missing explicit column |
| `intent.column_ambiguous` | duplicate normalized header |
| `intent.parse_ambiguous` | ambiguous complete parse |
| `intent.literal_invalid` | invalid or unsupported literal |
| `plan.type_mismatch` | incompatible column and literal types |
| `plan.expression_limit_exceeded` | excessive expression depth or size |
| `parse.value_malformed` | malformed compared-column values |
| `parse.column_mixed` | materially mixed compared-column values |

Plan IR schema version is 2. The `plan.json` run-artifact envelope version
moves from 3 to 4 with the recognition-evidence changes.

## Tasks

### Task 1: Exact decimal and parsed-value contracts

Status: complete

Add `ExactDecimal` and parsed/raw value separation contracts to `baho-model`
per Locked decisions 4–5.

### Task 2: Plan schema version 2 predicates and literals

Status: complete

Add recursive `Predicate` and `Literal` types, recursive structural and
reference validation with Locked decision 7 limits, v1 compatibility,
recognition-evidence extensions, and the `plan.json` envelope bump to 4 in
`baho-plan` and `baho-run`.

### Task 3: Constrained grammar and exact header binding

Status: complete

Implement the tokenizer, longest-complete-header binding, canonical
row-filter intent, refusal diagnostics, and compilation to version 2 plans in
`baho-core`, preserving Epic 002 behavior.

### Task 4: Strict typed parsing

Status: complete

Implement strict decimal parsing with blank/missing/malformed/valid
distinction, the Locked decision 5 mixed-column threshold, and bounded parse
diagnostics in `baho-ingest-csv`.

### Task 5: Three-valued execution

Status: complete

Implement deterministic three-valued predicate evaluation, true-only row
retention, typed validation, and provenance/order preservation in
`baho-exec`.

### Task 6: Orchestration and pipeline regressions

Status: complete

Wire typed parsing of compared columns into `baho-core` orchestration and add
synthetic CSV regressions per the fixture strategy.

### Task 7: CLI/GUI and run artifacts

Status: complete

Add CLI end-to-end and run-artifact regressions asserting schema versions and
diagnostic codes, and keep the GUI a pass-through of core rules.
