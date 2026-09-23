# Epic 007: Value Grounding and Structured Clarification

Status: Proposed — depends on approved and implemented Epic 006

Motivating run: None; this is the second implementation slice split from Epic
003.

User intent: `List rows where unemployed or < 10000`

## Review gate

Do not implement this epic before Epic 006 establishes typed Boolean predicates,
explicit-column execution, and plan schema version 2. The value-search,
normalization, clarification lifecycle, and CLI behavior proposed here require
separate review.

## Summary

Allow a Boolean filter clause to omit its column when table evidence can either
resolve the omission safely or present a bounded choice to the user. Exact
categorical values are searched across the complete selected table. A value
that occurs in exactly one column may ground to that column; a value that occurs
in several columns produces a structured clarification request. Comparisons
without a column also produce clarification rather than semantic guessing.

The completed intent compiles into the same typed predicate IR introduced by
Epic 006. This epic does not add new executor semantics and does not infer that
words such as `earning` mean income. User confirmation, not confidence scoring,
resolves evidence that is insufficient.

## Dependency on Epic 006

Epic 006 must already provide:

- explicit-column equality and ordered comparison;
- recursive `not`, `and`, and `or` predicates;
- exact decimal literals and three-valued execution;
- stable column IDs and exact-header matching;
- predicate-only output retaining all source columns; and
- versioned plan and recognition evidence.

Epic 007 operates before plan compilation. Once every clause has one bound
column, compilation, validation, and execution follow the Epic 006 path.

## Clause categories

Grounding distinguishes syntax from evidence.

### Implicit categorical equality

An unbound categorical term such as:

```text
unemployed
```

is represented first as:

```text
ImplicitEquality(Text("unemployed"))
```

Baho searches for an exact normalized observed value in every column of the
selected table.

- If it occurs in exactly one column, that stable column ID is proposed as the
  grounding.
- If it occurs in multiple columns, the user must choose among those columns.
- If it occurs nowhere, Baho may ask the user to choose from compatible columns
  or revise the value; it must not claim that the value was observed.

### Comparison with an omitted column

A comparison such as:

```text
< 10000
```

cannot be grounded by searching for the exact value `10000`; comparison targets
need not contain the threshold. Baho requests a choice among columns compatible
with the literal and operator under Epic 006's parsed schema.

A descriptive word such as `earning` in `earning < 10000` may be retained as
prompt evidence and displayed to the user, but it does not automatically select
an income column in this epic.

## Complete-table evidence

Unique occurrence is a claim about the complete selected table, not a bounded
sample. Baho must use one of these equivalent safe strategies:

- build a complete stream-oriented exact-value index with bounded memory and an
  explicit overflow/refusal policy; or
- use bounded profiling to propose candidate columns, then perform a targeted
  complete-table verification scan before accepting uniqueness.

Sampling may eliminate a candidate or cause refusal. It must never establish
that a value occurs in only one column. Search must operate on the immutable
source revision and selected table to which the resulting request is bound.

The implementation must bound retained samples and candidate details. It may
record counts, stable column IDs, and bounded source locations, but it must not
persist an unbounded value index or dump source contents into run artifacts.

## Text normalization

The first version uses one small, recorded normalization policy for value
lookup. A conservative starting contract is:

- apply the parser-configured outer-whitespace treatment;
- otherwise require exact Unicode text and case;
- do not stem, spell-correct, case-fold, decode abbreviations, or equate blanks
  with domain values; and
- retain the raw prompt literal and raw source cell as authoritative evidence.

Thus `unemplyed` is not silently corrected to `unemployed`. Optional weaker
match classes require a later reviewed policy and may suggest a clarification,
but must not silently ground a clause.

## Grounding outcomes

Grounding returns one of three core outcomes:

```text
Grounded
NeedsClarification
Refused
```

`Grounded` contains a complete canonical predicate whose clauses all reference
stable column IDs. `NeedsClarification` contains one or more unresolved clauses
and bounded choices. `Refused` indicates unsupported grammar, invalid literals,
incompatible types, stale responses, exhausted resource bounds, or another
condition that cannot be resolved by choosing a presented column.

Failure to ground any clause prevents plan compilation and materialization. Baho
never executes only the clauses it happened to understand.

