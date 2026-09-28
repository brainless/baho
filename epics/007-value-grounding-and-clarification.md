# Epic 007: Value Grounding and Structured Clarification

Status: Complete — the CLI-only grounding and clarification design was approved
and implemented on 2026-09-28. The selected-table grounding scan is bounded;
the preexisting execution pipeline still materializes the selected table.

Grounding artifact schema version 2 records bounded per-clause decisions and
per-column match counts. Version 1 pending clarification runs cannot be
resumed: `resolve` rejects them as incompatible, and the user must rerun the
original prompt to obtain a fresh request against the current source. This is
the migration decision because a version 1 result lacks the evidence needed
to audit an automatic choice.

Motivating run: None; this is the second implementation slice split from Epic
003.

User intent: `List rows where unemployed or < 10000`

## Review gate

Epic 006 and Epic 008 are implemented. The value-search, implicit-clause
grounding, clarification lifecycle, and CLI behavior proposed here were
reviewed on 2026-09-28 and are cleared for implementation; open questions 1
through 9 are resolved.

## Summary

Allow a Boolean filter clause to omit its column or its operator when table
evidence can either resolve the omission safely or present a bounded choice to
the user. A bare term in predicate position is grounded from table evidence;
blank predicates complete the clause set:

- an exact observed value, searched across the complete selected table, grounds
  implicit categorical equality when it occurs in exactly one column;
- a complete header match grounds an implicit column reference: a flag-shaped
  column becomes truthiness, any other column becomes non-blank;
- when more than one interpretation survives — for example a matching header
  and matching values in another column — a structured clarification request
  presents the alternatives; and
- comparisons without a column produce clarification rather than semantic
  guessing, while explicit `is blank` and `is not blank` phrases are
  recognized directly.

The completed intent compiles into the same typed predicate IR introduced by
Epic 006: `Compare`, `IsNotBlank`, `Not`, `And`, and `Or`. This epic adds no
IR nodes and no executor semantics, and it does not infer that words such as
`earning` mean income. User confirmation, not confidence scoring, resolves
evidence that is insufficient.

## Dependency on Epic 006 and Epic 008

Epic 006 provides:

- explicit-column equality and ordered comparison;
- recursive `not`, `and`, and `or` predicates;
- exact decimal literals and three-valued execution;
- stable column IDs and exact-header matching;
- predicate-only output retaining all source columns; and
- versioned plan and recognition evidence.

Epic 008 provides:

- the plan's `TextMatchPolicy` (`unicode_lowercase` under plan schema version
  3), which governs text equality in execution;
- deferred prompt-literal resolution against per-column numeric policies; and
- the policy-version linkage that grounding evidence must mirror.

The plan IR already contains `IsNotBlank` and `Not` (today used internally by
distinct operations), so blank predicates compile without an IR change. Epic
007 operates before plan compilation. Once every clause has one bound column
and one resolved predicate form, compilation, validation, and execution follow
the Epic 006/008 path.

## Clause categories

Grounding distinguishes syntax from evidence. A bare term in predicate
position — text with no operator and no following literal — is first offered
to Epic 006's complete-header binding, then searched as an observed value.
Every surviving interpretation is collected before any is accepted.

An explicit row-filter lead-in (`rows` or `where` immediately after a
`list`/`show`/`filter` action) establishes predicate position. In a compact
prompt without that lead-in, a bare term alone does not establish a row filter:
the existing retrieval interpretation and ambiguity rules remain authoritative.
A compact prompt can use a bare term as a Boolean operand only when another
explicit comparison or blank-test operator establishes the row-filter reading
for the complete predicate. Thus `List unemployed` retains its existing
retrieval-or-refusal behavior, while `List unemployed or < 10000` may be parsed
as a row filter. Quoted text remains literal and Boolean connectors retain the
Epic 006 precedence rules.

### Implicit categorical equality

An unbound categorical term such as:

```text
unemployed
```

is represented first as:

```text
ImplicitEquality(Text("unemployed"))
```

Baho searches for a matching observed value in every column of the selected
table. Matching uses the plan's text-match policy; see "Value lookup and text
matching".

Uniqueness is a property of the set of matching columns, not of the rows or
values within a column. A categorical column holding `unemployed` alongside
`employed` and `student` grounds exactly as well as a column holding only
`unemployed`: the compiled predicate is ordinary equality on the bound column
and retains only rows whose cell matches.

- If the value occurs in exactly one column, that stable column ID is proposed
  as the grounding.
- If it occurs in multiple columns, the user must choose among those columns.
- If it occurs nowhere and no header interpretation survives, Baho refuses
  with a stable value-not-found diagnostic; it does not offer columns
  unsupported by observed matches.

