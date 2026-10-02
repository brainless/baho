# Epic 012: Composable Header-First Queries

Status: Implemented

Motivating run: None supplied. This design follows the user's query examples and a review of the current recognizer, grounding path, and Epics 002, 006, and 007.

User intent: Make `Show Delhi`, `Show Income`, `value_x and value_y`, and mixed `and`/`or` value queries work with a small deterministic parser. A bare header used as a condition means `is not blank`, including when its values are Boolean shaped.

## Observed behavior and problem

`baho-core::intent` recognizes retrieval through an action word followed by a column name. It also recognizes row filters and builds a Boolean expression tree. `baho-plan` and `baho-exec` already support comparisons, `is_not_blank`, `and`, `or`, and `not`.

Current recognition keeps `List Income` as retrieval, but `List unemployed` is refused when `unemployed` is only a cell value. A standalone value can be a filter. Compact filters with bare values connected only by `and` or `or` are restricted, although `List rows where unemployed or employed` can reach value grounding. A bare header in a filter is classified as either `flag_is_true` for a Boolean shaped column or `is_not_blank` for another column. Grounding currently offers header and value interpretations together, so a phrase that is both a header and a value can require clarification.

The user wants a predictable rule: a complete header match wins over a value match. If no header matches, a bare phrase may be grounded by observed values. Explicit `City = Delhi` continues to mean a comparison on `City` even if `Delhi` is also a header.

## Desired behavior

Interpret the query in two stages: recognize the syntax and phrase boundaries, then bind each phrase to the selected table. An initial `show`, `list`, `get`, or other supported retrieval action is optional for filters. A connector or comparison operator is enough to establish filter syntax. A single bare phrase is also eligible for filter interpretation.

| Query shape | Proposed interpretation |
| --- | --- |
| `Show Income` where `Income` is a header | Retrieve the `Income` column. |
| `Show Delhi` where `Delhi` is a header and also a value under `City` | Retrieve the `Delhi` column. |
| `Show Jaipur` where no header matches and `Jaipur` occurs in one column | Filter rows by equality in that column. |
| `Income` or `Delhi` where the phrase is a header | Filter rows where that column is not blank. |
| `City = Delhi` | Filter rows where `City` equals the literal `Delhi`. |
| `value_x and value_y` | Filter rows satisfying both grounded value comparisons. |
| `value_x and value_y_1 or value_z_2` | Filter using `(value_x and value_y_1) or value_z_2`. |
| `value_x and value_y or value_z` | Apply the same precedence even when multiple values ground to the same column. |

For a bare header in predicate position, always compile `IsNotBlank(column)`. This includes columns containing only `true`, `false`, and configured blank values: both `true` and `false` are nonblank and therefore match. The explicit comparison `Delhi = true` remains available for Boolean filtering. This change does not redefine blank or null normalization.

Header priority applies to **complete** header phrases at condition boundaries. Partial header matches must not consume words that belong to a value or another condition. An explicit column comparison always binds its left side as a header and its right side as a literal; header priority does not reinterpret that literal. When no header matches a bare condition, search observed values under the existing text-match policy. Exactly one matching column grounds automatically; multiple matching columns require structured clarification; no matching column produces a stable refusal.

Keep current connector precedence: `not` binds most tightly, `and` and `but` next, and `or` last. `but not` means `and not`. Parentheses override precedence. Quoted connector words remain literal text. Complete header names containing connector words must remain selectable or, when syntax truly admits two complete parses, produce a visible parse ambiguity rather than a silent split.

## Scope and ownership

- `baho-core::intent` owns syntax recognition, complete header matching, interpretation precedence, and evidence for the chosen reading.
- `baho-core::grounding` owns source-bound value lookup and replacing bare clauses with column-bound predicates. Header priority should prevent value candidates from competing once a complete header has bound a bare clause.
- `baho-plan` continues to own the versioned expression tree and structural validation. No new expression kind is expected.
- `baho-exec` continues deterministic execution. Its `IsNotBlank` behavior should be reused rather than introducing Boolean coercion.
- CLI and GUI should present the selected plan or equivalent readable interpretation and preserve existing clarification flows. Run artifacts should show whether a phrase matched a header or an observed value and which rule chose it.

This epic does not add semantic synonyms, fuzzy column matching, LLM planning, new operators, or changes to CSV ingestion and blank normalization. It does not infer a column from the first matching row or silently choose among multiple value columns.