## Structured clarification contract

Clarification is a core data contract, not terminal or GUI behavior. A request
is conceptually equivalent to:

```text
ClarificationRequest {
    schema_version,
    request_id,
    source_revision,
    table_id,
    prompt_identity,
    unresolved: [
        ChooseColumn {
            clause_id,
            rendered_condition,
            reason,
            candidates: [
                { column_id, display_name, evidence }
            ]
        }
    ]
}
```

A response is conceptually equivalent to:

```text
ClarificationResponse {
    request_id,
    choices: [
        { clause_id, selected_column_id }
    ]
}
```

The exact Rust and persisted JSON shapes require review. Responses must be
validated against the original request, source revision, table ID, unresolved
clause IDs, and candidate sets. A response cannot introduce an arbitrary column
that was not presented. Changed source or parser configuration makes the
request stale and requires recognition to run again.

Where several clauses need choices, one request may present them together, but
each choice is associated with one stable clause ID. Partial responses do not
compile or execute a partial predicate.

## CLI behavior

A full REPL is not required.

The dependable default is noninteractive:

1. `baho run` records the unresolved interpretation and clarification request.
2. It prints a concise explanation and stable choices to stderr.
3. It materializes no output.
4. It exits using a documented status that distinguishes clarification from an
   execution failure.
5. The user re-runs or resumes with explicit clause-to-column selections using
   a reviewed CLI shape.

An illustrative presentation is:

```text
Could not determine the column for “< 10000”.
Candidates:
  column-2  Annual Income
  column-3  Age

Resolve clause-2 by selecting one of the column IDs above.
```

An optional `--interactive` mode may later ask numbered questions only when
stdin and stderr are attached to a terminal. Interactive presentation must use
the same structured request and response types. Scripts must never block for
input merely because ambiguity was encountered.

The exact resume command, exit status, and whether clarification creates a new
run or completes the original reserved run are open product decisions. Existing
run directories must never be overwritten, so resumption likely creates a new
run that references the earlier request.

## GUI behavior

The GUI renders each unresolved clause and its candidate columns using the same
core clarification contract. Submitting all required choices triggers response
validation, plan compilation, and execution against the same immutable opened
source snapshot.

The GUI must not implement its own value search, candidate ordering, or
grounding policy. It may display source locations or bounded previews only when
the core contract explicitly supplies privacy-safe evidence.

## Evidence and diagnostics

Recognition evidence should record:

- ungrounded clause IDs and prompt spans;
- the exact value-normalization policy version;
- whether the table was completely scanned or verified;
- candidate stable column IDs in deterministic source order;
- per-column match counts and bounded locations where safe;
- type compatibility for comparison candidates;
- automatic unique grounding or user-selected grounding;
- clarification request and response IDs; and
- refusal or stale-response reasons.

Potential diagnostic categories include value not found, value found in
multiple columns, comparison column required, no type-compatible column,
clarification required, invalid clarification response, stale clarification,
and grounding resource limit exceeded. Exact stable codes must be finalized
before implementation.

Observed values must not be persisted merely to explain matching. The prompt
already contains the query literal; evidence should otherwise favor counts,
column IDs, and bounded source coordinates.

## Architecture and ownership

- `baho-ingest-csv` may provide stream-oriented value lookup and typed column
  evidence, but it does not interpret prompts or choose columns.
- `baho-model` owns any reusable source-bound lookup result and source-coordinate
  types without owning interaction policy.
- `baho-core` owns ungrounded clauses, candidate construction, uniqueness rules,
  clarification request/response validation, and compilation of the completed
  canonical intent.
- `baho-plan` owns serializable recognition evidence and receives only fully
  grounded predicates; it does not scan source data.
- `baho-exec` is unchanged except for fixes independently required by its Epic
  006 contract; it never asks questions or infers columns.
- `baho-run` persists versioned clarification artifacts and relationships
  between original and resumed runs.
- `baho-cli` and `baho-gui` present the shared clarification contract.
- `baho-llm` remains outside this deterministic path.

## Out of scope

- Semantic-role inference such as `earning` automatically selecting income.
- Confidence scores, ambiguity margins, fuzzy matching, spell correction,
  embeddings, or statistical correlation.
