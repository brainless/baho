# Epic 002: Composable Intent Matching and Select-Only Retrieval

Status: Implemented

Motivating run: `000003`

Baseline user intent: `Extract all the unique floor plans`

Follow-up intents:

- `List income`
- `List incomes`
- `Show time` when available columns may include `Time` or `Show Time`

## Summary

Extend baho's deterministic intent recognizer from one rigid
`action + distinct modifier + column` grammar into a small composable matcher.
Retrieval actions, optional operation modifiers, and column mentions will be
recognized as separate, non-overlapping token roles. This permits select-only
requests such as `List income`, makes action and operation vocabulary explicitly
extensible through canonical aliases, and recognizes a conservative singular or
plural variant such as `income`/`incomes`.

The matcher will construct and score complete candidate parses rather than search
for a header anywhere in the prompt independently. A prompt token cannot serve as
both control vocabulary and part of a column name. This rule makes collisions such
as the action `show` and the column `Show Time` deterministic and explainable.

No LLM is involved. The recognized semantics continue to compile into the closed,
versioned plan IR and pass the existing structural and typed validators.

## Motivating evidence

Run `000003` establishes the successful baseline. It recognized the action
`extract`, the operation word `unique`, and the column `Floor Plan`. It then
materialized a plan containing:

1. `filter` with `is_not_blank` on the selected column;
2. `select` of that column; and
3. `distinct` with `keep: first`.

The current implementation encodes that one path directly:

- the first token must be one of `extract`, `list`, `show`, `get`, or `find`;
- after optional filler tokens, the next token must be `unique` or `distinct`;
- every remaining token is treated as the column mention; and
- orchestration always generates the same filter/select/distinct plan.

Consequently, `List income` accepts `list` as an action and then rejects `income`
because it occurs in the position reserved for a mandatory operation. Column
matching is never attempted. `List incomes` fails for the same reason even though
the existing terminal-`s` normalization could otherwise relate `incomes` to an
`Income` header.

The prompt `Show time` exposes a second issue. If `show` is consumed as an action,
the unconsumed column phrase is `time`. A global search over the complete prompt
could instead match a column named `Show Time`, reusing `show` as both an action and
part of the column. If both `Time` and `Show Time` exist, that strategy can select
the wrong column despite an exact interpretation of `time` being available.

These are limitations of the recognizer and plan builder, not of the executor.
The plan IR and executor already support a select-only plan.

## Problem

The recognizer currently conflates four decisions:

- whether the prompt begins with a supported retrieval action;
- whether the user requested a modifier such as distinctness;
- which prompt tokens denote the column; and
- which plan steps implement the recognized request.

This makes an operation word mandatory even when the action and column fully
express a useful request. Adding synonyms by appending words to arrays does not
solve this structural limitation, and searching globally for header names creates
token-role collisions.

The next matcher needs to accept a slightly broader but still constrained language
without pretending to understand arbitrary prose. It must explain which tokens
were assigned to which roles, prefer complete matches over partial overlap, and
refuse unresolved ambiguity.

## Desired behavior

### Canonical intent model

Recognition produces structured semantics equivalent to:

```text
RetrievalIntent {
    action: Retrieve,
    column_id: <stable selected column ID>,
    distinct: <true or false>
}
```

The exact Rust type names may differ, but the semantic operation must not be stored
only as an arbitrary string. Surface words are recognition evidence; canonical
semantics drive plan construction.

Initial action aliases map to the canonical retrieval action:

```text
extract, list, show, get, find, display, return
```

Initial distinct-modifier aliases map to `distinct: true`:

```text
unique, distinct, deduplicate, deduplicated
```

The modifier is optional. Alias tables are deterministic, centralized, and tested.
Adding an alias must not add a new executor operation implicitly.

### Plan compilation

A plain retrieval compiles to select only:

```text
List income
    -> select(Income)
```

It preserves every semantic input row, including blank values. Baho must not add an
unstated nonblank filter merely because the result is a single column.

A distinct retrieval retains the behavior established by Epic 001:

```text
List unique income
    -> filter(is_not_blank(Income))
    -> select(Income)
    -> distinct(Income, keep:first)
```

This preserves the existing meaning of `unique` and `distinct`: blank values are
excluded explicitly, exact typed values are deduplicated, and the first occurrence
and its provenance are retained.

### Token roles and column spans

Recognition proceeds by constructing complete candidate parses with non-overlapping
token spans:

1. recognize an initial action alias;
2. consume only documented filler tokens around an optional modifier — a
   review follow-up lets the column span begin at any position within the
   leading filler run, so a header phrase that starts with a filler word
   (e.g. `Value`) remains matchable, while skipped tokens before the span may
   still be only fillers plus the optional modifier;
3. identify a contiguous column span from the remaining unconsumed tokens;
4. match that span against normalized column labels; and
5. accept one parse only when it clears the ambiguity margin; the match
   threshold sketched here proved inert (every match class scored above it)
   and was removed in review follow-ups.

A token may belong to exactly one semantic role. In particular, a token consumed as
an action or modifier cannot also contribute to a column match. Filler tokens may be
ignored only in documented grammar positions; they must not be deleted from the
middle of a possible header phrase merely to manufacture a match.

Column matching prefers, in order:

1. an exact normalized contiguous phrase;
2. a phrase differing only by a supported singular/plural token variant; and
3. any future explicitly defined weaker match, which must remain below the first two
   classes and be separately evidenced.

The first implementation does not need weak partial-token matching. Exact coverage
in both directions is safer than the current one-sided overlap score: the selected
prompt span covers the complete normalized header, and the complete normalized
header covers the selected prompt span, modulo supported inflection.

### Singular and plural matching

For this epic, a token pair is equivalent when it is:

- exactly equal after case and whitespace normalization; or
- identical except that one side has one additional terminal `s`.

The comparison checks the relationship between the two original normalized tokens;
it does not strip `s` unconditionally from both. Thus `income` matches `incomes`,
while an exact word ending in `s` remains intact. More complex `es`, `ies`, and
irregular forms are out of scope until they can be introduced as explicit,
well-tested rules rather than a general linguistic claim.

### Collision behavior

Given columns named `Time` and `Show Time`:

| Prompt | Interpretation |
|---|---|
| `Show time` | action `show`; exact column `Time` |
| `List Show Time` | action `list`; exact column `Show Time` |
| `Show Show Time` | action `show`; exact column `Show Time` |

If only `Show Time` exists, `Show time` must not silently select it from the partial
word `time`. It produces `intent.column_not_found` with bounded evidence about the
unmatched span. The user can say `List Show Time` or `Show Show Time` to allocate an
unambiguous action token and complete column span.

The initial grammar continues to require an action. Bare `Show Time` as a reference
to the `Show Time` column is not accepted because it has no separate retrieval
action under the non-overlap rule. Quoted column references and action-free prompts
may be considered later with explicit precedence rules.

When more than one complete candidate parse remains within the configured ambiguity
margin, recognition emits `intent.parse_ambiguous` and does not materialize a result.
When one column span maps equally to multiple columns — multiple headers share the
same normalized label that a single prompt phrase matches — recognition emits
`intent.column_ambiguous` instead, listing the matched column display names in
deterministic order. The two cases are distinguished structurally: tying candidates
that share a column span and resolve different columns are column ambiguity, while
tying candidates with distinct spans or modifier/column assignments are parse
ambiguity. For example, two `floor`/`Floor` headers produce `intent.column_ambiguous`
for `list floor`, whereas `list unique income` with columns `Unique Income` and
`Income` produces `intent.parse_ambiguous` because the prompt phrase can be read
either as a distinct modifier plus `Income` or as the whole `Unique Income` column.
This distinction does not affect the collision table: `Show time`, `List Show Time`,
and `Show Show Time` each resolve to a single unambiguous parse as documented above.

## Scope

### In scope

- A structured retrieval intent with an optional distinct modifier.
- Centralized aliases for supported retrieval actions and distinct modifiers.
- The initial new aliases `display`, `return`, `deduplicate`, and `deduplicated`.
- Select-only plans for prompts such as `List income`.
- Preservation of blanks and duplicates in plain select-only retrieval.
- Existing explicit blank filtering and first-occurrence behavior for distinct
  retrieval.
- Conservative terminal-`s` singular/plural equivalence.
- Non-overlapping token-role assignment.
- Exact contiguous column phrase matching after documented normalization.
- Deterministic scoring, tie-breaking, refusal, diagnostics, and recognition
  evidence.
- Unit, orchestration, and CLI artifact coverage using synthetic data.

### Out of scope

- Arbitrary natural-language understanding or an LLM planner.
- Actions other than retrieval, such as calculating, grouping, joining, sorting, or
  updating data.
- New plan IR operations; `select`, `filter`, and `distinct` already suffice.
- Multi-column retrieval in one prompt.
- Action-free prompts.
- Quoted or escaped column syntax.
- Arbitrary word reordering or searching a column name anywhere while ignoring all
  surrounding text.
