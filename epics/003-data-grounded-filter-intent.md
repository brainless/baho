# Epic 003: Data-Grounded Filter Intent

Status: Proposed — further brainstorming required before implementation

Motivating run: None; this is a forward-looking design prompted by a hypothetical
demographics CSV.

User intent: `Unemployed or earning < 10,000`

## Review gate

This epic is deliberately not implementation-ready. It records a promising direction,
the limits of deterministic inference, and the product decisions that must be resolved
first. Do not implement it until the open questions have been discussed, the grammar
and refusal policy have been narrowed, and this status has been updated to indicate
approval.

## Delivery split

This document remains the umbrella exploration for data-grounded filtering. Its
full combination of Boolean execution, typed comparison, implicit value
grounding, semantic roles, and ambiguity policy is intentionally not one
implementation unit.

The reviewed direction is split into two narrower proposed epics:

- Epic 006 defines explicit-column Boolean filters. Every atomic condition is
  bound to an exact source header before execution; it adds the predicate IR,
  typed comparison, Boolean semantics, and deterministic filtering without
  inferring omitted columns.
- Epic 007 builds on Epic 006 with implicit value-to-column grounding and
  structured clarification. Exact complete-table evidence may resolve an
  omitted column; otherwise Baho asks the caller to choose rather than applying
  semantic-role or confidence-based guessing.

Epic 003 is not superseded or approved by this split. Its broader semantic-role
and unit-inference ideas remain brainstorming material outside Epics 006 and
007 unless separately reviewed later.

## Summary

Extend Baho beyond header-naming retrieval into a constrained, deterministic filter
language whose omitted column references may be grounded in evidence from the selected
table. For the motivating intent, Baho would attempt to interpret:

```text
Unemployed or earning < 10,000
```

as a Boolean predicate equivalent to:

```text
equal(<employment-related column>, "Unemployed")
or less_than(<income-related column>, 10000)
```

The operators and literal are syntactically straightforward. The hard problem is
grounding `Unemployed` and `earning` to columns that the user did not name exactly.
Baho may do that without an LLM only when table evidence and small, explicit semantic
rules produce one sufficiently strong interpretation. It must refuse when the source
does not distinguish plausible meanings.

The initial design should combine exact observed-value grounding, explicit header-role
aliases, column type evidence, and whole-parse ambiguity handling. It must not claim
general natural-language understanding, silently interpret codes or blank values, or
turn weak correlations into domain facts.

An eventual LLM planner may propose the same typed predicate, but its output must pass
the same structural and typed validation. An LLM proposal does not remove genuine
ambiguity in the source.

## Context and current behavior

Epic 002 introduced a composable but intentionally narrow recognizer. It supports an
initial retrieval action, an optional distinct modifier, and one exact or terminal-`s`
column phrase. Its canonical intent selects one column and compiles to either:

- a `select` step; or
- `filter(is_not_blank)` followed by `select` and `distinct`.

That design cannot represent the motivating intent:

- there is no initial retrieval action;
- two different columns may be involved;
- the prompt contains a Boolean `or`;
- `Unemployed` is most naturally a cell value rather than a header mention;
- `earning` describes a semantic column role rather than necessarily repeating a
  header; and
- `< 10,000` requires a typed numeric comparison.

The plan IR currently exposes only the `is_not_blank` expression. The executor assumes
that a filter predicate references one column and does not implement equality, ordered
comparison, or recursive Boolean expressions. Column definitions also do not yet carry
the semantic type and unit information needed to distinguish general numeric fields
from income fields.

## Problem

Users commonly describe rows by properties rather than by exact schema names. A useful
filter recognizer must answer several independent questions:

1. Which spans are operators, literals, connectors, and possible column or value
   mentions?
2. Does an unqualified categorical term occur as a value in one or more columns?
3. Does a relational phrase such as `earning` match an explicit, versioned semantic
   role assigned from a header?
4. Is a literal compatible with the candidate column's parsed type and unit?
5. Does the complete predicate have one defensible interpretation, or only several
   plausible interpretations?
6. What should a predicate-only request return when it contains no explicit projection?