### Implicit column reference

A bare term that matches one complete source header under Epic 006's binding
rules is an implicit column reference. The predicate it contributes depends on
the column's shape:

- A flag-shaped column contributes truthiness, compiled as `column = "true"`.
  Blank flag cells evaluate to `unknown` and are not retained.
- Any other column contributes non-blank, compiled as `IsNotBlank`. This is a
  documented grammatical reading of a bare name ("rows where this field has
  content"), not a domain inference.

Boolean columns never use the non-blank reading: `false` is non-blank, so that
reading would retain rows the truthiness predicate correctly excludes. A
column with no non-blank cells is not flag-shaped; the non-blank reading
retains no rows in that case either. Flag shape is defined in "Blank and flag
semantics".

Duplicate normalized headers keep Epic 006's refusal with a
column-ambiguity diagnostic; source order never breaks the tie.

### Competing interpretations

A bare term may simultaneously match a header and observed values. Baho does
not prefer one evidence class over the other. Exactly one surviving
interpretation grounds automatically; two or more produce one clarification
request whose candidates are the interpretations themselves. Candidates may
share a column while differing in predicate form.

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

### Explicit blank predicates

The grammar gains atomic conditions:

```text
column is blank
column is not blank
```

`is not blank` compiles to `IsNotBlank` and `is blank` to its three-valued
negation. The phrase is one atomic condition; its `not` is not the unary
Boolean rule, so `not A is blank` negates the whole blank test. Blank testing
is defined in "Blank and flag semantics".

## Complete-table evidence

Unique occurrence is a claim about the complete selected table, not a bounded
sample. Bounded profiling may help schedule verification, but absence from a
sample cannot eliminate a column: a matching cell may appear later. A column
may be excluded only when complete schema or type evidence proves its compiled
text-equality predicate can never return `true`. With today's CSV grid, every
column that could return `true` must be checked through every selected row.

Use a stream-oriented targeted scan of those eligible columns, capped by a
configurable total-cells-scanned limit. If the cap would be exceeded before all
eligible columns are verified, refuse with the grounding resource-limit
diagnostic. Neither a sampled match nor a partial scan establishes uniqueness
or absence. Search must use the selected table and immutable source revision
to which the resulting request is bound.

Flag shape likewise requires checking every selected row in the matching
header column. The same scan limit applies; a partial shape scan cannot
establish that every non-blank value is `true` or `false`.

Retain only bounded samples and candidate details. Evidence may record counts,
stable column IDs, and bounded source locations; it must not persist a value
index or dump source contents into run artifacts.

Implementation limit (2026-09-28): Grounding reopens the selected CSV source,
checks its content revision before and after a single capped scan, and retains
only per-column counts and bounded coordinates. `open_table` still materializes
the selected region for downstream execution before grounding starts, so peak
pipeline memory remains proportional to selected table size. Streaming
grounding bounds its additional evidence memory; it does not yet make the
entire run stream-oriented.

## Value lookup and text matching

Value lookup must agree with execution. The contract is the grounding
consistency invariant:

```text
a cell matches the groundable value
iff the compiled `column = literal` predicate would be true for that cell
under the plan's text-match policy.
```

Under plan schema version 3, text equality uses Unicode lowercase comparison
of both sides with no whitespace trimming (Epic 008 locked decisions 1 and
11); earlier plan schema versions keep exact-case equality. Equality also has
typed-cell eligibility rules: the current executor returns `unknown` for a
strictly parsed numeric cell in a materially mixed column before comparing
raw text. Lookup must apply the same rule and must not count such a cell as a
match even when its raw spelling equals the prompt literal. The parser's blank
rule classifies blanks but never participates in text equality. Blank and
missing cells do not match values.

The lookup and executor should share the predicate's text-cell eligibility and
matching rule, or the implementation must prove their equivalence with tests
covering text, numeric, mixed, blank, and missing cells. A lookup match must
mean that execution of the compiled equality would return `true`, not merely
that the raw strings compare equal.

Baho never stems, spell-corrects, decodes abbreviations, or equates blanks
with domain values. Thus `unemplyed` is not silently corrected to
`unemployed`. Optional weaker match classes require a later reviewed policy
and may suggest a clarification, but must not silently ground a clause. The
raw prompt literal and raw source cell remain the authoritative evidence.

## Blank and flag semantics

Blank classification reuses the parser-configured blank rule
(`TrimmedUnicodeWhitespace` today):

- a field absent from a physically short (ragged) row is blank;
- a field whose text is empty or whitespace-only under the rule is blank;
- any other raw content — including malformed or type-incompatible text — is
  non-blank; raw content is never coerced into blank.

Unlike comparisons, blank testing is two-valued and total: it never produces
`unknown`, and ragged absence is `is blank`, not unknown. Blanks never match
values. A blank cell is not a category such as `unemployed` or `unknown`, and
blank cells neither create nor break a unique column match.

Flag shape is schema evidence, not a value interpretation: a column is
flag-shaped when it has at least one non-blank cell and every non-blank cell
text-equals `true` or `false` under the plan's text-match policy. Any other
non-blank spelling — `yes`, `no`, `1`, `0`, `Y`, `N`, or an unrelated
category — makes the column not flag-shaped, and a bare reference to it falls
back to the non-blank reading. Such spellings are not decoded (out of scope).

## Grounding outcomes

Grounding returns one of three core outcomes:

```text
Grounded
NeedsClarification
Refused
```

`Grounded` contains a complete canonical predicate whose clauses all reference
stable column IDs; each leaf is an Epic 006 `Compare` or an `IsNotBlank`,
possibly under `Not`. `NeedsClarification` contains one or more unresolved
clauses and bounded choices. `Refused` indicates unsupported grammar, invalid
literals, incompatible types, stale responses, exhausted resource bounds, or
another condition that cannot be resolved by choosing a presented candidate.

Failure to ground any clause prevents plan compilation and materialization. Baho
never executes only the clauses it happened to understand.

## Structured clarification contract

Clarification is a core data contract, independent of presentation. A request
is conceptually equivalent to:

```text
ClarificationRequest {
    schema_version,
    request_id,
    source_revision,
    table_id,
    parser_config_identity,
    prompt_identity,
    unresolved: [
        ChooseInterpretation {
            clause_id,
            rendered_condition,
            reason,
            candidates: [
                {
                    candidate_id,
                    column_id,
                    display_name,
                    predicate_form,
                    evidence
                }
            ]
        }
    ]
}
```

`predicate_form` is one of `equals_value`, `flag_is_true`, `is_not_blank`, or
`compare`, so a candidate states the exact predicate its selection compiles.

A response is conceptually equivalent to:

```text
ClarificationResponse {
    request_id,
    choices: [
        { clause_id, selected_candidate_id }
    ]
}
```

The exact Rust and persisted JSON shapes require review. Responses must be
validated against the original request, source revision, table ID, parser
configuration identity, unresolved clause IDs, and candidate sets. A response
cannot introduce a candidate that was not presented; a bare column ID is not
selectable when two candidates share a column. A changed source or parser
configuration makes the request stale and requires recognition to run again.
`resolve` reopens the source and verifies those identities before compilation;
the earlier run does not retain an executable source snapshot.

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
5. The user re-runs or resumes with explicit clause-to-candidate selections
   using a reviewed CLI shape.

An illustrative presentation for a competing bare term is:

```text
Could not determine the meaning of “unemployed”.
Candidates:
  candidate-1  column-1  Unemployed  flag is true
  candidate-2  column-2  Job         equals “unemployed”

Resolve clause-1 by selecting one of the candidate IDs above.
```

An unbound comparison renders column candidates:

```text
Could not determine the column for “< 10000”.
Candidates:
  candidate-1  column-2  Annual Income  compare
  candidate-2  column-3  Age            compare

Resolve clause-2 by selecting one of the candidate IDs above.
```

`baho resolve <run-id> <clause_id>=<candidate_id> [...]` submits all choices
from the pending request in that run. It creates a linked new run, preserves
the original run, and revalidates source and parser identity before executing.
The default CLI never prompts for input. Interactive terminal behavior is
deferred.

## Evidence and diagnostics

Recognition evidence should record:

- ungrounded clause IDs and prompt spans;
- the value-match policy version and the plan text-match policy
  (`unicode_lowercase` under schema 3, as with Epic 008 row-filter evidence);
- whether the table was completely scanned or verified;
- candidate interpretations in deterministic source order, each with stable
  column ID and predicate form;
- per-column match counts and bounded locations where safe;
- flag-shape classification and its evidence for column-reference candidates;
- type compatibility for comparison candidates;
- automatic grounding or user-selected grounding;
- clarification request and response IDs; and
- refusal or stale-response reasons.

Potential diagnostic categories include value not found, value found in
multiple columns, multiple interpretations for a bare term, comparison column
required, no type-compatible column, column header ambiguous, clarification
required, invalid clarification response, stale clarification, and grounding
resource limit exceeded. Exact stable codes must be finalized before
implementation.

Observed values must not be persisted merely to explain matching. The prompt
already contains the query literal; evidence should otherwise favor counts,
column IDs, predicate forms, and bounded source coordinates.

## Architecture and ownership

- `baho-ingest-csv` may provide stream-oriented value lookup and typed column
  evidence, but it does not interpret prompts or choose columns.
- `baho-model` owns `TextMatchPolicy`, any reusable source-bound lookup result,
  and source-coordinate types without owning interaction policy.
- `baho-core` owns ungrounded clauses, candidate construction, uniqueness
  rules, clarification request/response validation, and compilation of the
  completed canonical intent.
- `baho-plan` owns serializable recognition evidence and receives only fully
  grounded predicates; it does not scan source data.
- `baho-exec` keeps its predicate semantics and IR. A small refactor to share
  text-cell eligibility with lookup is acceptable if needed for the grounding
  consistency invariant.
- `baho-run` persists versioned clarification artifacts and relationships
  between original and resumed runs.
- `baho-cli` presents the shared clarification contract and submits responses.
- `baho-llm` remains outside this deterministic path.

## Out of scope

- Semantic-role inference such as `earning` automatically selecting income.
- Confidence scores, ambiguity margins, fuzzy matching, spell correction,
  embeddings, or statistical correlation.
- Treating blank cells as categories such as unemployed or unknown.
- Decoding undocumented values such as `U`, `0`, `1`, `yes`, or `no`,
  including them as flag spellings.
- Projecting a bare column name: predicate-position terms never project.
  Column projection remains Epic 002 retrieval behavior.
- Currency or period inference and automatic unit conversion.
- Searching across unselected tables, joins, aggregates, or conversational
  history.
- Allowing a clarification response to bypass plan validation.
- A mandatory terminal REPL.
- GUI clarification presentation; the shared core contract leaves room for a
  later GUI milestone.

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

### Bare column name as projection

Rejected. Projection cannot compose with Boolean connectors — `unemployed or
< 10000` needs `unemployed` as a Boolean operand — and silently converting a
row-filter predicate into a retrieval would change the action and drop source
columns the filter contract retains.

### Non-blank truthiness for flag columns

Rejected. `false` is non-blank, so the non-blank reading retains rows the
truthiness predicate correctly excludes.

### First-class Boolean literal or truthy IR node

Deferred. `column = "true"` under the Epic 008 text-match policy expresses
truthiness within the existing IR. A dedicated node waits for a real need,
such as a reviewed `yes`/`no` flag policy.

### Value lookup with its own match policy

Rejected. If lookup and execution disagree on which cells match, a grounding
that verified uniqueness could compile to a predicate that retains different
rows.

## Fixture and test strategy

Use synthetic fixtures covering:

- a categorical value appearing in exactly one column alongside other values;
- a categorical value appearing in exactly one column as its only value;
- the value appearing in two columns, producing ordered choices;
- a match that appears unique in the sample but occurs in another column later;
- a value absent from the sample but present later in another column;
- a value absent from the table;
- a compact bare term that retains the existing retrieval-or-refusal reading,
  and a compact Boolean filter anchored by an explicit operator;
- case and whitespace boundaries under the plan's text-match policy;
- a bare term matching a flag-shaped header, including `True` and `TRUE`
  spellings;
- a bare term matching a non-boolean header, retaining non-blank rows;
- a `yes`/`no` column falling back to non-blank as a documented limitation;
- a bare term matching both a header and values in another column, producing
  interpretation candidates;
- blank classification boundaries: ragged absence, empty, whitespace-only,
  and malformed raw content;
- `is blank` and `is not blank` compiling to the existing IR and evaluating
  two-valued;
- lookup matching exactly the rows the compiled equality retains, including
  typed numeric cells in materially mixed columns;
- an unbound numeric comparison with one, several, or no compatible columns;
- multiple unresolved clauses in one Boolean predicate;
- complete-predicate refusal when one clause remains unresolved;
- accepted, invalid, incomplete, and stale clarification responses;
- source revision and candidate-set binding;
- deterministic evidence with bounded locations and no source dump;
- CLI noninteractive behavior and `resolve` using the same core request; and
- compilation into the unchanged Epic 006 predicate IR with preserved
  provenance.

Tests should exercise complete-table verification explicitly so a bounded
profile cannot accidentally establish uniqueness.

## Acceptance criteria

1. An exact implicit text value grounds automatically only after complete-table
   verification shows that it occurs in exactly one column; other values in
   that column do not affect the result. Sampled absence cannot exclude a
   column, and an unfinished scan refuses rather than grounding.
2. A value occurring in multiple columns produces a structured choice limited
   to those columns and materializes no output.
3. A bare term matching a flag-shaped header grounds to `column = "true"`; a
   bare term matching any other header grounds to `IsNotBlank`. A bare term
   alone in a compact prompt retains existing retrieval-or-refusal behavior.
4. Blank and missing cells never match values and never count toward unique
   occurrence.
5. Value lookup matches exactly the cells the compiled equality predicate
   retains under the plan's text-match and typed-cell rules.
6. When a header interpretation and a value interpretation both survive, Baho
   clarifies with all surviving candidates and grounds none automatically.
7. An unbound ordered comparison produces choices from type-compatible columns;
   it is not grounded by searching for the threshold value.
8. Descriptive terms such as `earning` do not silently select semantic roles.
9. Every response is bound to the original request, source revision, table,
   parser configuration, clauses, and candidate sets and is revalidated after
   reopening the source; a response cannot select a candidate that was not
   presented.
10. Unresolved or partially resolved predicates never execute.
11. `is blank` and `is not blank` compile to `Not(IsNotBlank)` and `IsNotBlank`
    with two-valued blank semantics and no executor changes.
12. The CLI presents the core clarification types, `resolve` accepts only
    presented candidate IDs, and the default CLI never blocks for input.
13. Evidence is deterministic and bounded, and run artifacts never contain an
    unbounded value index or source dump.
14. Completed intents compile to Epic 006 plans without adding executor-specific
    grounding behavior.
15. Synthetic regressions cover the behavior and the workspace baseline passes:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

## Implementation outline

Implementation proceeds in outline order; the review gate cleared on
2026-09-28.

1. Finalize the bare-term and `is [not] blank` grammar, preserving compact
   retrieval behavior and matching lookup to the full execution predicate.
2. Define versioned grounding outcomes and clarification request/response types
   with interpretation candidates.
3. Define complete-table verification over every eligible column and the
   resource-limit refusal policy.
4. Add failing tests: unique value among other values, flag and non-flag header
   references, competing interpretations, blank boundaries, and
   sample-misleading values.
5. Implement source-bound exact value lookup without persisting source data.
6. Construct deterministically ordered interpretation candidates: header
   candidates first in source order, then value candidates in source order.
7. Validate responses and compile only completely grounded predicates.
8. Persist versioned clarification evidence and original/resumed run links.
9. Add noninteractive CLI presentation and `resolve` using the shared core
   contract.
10. Run focused tests followed by the workspace baseline.

## Open questions

1. **Resolved (review 2026-09-28):** An absent value refuses with a "value not
   found" diagnostic; Baho does not offer all text-compatible columns as
   candidates, since nothing observed narrows that choice.
2. **Resolved (review 2026-09-25):** Unique occurrence grounds automatically,
   for header and value interpretations alike; evidence records the automatic
   decision. An always-confirm mode remains a possible later option.
3. **Resolved (review 2026-09-25):** Only the set of matching columns decides.
   Per-column match counts may be recorded as evidence but never affect
   grounding.
4. **Resolved (review 2026-09-28):** Complete-table lookup uses bounded
   profiling to schedule work, then a targeted scan verifies every column
   whose compiled text-equality predicate could return `true`. Sampled absence
   never excludes a column. The scan is capped by a configurable
   total-cells-scanned limit; reaching the cap before verification completes
   refuses with the grounding resource-limit diagnostic.
5. **Resolved (review 2026-09-28):** A new `baho resolve <run-id>
   <clause_id>=<candidate_id> [...]` subcommand submits clause choices. The
   run ID identifies the pending clarification request stored in that run's
   artifacts, so the command need not repeat the request ID. Choices reference
   only `clause_id` and `candidate_id`; display names never appear in the
   submitted contract.
6. **Resolved (review 2026-09-28):** A clarification response creates a new
   run rather than completing the original reserved run, consistent with run
   directories never being overwritten. The new run's manifest records
   `resumes_run_id` (the original run ID) and `clarification_request_id` (the
   request being answered). The original run's manifest and outcome
   (`needs_clarification`) remain unchanged.
7. **Resolved (review 2026-09-28):** A dedicated exit status (distinct from
   success and from a generic processing error) communicates `needs
   clarification`, so scripts can branch on it explicitly.
8. **Resolved (review 2026-09-28):** Optional terminal (`--interactive`)
   prompting is deferred to a later presentation-only follow-up epic; this
   epic implements the noninteractive CLI contract only.
9. **Resolved (review 2026-09-28):** `yes`/`no`/`1`/`0` columns do not gain a
   flag-shape policy in this epic; users must write explicit comparisons
   against those spellings, consistent with "Out of scope."