- General stemming, lemmatization, irregular plurals, or locale-aware morphology.
- Multiword modifiers such as `without duplicates`.
- Fuzzy spelling correction or semantic header similarity.

## Architecture and ownership

### `baho-core`

Owns the deterministic grammar, alias tables, token-span construction, candidate
parse scoring, ambiguity policy, canonical recognized intent, and compilation from
recognized intent to plan steps. The recognizer remains independent of CLI
presentation and LLM providers.

Plan construction should be extracted from the unconditional three-step block in
orchestration into a small function whose input is the canonical intent. This keeps
recognition and compilation separately testable:

```text
recognize(prompt, columns) -> canonical intent + evidence
compile(canonical intent, source) -> validated plan
```

### `baho-plan`

Retains the existing version 1 IR. A select-only plan is already structurally valid,
so this epic does not require a new plan schema version. Recognition evidence may
continue to use plan-owned serializable evidence types, but its persisted shape must
describe token roles and the canonical operation without embedding parser policy in
the executor.

### `baho-exec`

Requires no new operation. Existing sequential execution of `select`, `filter`, and
`distinct` remains authoritative. Regression coverage must confirm that select-only
execution preserves duplicates, blanks, row order, and provenance.

### `baho-cli`

Persists the selected parse and its bounded recognition evidence. Because this epic
adds token-role and candidate-parse evidence, `plan.json`'s plan-artifact wrapper
must advance from schema version 1 to version 2. The nested plan remains plan IR
schema version 1. Historical artifacts are not rewritten; a future reader must
handle plan-artifact versions explicitly. A review follow-up later advanced the
plan-artifact wrapper to version 3, adding competing-parse token spans and
modifier assignment to recognition evidence.

## Recognition evidence and diagnostics

Plan-artifact version 2 records enough bounded evidence to explain the decision:

- normalized prompt tokens with their original token indices;
- the action alias and token span;
- the optional modifier alias and token span;
- the selected column phrase and token span;
- the stable column ID and display name;
- match class (`exact` or `terminal_s_variant`) and score;
- canonical operation (`select` or `distinct`);
- bounded competing parses — each recorded with its column span and modifier
  assignment — when ambiguity causes refusal; and
- a stable refusal reason when no plan is produced.

Evidence ordering is deterministic. It must not contain source rows, environment
data, or unbounded prompt-derived collections.

Stable diagnostics include:

- `intent.unsupported` when no supported initial action or grammar is found;
- `intent.column_not_found` when no complete column span matches;
- `intent.column_ambiguous` when the selected span matches multiple headers;
- `intent.parse_ambiguous` when multiple complete semantic parses remain tied; and
- `plan.invalid` if compilation produces a plan rejected by existing validators.

Diagnostic messages should mention the relevant bounded phrase and supported
expectation, while tests assert codes and evidence rather than prose.

## Fixture and test strategy

Do not use the source document from run `000003` as a fixture. Reuse or extend a
small synthetic CSV with neutral headers and values. Add an intent-only column set
containing `Income`, `Time`, and `Show Time` for collision tests.

### Recognizer tests

- Every action alias maps to the same canonical retrieval action.
- `unique`, `distinct`, `deduplicate`, and `deduplicated` set the distinct modifier.
- `List income` recognizes select-only retrieval of `Income`.
- `List incomes` recognizes the singular header `Income` through the terminal-`s`
  variant.
- `List unique income` and `Show distinct incomes` recognize distinct retrieval.
- `Show time` selects `Time` when `Time` and `Show Time` both exist.
- `List Show Time` and `Show Show Time` select `Show Time`.
- `Show time` does not partially select `Show Time` when no `Time` column exists.
- No token index appears in more than one semantic role.
- Unsupported actions, missing column spans, duplicate normalized headers, and tied
  parses produce their stable diagnostic paths.
- Words that already end in `s` are not corrupted by plural normalization.

### Plan compilation and execution tests

- Plain retrieval compiles to exactly one `select` step.
- Select-only execution preserves duplicates, blank values, source order, and
  column-aligned provenance.
- Distinct retrieval compiles to the existing ordered
  `filter`/`select`/`distinct` steps.
- Existing run-`000003` behavior remains unchanged for
  `Extract all the unique floor plans`.

### CLI artifact tests

- A select-only run materializes successfully and writes a one-step plan.
- A distinct run retains its three-step plan.
- Plan-artifact schema version 2 records token spans and canonical operation.
- Ambiguous and unsupported prompts finalize failure manifests and diagnostics
  without writing misleading output.