These decisions must remain separate and observable. Finding a word in one column is
evidence, not proof of the user's intended meaning. Likewise, identifying a numeric
column does not establish that it represents earnings.

## Proposed deterministic interpretation model

Recognition should occur in two phases: syntax first, schema grounding second.

### 1. Parse an ungrounded predicate

The prompt is parsed into a constrained intermediate form that preserves omissions:

```text
Or(
    ImplicitEquality(value = Text("Unemployed")),
    Compare(
        subject = SemanticTerm("earning"),
        operator = LessThan,
        literal = Number(10000)
    )
)
```

At this stage, neither clause contains a fabricated column ID. Tokens have
non-overlapping roles and source spans, following the evidence discipline established
by Epic 002.

The grammar should be deliberately small. A first implementation might recognize:

- explicit comparison operators: `=`, `!=`, `<`, `<=`, `>`, and `>=`;
- Boolean `and` and `or`, with documented precedence or required parentheses;
- quoted text, unquoted constrained text values, and numeric literals;
- explicit header references; and
- carefully defined implicit equality for an otherwise unbound categorical term.

The exact first grammar remains a brainstorming question rather than an accepted scope.

### 2. Ground each clause against the selected table

For each unbound term, construct bounded candidates from independent evidence sources.

#### Observed-value evidence

Normalize a text literal conservatively and search a bounded or purpose-built value
index for exact matches. If `Unemployed` occurs in exactly one categorical column,
that is strong evidence for:

```text
equal(<that column>, "Unemployed")
```

Value matching must not generalize `Unemployed` to `Not employed`, infer that a blank
job title means unemployment, or decode values such as `U` without an explicit rule or
schema annotation. Case and whitespace normalization need their own reviewed contract;
the raw source value and provenance remain authoritative.

#### Header-role evidence

A small, centralized, versioned alias table may map normalized header terms to canonical
semantic roles. For example, a reviewed income role might recognize header tokens such
as `income`, `earnings`, `salary`, or `wage`. Prompt term `earning` could then request
that role.

This vocabulary is an ontology, not merely a synonym array. Each addition changes what
Baho claims a column means and therefore requires collision tests and documentation.
It must not live in `baho-exec`, and adding an alias must not add an executor operation.

#### Type and unit evidence

A numeric comparison may target only a column whose interpreted values and schema are
compatible with the literal. Currency markers or explicit schema metadata may strengthen
an income candidate. Numeric compatibility alone is insufficient: age, household size,
and income may all be numeric.

Units are semantically significant. Baho must not compare `10000` to a monthly-income
column when the user might mean annual income, or compare unlike currencies, without a
reviewed rule or explicit metadata.

### 3. Rank complete grounded predicates

Candidate scores should be computed for complete predicates, not by greedily resolving
each token in isolation. Stronger evidence classes should dominate weaker ones, for
example:

1. explicit complete column reference plus compatible literal;
2. exact observed value unique to one compatible column;
3. exact canonical header-role match plus compatible type and unit; and
4. explicitly documented weaker combinations, if any are approved later.

The scoring formula, threshold, ambiguity margin, evidence classes, and deterministic
tie-breaking must be configuration or stable policy rather than incidental control
flow. A unique top score is not automatically adequate; it must also clear an absolute
acceptance threshold.

If one clause remains unresolved, the entire predicate is refused. Baho must not execute
only the clauses it happened to understand.

## Illustrative success case

Given a synthetic table shaped like:

```csv
Name,Job Position,Annual Income
Asha,Engineer,75000
Ravi,Unemployed,0
Mina,Designer,8500
```

the motivating prompt could be grounded when all of the following hold:

- `Unemployed` exactly matches an observed value only in `Job Position`;
- `Annual Income` has the reviewed canonical role `income`;
- `earning` is an accepted prompt alias for that role;
- the column is parsed as numeric and its unit semantics are compatible with `10000`;
- no competing complete predicate falls within the ambiguity margin; and
- predicate-only prompts have an approved output-shape policy.

The resulting plan would be conceptually equivalent to:

```text
filter(
    or(
        equal(column-job-position, text("Unemployed")),
        less_than(column-annual-income, number(10000))
    )
)
```

This syntax is illustrative. The persisted representation must use reviewed, typed,
versioned Rust plan types.

## Required refusal cases

Baho should not produce a result when, for example:

- `Unemployed` appears in both `Employment Status` and `Notes`;
- both `Personal Income` and `Household Income` satisfy the income role;
- several numeric columns exist and no semantic evidence identifies income;
- unemployment is represented only by a blank job-position cell;
- employment is encoded as unexplained values such as `0`, `1`, or `U`;
- the threshold's period or currency is incompatible or unclear;
- the literal cannot be parsed without locale-dependent assumptions;
- type inference is too weak or materially inconsistent within the candidate column;
  or
- `or` can participate in more than one valid parse under the supported grammar.

Refusal is successful behavior. Diagnostics should identify the unresolved clause and
provide bounded candidate evidence without dumping source values.

## Output semantics

The motivating prompt expresses only a predicate. It does not say which columns to
return. One plausible rule is:

```text
predicate-only prompt -> retain all source columns for matching rows
```

This would make the request behave like a filter rather than an implicit projection.
It is not yet an accepted product decision. Alternatives include requiring an explicit
retrieval phrase, requiring an explicit projection, or returning only the columns used
by the predicate. This decision affects the canonical intent, plan compilation, CLI
output, and future conversational refinement, so it must be resolved before coding.

## Proposed plan IR direction

The expression IR will need typed literals, comparisons, and recursive Boolean
composition. A possible conceptual shape is:

```text
Expression =
    IsNotBlank(column)
    | Equal(column, literal)
    | NotEqual(column, literal)
    | LessThan(column, literal)
    | LessThanOrEqual(column, literal)
    | GreaterThan(column, literal)
    | GreaterThanOrEqual(column, literal)
    | And(expressions)
    | Or(expressions)

Literal = Text(string) | Number(decimal) | Boolean(bool) | ...
```

This is not an approved schema. In particular, brainstorming must resolve:

- whether numbers use a decimal representation rather than `f64`;
- whether comparison operands should be general expressions or restricted to
  column-versus-literal in the first version;
- null and missing-cell semantics;
- text equality normalization;
- short-circuit behavior and diagnostic emission;
- heterogeneous and malformed column values; and
- whether extending the current enum is compatible with plan schema version 1 or
  requires plan IR version 2.

Persisted schema evolution must be explicit. Existing version 1 plans and historical
run artifacts must not be rewritten or reinterpreted.

## Diagnostics and recognition evidence

New or refined stable diagnostics will likely be required. Candidate codes for review
include:

- `intent.predicate_unsupported`: the prompt is outside the constrained grammar;
- `intent.value_not_found`: an implicit value matches no compatible column;
- `intent.value_ambiguous`: an implicit value matches multiple columns;
- `intent.semantic_role_not_found`: no column has the requested role;
- `intent.semantic_role_ambiguous`: several columns have the requested role;
- `intent.literal_invalid`: a literal cannot be parsed under the recorded rules;
- `intent.unit_ambiguous`: comparison units cannot be reconciled safely; and
- `intent.predicate_ambiguous`: multiple complete grounded predicates remain viable.

Names and boundaries are provisional. The final set should remain small and distinguish
failures that a user can resolve differently.

Recognition evidence should record, in bounded deterministic form:

- normalized prompt tokens and their original indices;
- operator, connector, literal, value-term, and semantic-term spans;
- the ungrounded predicate structure;
- candidate column IDs and display names per clause;
- evidence classes such as explicit header, exact observed value, semantic role, type,
  and unit compatibility;
- scores, threshold, ambiguity margin, and rejection reasons;
- the selected grounded predicate, when any; and
- the stable refusal reason otherwise.

Observed source values must not be persisted merely to explain matching. Evidence may
record a bounded normalized query term, counts, column IDs, and source locations where
appropriate, while respecting the existing prohibition on logging full document
contents.

## Architecture and ownership

### `baho-model`