## Design and tradeoffs

1. Parse the whole query into a small syntax tree with atomic phrase spans, comparison operators, and Boolean connectors. Require complete consumption of the prompt. Keep the current limits on predicate depth and node count.
2. Use context to distinguish retrieval from filtering. An action plus a complete header phrase without predicate syntax retrieves that column. A bare header phrase without an action is a filter condition. An action plus a phrase with no complete header match may use value grounding as a filter. Explicit `rows where` remains a filter lead-in.
3. At every bare filter atom, try a complete header match first. If it matches one header, emit `IsNotBlank`; if duplicate headers match, retain the column ambiguity refusal. Only when no header matches should the phrase enter observed-value grounding.
4. Ground all unbound value atoms against the complete selected table using the existing bounded scan and text-match policy. Preserve a separate clause ID and decision for each atom, even when several atoms resolve to one column. Do not compile or execute a partial predicate while clarification is pending.
5. Retain source spans and structured evidence for the retrieval/filter decision, header priority, value match counts, connector precedence, and any competing complete parses. Extend artifact schemas only if required, with a version change and compatibility coverage.

Header priority is a product rule, not a statistical confidence score. It sacrifices the ability to use a header-shaped value as a bare filter term; `City = Delhi` gives users an explicit path to that meaning. This avoids presenting a choice on every header/value collision. For `Show <phrase>`, trying a header before a value likewise preserves familiar column retrieval.

An alternative is to clarify every header/value collision, as the current grounding path does. That preserves both readings but conflicts with the desired predictable header-first rule. Another alternative is to require `rows where` for all value chains. That is easier to parse but leaves the motivating queries unsupported.

## Acceptance criteria

- `Show Income` retrieves the `Income` column, and `Show Delhi` retrieves a `Delhi` column even when `Delhi` also occurs under `City`.
- A bare `Delhi` header used as a filter compiles to `IsNotBlank(Delhi)` for both Boolean shaped and non-Boolean columns; `false` is retained and configured blanks are excluded.
- `City = Delhi` compares `City` with literal `Delhi`, regardless of any `Delhi` header.
- A single value with no header match works with and without `Show` when it uniquely identifies a column.
- Two-value and three-value `and`/`or` chains work without `rows where`; tests cover values in distinct columns and multiple values in the same column. `not`, `but not`, and parentheses preserve their documented precedence.
- A value found in multiple columns produces a bounded clarification; a value found nowhere produces a stable refusal. A header/value collision follows header priority without clarification.
- Complete header names containing connector words, quoted connector literals, duplicate headers, and competing complete syntax parses remain deterministic and observable.
- The generated plan validates, retains source provenance, and yields the expected rows. CLI and GUI show the chosen interpretation clearly enough for a user to notice a header-first decision.

## Fixture and verification strategy

Use small synthetic CSV fixtures with neutral headers and values: one Boolean shaped header whose name also occurs under another column, one ordinary header, values unique to one column, values repeated across columns, and values repeated within one column. Do not use or copy a user document. Add failing recognition and end-to-end core tests before implementation. Assert plan shape, retained rows, clause evidence, clarification choices, diagnostic codes, and source coordinates where relevant. Run focused tests, owning-crate tests, then `cargo fmt --check`, `cargo check --workspace`, and `cargo test --workspace`. Compare structured run artifacts from a synthetic CLI run.

## Implementation outline

1. Lock the retrieval versus filter decision and the header-first rule in tests.
2. Update compact syntax recognition so connector-only bare-value chains can form a complete filter tree, while preserving retrieval of an action plus a complete header.
3. Change bare-header grounding to `IsNotBlank` and skip observed-value alternatives after a complete header match.
4. Carry the chosen interpretation through evidence and presentation, updating persisted versions if their shapes change.
5. Verify ambiguous and malformed cases, then run the workspace checks.

## Open question

For an action plus a phrase that is **both** a complete header and part of a longer valid filter reading (for example, a header literally named `A and B`), should the complete retrieval always win, or should Baho report a parse ambiguity? This epic proposes ambiguity when both readings consume the whole prompt; review should lock that boundary before implementation.

Implementation resolution: complete retrieval wins when connector words are part of one complete header phrase and no distinct complete filter parse survives. When distinct complete parses do survive, recognition reports `intent.parse_ambiguous` with competing parse evidence.