## Acceptance criteria

1. `List income` resolves an unambiguous `Income` column and materializes a
   select-only result.
2. `List incomes` resolves the same `Income` column through the documented
   terminal-`s` equivalence.
3. Select-only retrieval preserves blank values, duplicates, source order, and
   provenance.
4. `List unique income` and its supported aliases retain the explicit
   filter/select/distinct semantics from Epic 001.
5. Retrieval and modifier synonyms are centralized aliases mapped to canonical
   semantics rather than separate executor behaviors.
6. A prompt token is never assigned to more than one semantic role.
7. `Show time`, `List Show Time`, and `Show Show Time` behave according to the
   collision table above.
8. A partial phrase does not silently select a longer column name merely because it
   reaches a one-sided overlap threshold.
9. Unsupported, missing, and ambiguous interpretations produce stable diagnostics
   and no materialized result.
10. Recognition evidence records bounded token roles, match class, canonical
    operation, and relevant alternatives deterministically.
11. The nested plan remains plan IR version 1; the changed plan-artifact envelope is
    versioned explicitly and historical runs are not rewritten.
12. The original source and `.baho/` artifacts remain untracked.
13. Focused tests and the workspace baseline pass:

```text
cargo fmt --check
cargo check --workspace
cargo test --workspace
```

## Implementation outline

1. Define canonical retrieval semantics and typed optional modifiers in
   `baho-core`.
2. Replace positional string checks with centralized action and modifier alias
   lookup while retaining an initial-action grammar.
3. Introduce indexed normalized tokens, non-overlapping role spans, and complete
   candidate-parse construction.
4. Replace one-sided token-overlap column scoring with exact contiguous phrase and
   conservative terminal-`s` matching.
5. Add deterministic candidate ranking and the `intent.parse_ambiguous` refusal
   path.
6. Extract intent-to-plan compilation and emit either select-only or
   filter/select/distinct plans.
7. Extend bounded recognition evidence and advance the plan-artifact wrapper to
   schema version 2 without changing plan IR version 1.
8. Add layered recognizer, compiler, executor, orchestration, and CLI artifact
   tests using synthetic data.
9. Run the workspace baseline and locally re-run the baseline prompt to verify that
   run `000003` behavior remains logically unchanged in a newly allocated run.

## Alternatives considered

### Add more words to the existing arrays

Rejected as the complete solution. It makes aliases broader but leaves the distinct
modifier mandatory, keeps plan generation hard-coded, and cannot represent
select-only retrieval.

### Search the entire prompt for any column name first

Rejected. Control words can legitimately occur inside headers. Global matching can
reuse `show` as both an action and part of `Show Time`, or prefer `Show Time` over an
exact residual `Time` column. It does not produce a coherent parse with exclusive
token roles.

### Remove all recognized control and filler words, then match what remains

Rejected. A header can itself contain a word that is also an action, modifier, or
filler. Deleting such words globally corrupts column phrases. Words are consumed
only through a valid grammar role at a specific position.

### Keep the current partial token-overlap score

Rejected for this slice. Its denominator considers only the header length, allowing
`time` to match `Show Time` at the current threshold. Exact bidirectional phrase
coverage plus a documented inflection rule is easier to explain and safer to
extend.

### Treat plain `List income` as implicitly nonblank

Rejected. A plain projection should preserve source data. Blank exclusion changes
row semantics and belongs in an explicit recognized modifier and plan step. The
existing distinct template continues to exclude blanks for compatibility with Epic
001.

### Use general stemming or an English inflection library

Deferred. It broadens behavior beyond the demonstrated `income`/`incomes` need and
can create surprising matches for domain headers. New morphology rules should be
introduced deliberately with collision tests.

### Add an LLM planner now

Deferred. These examples have small deterministic grammars and useful ambiguity
rules. An eventual LLM planner may propose the same typed intent or plan, but its
output must pass the same validators and executor.

## Open questions for review

1. Should `display` and `return` ship in the first alias expansion, or should Epic
   002 initially retain only the five existing action words while establishing the
   alias mechanism?
2. Are `deduplicate` and `deduplicated` desirable user-facing synonyms, or should
   the initial modifier vocabulary remain `unique` and `distinct`?
3. Should a future explicit `nonblank` modifier be included in this epic? The
   proposed scope intentionally preserves blanks for plain retrieval and supports
   nonblank filtering only as part of the existing distinct template.
4. Should quoted column references be the next disambiguation mechanism after this
   epic, allowing a future prompt such as `Show "Show Time"` without repeating an
   action word?