May need reviewed value/type metadata and unit concepts shared by planning and
execution. Raw source representation and provenance remain separate from interpreted
values. Schema additions must not imply domain semantics that were not evidenced.

### `baho-ingest-csv`

Continues to own CSV parsing and physical/value profiling. It may produce bounded
column-shape evidence needed by grounding, but it must not interpret prompts or own the
income ontology.

### `baho-plan`

Owns the versioned typed predicate IR, literals, Boolean composition, structural
validation, and serializable recognition evidence. It must not contain planner policy
or perform source scans.

### `baho-exec`

Owns typed validation and deterministic evaluation of approved predicate operations.
It must define behavior for missing, malformed, and incompatible values without
silently coercing them to null or false. It does not infer columns or semantic roles.

### `baho-core`

Owns the constrained grammar, ungrounded predicate model, semantic-role vocabulary,
candidate construction, evidence scoring, ambiguity policy, and compilation into the
plan IR. Recognition should consume an explicit bounded table profile rather than
reaching into CLI artifacts.

### `baho-cli`

Persists the selected or refused interpretation and renders concise diagnostics. Any
changed plan-artifact evidence shape requires an explicit artifact schema version
change independent of the nested plan IR version.

### `baho-llm`

Remains outside this deterministic path. A future LLM planner may propose typed plans,
but provider transport does not own grammar, grounding, validation, or execution.

## Scope candidates for the first implementation

The following is a proposed narrow slice to refine during brainstorming:

- predicate-only filtering with an explicitly chosen output policy;
- one selected table;
- exact header references;
- exact observed categorical-value grounding;
- a small reviewed semantic-role vocabulary, initially perhaps only income;
- typed numeric literals with unambiguous separators;
- equality and ordered numeric comparisons;
- `and` and `or` with explicit precedence;
- recursive typed predicate validation and execution;
- deterministic candidate scoring and ambiguity refusal; and
- bounded recognition evidence and stable diagnostics.

## Out of scope

- General natural-language understanding.
- Fuzzy spelling, embeddings, or opaque similarity models.
- Inferring meanings from statistical correlation between columns.
- Treating blanks as domain values such as unemployed or unknown.
- Decoding undocumented categorical codes.
- Automatic currency or time-period conversion.
- Locale guessing when separators or numeric forms are ambiguous.
- Negation with broad linguistic scope such as `not employed or earning...` until
  precedence and scope are designed.
- Cross-table predicates, joins, aggregates, grouping, or subqueries.
- Arbitrary expressions, SQL strings, generated source code, or host-language
  evaluation.
- Live LLM calls as part of the deterministic recognizer.

## Alternatives considered

### Require exact column names everywhere

This is safest and should remain an explicit fallback syntax, but it does not address
the useful case where a value itself uniquely identifies a categorical column. It may
still be the appropriate initial product boundary if the proposed grounding policy
cannot be made understandable.

### Infer from column types alone

Rejected. Multiple demographic fields are commonly numeric. Numeric type compatibility
can eliminate impossible candidates but cannot establish an income meaning.

### Search all values and select the first match

Rejected. Source order is not semantic evidence, duplicate values across columns are
common, and greedy resolution hides ambiguity.

### Add a large synonym dictionary

Rejected as a general solution. A broad dictionary becomes an undocumented domain
ontology with surprising collisions. Semantic roles should be small, versioned,
reviewed, and supported by type and unit evidence.

### Use fuzzy matching or embeddings without an LLM

Deferred. Avoiding an LLM does not automatically make a similarity model deterministic,
explainable, or safe. Model versions, thresholds, and opaque neighbors introduce many
of the same grounding problems. Exact evidence should be established first.

### Send the prompt directly to an LLM

Rejected as the execution contract. An LLM may eventually propose a constrained plan,
but it cannot safely decide whether `Household Income` or `Personal Income` was intended
when the request and source do not say. Validation and explicit ambiguity handling
remain necessary.

## Fixture and test strategy

Implementation, once approved, should use small synthetic demographics fixtures rather
than real user data. At minimum the fixtures should cover:

- one categorical column containing `Unemployed` and one numeric income column;
- the same categorical value appearing in two columns;
- personal and household income columns competing for the same role;
- several numeric columns where no header has an income role;
- malformed and blank numeric cells with preserved raw values and diagnostics;
- thousands separators, decimal values, negative values, and boundary equality;
- incompatible or missing currency/period metadata;
- `and`/`or` precedence and parenthesized variants if parentheses are supported;
- a clause that resolves while another clause does not, causing whole-predicate
  refusal; and
- provenance preservation after Boolean filtering.

Tests should be layered across grammar parsing, grounding, plan validation,
execution, orchestration, and CLI artifacts. They should assert structured evidence
and diagnostic codes rather than private scoring implementation details.

## Provisional acceptance criteria

These criteria are discussion material and must be finalized before implementation:

1. The motivating intent can produce a typed Boolean filter only when each clause has
   one sufficiently evidenced grounding.
2. Exact observed-value grounding never silently chooses among multiple matching
   columns.
3. A semantic header role is assigned only through documented aliases and compatible
   type/unit evidence.
4. Numeric comparisons use reviewed literal, precision, null, and malformed-value
   semantics.
5. Failure to ground any clause refuses the complete predicate and materializes no
   misleading partial result.
6. Predicate-only output semantics are explicit in the canonical intent and persisted
   plan.
7. The plan contains only closed, versioned operations; no user or LLM-generated code
   is executed.
8. Recognition evidence explains token roles, grounding candidates, scoring, and
   refusal without exposing unbounded source data.
9. Existing Epic 001 and Epic 002 behavior remains unchanged for version 1 plans and
   retrieval prompts.
10. Synthetic regression tests cover success, ambiguity, incompatible types/units,
    malformed cells, and provenance.
11. Focused tests and the workspace baseline pass:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

## Provisional implementation outline

Do not begin this outline until the review gate is cleared.

1. Resolve the open product and semantic questions below.
2. Specify the constrained grammar and ungrounded predicate types independently of the
   persisted plan IR.
3. Specify typed literals, comparison/null semantics, Boolean precedence, resource
   limits, and plan schema evolution.
4. Define the bounded table profile consumed by grounding, including exact-value, type,
   and unit evidence.
5. Define a minimal versioned semantic-role vocabulary and collision policy.
6. Implement candidate grounding and whole-predicate ambiguity handling in `baho-core`.
7. Extend structural and typed validation in `baho-plan` and `baho-exec`.
8. Implement deterministic predicate evaluation while retaining raw values,
   diagnostics, row order, and provenance.
9. Version recognition evidence and CLI artifact envelopes as required.
10. Add synthetic layered regressions and run the workspace baseline.

## Open questions for brainstorming

1. Should a predicate-only prompt return every column for matching rows, or must the
   user state a retrieval action or projection?
2. Is exact observed-value grounding desirable, or should implicit categorical terms
   always require a column name?
3. Should observed-value matching scan the complete selected table, use a bounded
   profile, or use a separately bounded index built during materialization?
4. What normalization is safe for text equality: exact raw text, case folding,
   whitespace folding, or an explicit per-column policy?
5. What is the smallest useful semantic-role vocabulary, and where is its version
   recorded?
6. Is `earning` sufficiently precise to mean income, or should the first grammar require
   a closer header term such as `income`?
7. How are personal, household, gross, net, monthly, and annual income distinguished?
8. Must currency and time period be explicit before a numeric threshold can execute?
9. Which numeric representation provides deterministic comparison and serialization?
10. What are the null, blank, malformed, and mixed-type semantics for each comparison?
11. Should `and` bind more tightly than `or`, or should mixed Boolean expressions
    require parentheses initially?
12. Does the first version allow only column-versus-literal comparisons, or general
    expression operands?
13. What absolute confidence threshold and ambiguity margin are explainable enough to
    persist as policy?
14. Which distinctions deserve separate stable diagnostic codes rather than evidence
    fields under one ambiguity diagnostic?
15. Does this require plan IR version 2, and what compatibility behavior should a future
    plan reader provide for versions 1 and 2?