- Treating blank cells as categories such as unemployed or unknown.
- Decoding undocumented values such as `U`, `0`, or `1`.
- Currency or period inference and automatic unit conversion.
- Searching across unselected tables, joins, aggregates, or conversational
  history.
- Allowing a clarification response to bypass plan validation.
- A mandatory terminal REPL.

## Alternatives considered

### Semantic role aliases

Deferred. A versioned ontology may eventually help rank or label choices, but
it should not silently resolve `earning` to one of personal, household, gross,
net, monthly, or annual income.

### Select the first column containing the value

Rejected. Source order is not semantic evidence and hides ambiguity.

### Treat a sampled unique match as conclusive

Rejected. A later row may contain the same value in another column.

### Require explicit columns forever

Safe but less useful. Structured clarification preserves determinism while
allowing users to express value-centered requests and resolve only the missing
information.

## Fixture and test strategy

Use synthetic fixtures covering:

- a categorical value appearing in exactly one column;
- the value appearing in two columns, producing ordered choices;
- a match that appears unique in the sample but occurs in another column later;
- a value absent from the table;
- exact case and whitespace normalization boundaries;
- an unbound numeric comparison with one, several, or no compatible columns;
- multiple unresolved clauses in one Boolean predicate;
- complete-predicate refusal when one clause remains unresolved;
- accepted, invalid, incomplete, and stale clarification responses;
- source revision and candidate-set binding;
- deterministic evidence with bounded locations and no source dump;
- CLI noninteractive behavior and GUI use of the same core request; and
- compilation into the unchanged Epic 006 predicate IR with preserved
  provenance.

Tests should exercise complete-table verification explicitly so a bounded
profile cannot accidentally establish uniqueness.

## Acceptance criteria

1. An exact implicit text value grounds automatically only after complete-table
   verification shows that it occurs in exactly one column.
2. A value occurring in multiple columns produces a structured choice limited
   to those columns and materializes no output.
3. An unbound ordered comparison produces choices from type-compatible columns;
   it is not grounded by searching for the threshold value.
4. Descriptive terms such as `earning` do not silently select semantic roles.
5. Every response is bound to the original request, source revision, table,
   clauses, and candidate sets and is revalidated before compilation.
6. Unresolved or partially resolved predicates never execute.
7. CLI and GUI presentation consume the same core clarification types; the
   default CLI never blocks for interactive input.
8. Evidence is deterministic and bounded, and run artifacts never contain an
   unbounded value index or source dump.
9. Completed intents compile to Epic 006 plans without adding executor-specific
   grounding behavior.
10. Synthetic regressions cover the behavior and the workspace baseline passes:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

## Implementation outline

Do not begin until both review gates are cleared and Epic 006 is implemented.

1. Finalize implicit-clause grammar and exact value-normalization policy.
2. Define versioned grounding outcomes and clarification request/response types.
3. Define the complete-table verification strategy and resource-limit refusal
   policy.
4. Add failing unique, ambiguous, absent, and sample-misleading value tests.
5. Implement source-bound exact value lookup without persisting source data.
6. Construct deterministically ordered candidates for implicit equality and
   unbound comparisons.
7. Validate responses and compile only completely grounded predicates.
8. Persist versioned clarification evidence and original/resumed run links.
9. Add noninteractive CLI presentation, then GUI presentation using the same
   contract.
10. Run focused tests followed by the workspace baseline.

## Open questions

1. When a value is absent, should Baho offer all text-compatible columns or only
   refuse and ask the user to rewrite with an explicit header?
2. Is unique occurrence sufficient for automatic grounding, or should Baho
   always ask the user to confirm even a unique column?
3. Should repeated occurrences in one column strengthen evidence, or is the set
   of matching columns the only decision input?
4. What resource bound and overflow behavior should the complete-table lookup
   use for very wide or high-cardinality inputs?
5. What CLI syntax should submit clause choices without making display names
   part of the stable contract?
6. Does a clarification response create a linked new run, and what manifest
   fields describe that relationship?
7. Which exit status communicates `needs clarification` without treating it as
   a processing crash?
8. Should optional terminal prompting be part of this epic or a later
   presentation-only follow-up?
