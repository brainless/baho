# Epic 008: Filter Value Normalization

Status: Implemented (decisions locked 2026-09-24; all tasks complete 2026-09-24)

Motivating run: None supplied. These requests follow a successful manual test
of Epic 006 using the CSV CLI.

User intent: Make numeric filters understand comma and dot formats found in a
column, and make text equality insensitive to letter case.

## Summary

Improve the value comparison path introduced by Epic 006. A numeric comparison
should parse supported formats found in the selected column under an explicit,
recorded policy. A text equality comparison should match values that differ
only in letter case. Raw source cells, prompt literals, coordinates, and
diagnostics remain available as evidence.

This epic was a collection point for limitations found while testing Epic 006.
Scope and decisions are now locked; implementation proceeds as the tasks below.

## Observed behavior and problem

Epic 006 deliberately uses `StrictDecimal`: an optional sign, ASCII digits,
and an optional dot fractional part. A comma in a number is rejected. If enough
cells in a compared column use an unsupported format, numeric comparison can
refuse the column as `parse.column_mixed`. The current text equality check
compares the source string and literal exactly, including case. For example,
`Status = inactive` does not match a source cell containing `Inactive`.

The successful manual test has no supplied run ID or source document, so these
observations are based on the Epic 006 contract and current code, not on a
specific run's diagnostics.

## Requested behavior

### Numeric formats

- Detect supported comma and dot number formats from evidence in the compared
  column, then compare their interpreted values as exact decimals.
- Record the selected parsing policy and its supporting evidence in the run.
- Preserve the raw cell value and source coordinate in the materialized result
  and diagnostics.
- Refuse or request an explicit format when the column does not support a
  unique interpretation. For example, `1,234` could be a grouped integer or
  a decimal in different conventions; `1.234` has the same ambiguity.
- Define how prompt literals such as `1,000` are interpreted alongside the
  column policy. Never silently reinterpret an ambiguous threshold.

A dependency is an implementation option, not a design decision. Locked
decision 12 extends the existing exact-decimal parser with explicit policies
instead of adopting a crate. If a crate is ever reconsidered, its local source
must be inspected first, per `AGENTS.md`.

### Case-insensitive text equality

- Compare text values and text literals without a difference in letter case
  affecting `=` or `!=` results. For example, `Status = inactive` should
  match `Inactive`.
- Apply the same comparison policy to both sides. Preserve their original
  spelling in the prompt evidence and source/output cells.
- Do not conflate case normalization with trimming, accent removal, fuzzy
  matching, or spelling correction; those remain separate decisions (see
  Non-goals).

## Locked decisions

1. **Resolved:** Text `=` and `!=` use Unicode full lowercase conversion
   (`str::to_lowercase`) applied to both the source text and the literal, then
   compare the folded strings for exact equality. This is deterministic
   std-only Unicode simple mapping, not locale-aware (Turkish dotted-I stays
   simple), and not Unicode case folding (`ß` does not equal `ss`). Ordered
   text comparisons remain rejected by plan structural validation. The same
   fold applies to the mixed-column text path that string-compares malformed
   cells. Blank and missing cells keep their existing `unknown` behavior.
2. **Resolved:** The text-match policy is derived from the plan schema version,
   not a per-predicate field. Plan schema version 3 defines text `=`/`!=` as
   case-insensitive under decision 1. Plan schema versions 1 and 2 retain
   exact case-sensitive equality. `baho-exec` derives the policy from
   `Plan::schema_version` (a helper such as
   `Plan::text_match_policy()` should own the mapping). Version 1 and 2 plans
   remain readable and executable with their existing semantics.
3. **Resolved:** Recognition evidence records the text-match policy as
   `unicode_lowercase` for row-filter evidence compiled under plan schema
   version 3. `RowFilterEvidence::PLAN_SCHEMA_VERSION` moves from 2 to 3.
4. **Resolved:** Numeric parsing gains two separator policies alongside
   `StrictDecimal`, serialized as `dot_decimal_comma_grouping` (`1,234.56`;
   `.` marks decimals, `,` groups) and `comma_decimal_dot_grouping`
   (`1.234,56`; `,` marks decimals, `.` groups). `StrictDecimal` keeps its
   existing grammar and serialization.
5. **Resolved:** Per-value separator-shape classification (after an optional
   sign) is:
   - no `.` or `,` → neutral (compatible with every policy);
   - both `.` and `,` → the later separator is the decimal mark, the earlier
     one is the grouping mark (decided);
   - one separator occurring more than once → it is the grouping mark
     (decided);
   - one separator occurring once with a trailing group whose length is not
     exactly 3 → it is the decimal mark (decided);
   - one separator occurring once with a trailing group of exactly 3 digits
     (and any earlier groups of exactly 3) → undecided (grouping or decimal).
6. **Resolved:** Column policy selection merges per-value role assignments
   (decimal-mark or grouping-mark per separator character):
   - conflicting assignments (one character as both marks, or both characters
     as decimal marks) refuse with `parse.format_ambiguous`;
   - one consistent assignment selects the matching policy: decimal mark `.`
     with a comma grouping role present selects `dot_decimal_comma_grouping`;
     decimal mark `.` with no comma present selects `strict_decimal`; decimal
     mark `,` selects `comma_decimal_dot_grouping`; grouping-only assignments
     select the policy whose grouping mark matches;
   - undecided values are interpreted under the selected policy;
   - when there is no decided evidence and exactly one separator character
     occurs in the column and every occurrence is a well-formed run of
     3-digit groups, locked preference: that separator is the grouping mark
     (`10,000` = 10000, `1.234` = 1234);
   - when there is no decided evidence and both separator characters occur,
     refuse with `parse.format_ambiguous`.
7. **Resolved:** Prompt numeric literals inherit the bound column's selected
   policy. A format-shaped literal that is not a valid strict decimal is
   deferred during recognition and resolved under that policy before plan
   compilation. A literal that does not parse under the selected policy is
   refused with `intent.literal_invalid`, naming the policy. A deferred
   numeric literal bound to a non-numeric (text or blank) column is refused
   with `intent.literal_invalid`, preserving the existing public code for
   `List job = 10,000`. Structurally invalid numerics such as `1e5` and
   `1.2.3` (invalid grouping under every policy) still refuse at recognition
   as `intent.literal_invalid`. Against a `strict_decimal` column,
   `Annual Income < 10,000` therefore still refuses; users write `10000`.
8. **Resolved:** Policy selection runs before deferred literal resolution and
   before plan compilation. The orchestration order for row filters is:
   recognize with deferred numeric literals → for columns carrying deferred
   literals, profile and select each column's numeric policy → resolve the
   deferred literals under those policies → compile the plan → validate →
   typed-parse compared columns under their selected policies → execute.
   Typed parsing keeps the Epic 006 mixed-column threshold and diagnostics.
   Recognition evidence records each deferred literal's raw text, resolved
   decimal, and parser policy. Policy ambiguity on a deferred-literal column
   refuses with `parse.format_ambiguous` before literal resolution
   (locked decision 9).
9. **Resolved:** A column whose policy cannot be selected refuses with
   `parse.format_ambiguous` before typed parsing and before literal
   resolution. Genuinely mixed columns after policy selection keep the Epic
   006 `parse.column_mixed` refusal (Locked decision 5 of Epic 006: zero
   parseable nonblank cells, or malformed share above 10%). Malformed cells
   below that threshold evaluate to `unknown` as before.
10. **Resolved:** No CLI or parser-config format override ships in this slice.
    Ambiguous columns and literals refuse with diagnostics. An explicit
    user-supplied format setting is a later milestone.
11. **Resolved:** Whitespace trimming in text equality stays out of scope.
    `"unemployed "` continues not to match `unemployed`. This is an explicit
    non-goal of this epic and a candidate follow-up, not an accidental gap.
12. **Resolved:** Separator-aware parsing is implemented in `baho-model` on
    top of `ExactDecimal` by validating grouping structure and feeding the
    normalized digit string to the existing strict grammar. No new dependency
    is introduced. A new `DecimalParseError::InvalidGrouping` covers grouping
    structure violations; other refusals reuse the existing error variants.
13. **Resolved:** Diagnostic codes added by this epic are public behavior:

| Code | Meaning |
|---|---|
| `parse.format_ambiguous` | column has no unique numeric interpretation |
| `intent.literal_invalid` | (existing) literal invalid under the selected policy |
| `parse.value_malformed` | (existing) malformed compared-column values |
| `parse.column_mixed` | (existing) materially mixed compared-column values |

14. **Resolved:** Persisted-schema bumps required by this epic:
    `RECOGNITION_EVIDENCE_SCHEMA_VERSION` 2 → 3 (new policy value space and
    evidence fields) and `PLAN_ARTIFACT_SCHEMA_VERSION` 4 → 5 (the `plan.json`
    envelope carrying that evidence). `NumericParsePolicy` gains the two
    grouping variants from decision 4 wherever it is serialized. Task 6 owns
    the bumps and compatibility coverage.

## Numeric policy selection examples

| Column evidence | Selection | Values |
|---|---|---|
| `500`, `1000`, `2.5` | `strict_decimal` | 500, 1000, 2.5 |
| `50000`, `10,000`, `75000` | `dot_decimal_comma_grouping` (locked preference) | 50000, 10000, 75000 |
| `1,23`, `1,234` | `comma_decimal_dot_grouping` | 1.23, 1.234 |
| `1,234.56`, `12.34` | `dot_decimal_comma_grouping` | 1234.56, 12.34 |
| `1.234,56`, `12,34` | `comma_decimal_dot_grouping` | 1234.56, 12.34 |
| `1,234`, `1.56` | `dot_decimal_comma_grouping` | 1234, 1.56 |
| `1.234`, `1.56` | `strict_decimal` | 1.234, 1.56 |
| `1,234`, `1.234` | `parse.format_ambiguous` | — |
| `1,23`, `1,234,567` | `parse.format_ambiguous` | — |
| `1,23`, `1.56` | `parse.format_ambiguous` | — |
| `1.234`, `2.567` | `comma_decimal_dot_grouping` (locked preference) | 1234, 2567 |

## Known limitations

- A column whose separator-bearing values are all exactly-3-digit tails under
  a single separator (for example `1.234`, `2.567`) is read as grouped
  thousands under the locked preference in decision 6, not as three-decimal
  values. A single non-3-digit tail elsewhere in the column (for example
  `1.56`) forces the decimal interpretation. Explicit format overrides are
  out of scope (decision 10).
- Text equality does not trim outer whitespace (decision 11).
- Prompt literals do not invent a format the column did not evidence
  (decision 7): `10,000` against a `strict_decimal` column still refuses.
- No locale guessing, currency, percentage, exponent, or unit handling.

## Relevant architecture

- `baho-model` owns `ExactDecimal`, the extended `NumericParsePolicy` and
  `DecimalParseError`, separator-shape helpers, and `TextMatchPolicy` with its
  folding helper.
- `baho-ingest-csv` owns numeric policy selection from column evidence, typed
  cell parsing, and parse diagnostics including `parse.format_ambiguous`.
- `baho-core` owns prompt literal recognition, deferred numeric literal
  resolution under the bound column's policy, and parser orchestration.
- `baho-exec` owns text and numeric comparison semantics, deriving the
  text-match policy from the plan schema version.
- `baho-plan` owns plan schema version 3 and versioned literal and predicate
  contracts; it rejects any deferred literal shape in persisted plans.
- `baho-cli` records the run artifacts; it does not implement parsing rules.

Epic 007 currently proposes case-sensitive categorical value lookup. Its
lookup normalization must use the same `TextMatchPolicy` helper this epic
introduces so grounding and execution agree. That reconciliation is an Epic
007 acceptance dependency, not an implementation task of this epic.

## Alternatives and tradeoffs to review

- Infer one numeric policy per column (chosen), or require an explicit parser
  setting when multiple formats remain plausible (rejected for this slice;
  decision 10).
- Support only unambiguous grouping and decimal patterns first (chosen via
  decisions 5–6), or also support locale-specific patterns (rejected). Mixed
  conventions refuse with `parse.format_ambiguous`.
- Extend the existing exact-decimal parser with small explicit policies
  (chosen), or adopt a crate after checking that it preserves exactness
  (rejected for this slice; decision 12).
- Use lowercase conversion for simple case-insensitive matching (chosen), or
  Unicode case folding for broader equivalence (rejected; `ß`/`ss` stays
  distinct).
- Derive the text-match policy from the plan schema version (chosen), or add
  a per-predicate policy field (rejected; keeps v2 JSON compatible and keeps
  plans smaller).

## Non-goals

- Trimming or collapsing whitespace in text equality (see decision 11).
- Accent removal, fuzzy matching, stemming, or spelling correction.
- Unicode case folding, locale-aware casing, or language-specific rules.
- CLI flags or parser-config overrides for numeric formats (decision 10).
- Currency, percentage, exponent, or unit-aware parsing.
- Ordered text comparison (`<` on text remains rejected).

## Provisional acceptance criteria

1. A numeric filter accepts clearly supported comma/dot formats under a
   recorded policy and compares their exact decimal values.
2. Ambiguous or malformed numeric input is never silently guessed or coerced;
   diagnostics explain the column or literal that could not be interpreted.
3. Text `=` and `!=` use the reviewed case-insensitive policy, while output and
   provenance retain the original source text.
4. Numeric and text results remain deterministic for the same input, policy,
   and plan; existing three-valued behavior for blank and malformed cells is
   preserved.
5. Synthetic tests cover supported formats, ambiguous separators, mixed
   columns, case variants, and unchanged raw values. CLI run artifacts show
   the chosen policy and relevant diagnostics.
6. Plan schema versions 1 and 2 retain exact text equality; plan schema
   version 3 emits case-insensitive text equality and remains deterministic.
7. Existing Epic 001, 002, and 006 behavior is unchanged except where this
   epic explicitly replaces the grouped-literal refusal described in
   decision 7.

## Fixture and test strategy

Use small synthetic CSVs containing grouped integers, dot decimals, comma
decimals, ambiguous single-separator values, mixed formats, and text values
with case variants. Add focused parsing and executor tests, followed by a CLI
regression that checks the recorded policy, result rows, diagnostics, and raw
provenance. Do not copy a user document into the repository without approval.

Existing tests whose contract changes under decision 7 must be updated, not
deleted without replacement: `grouped_numeric_literals_refuse_before_typed_parsing`
and `grouped_numeric_literals_refuse_as_invalid` in `baho-core` (ordering and
outcome change for grouping-shaped literals that a column policy can explain),
`compact_grouped_literal_refuses_as_literal_invalid`, and
`text_comparison_is_exact_and_incompatible_cells_are_unknown` in `baho-exec`
(the case-variant row matches under plan schema version 3). Each keeps a
replacement that pins the new behavior and the preserved refusal cases
(`1e5`, `1.2.3`, grouped literals against strict columns).

## Implementation outline

1. Define the parsing and comparison policies (`TextMatchPolicy`, extended
   `NumericParsePolicy`, `InvalidGrouping`).
2. Write failing tests for policy selection, folding, and deferred literals.
3. Implement policy selection and exact parsing in the owning crates.
4. Align prompt literal handling with the selected column policy.
5. Record decisions in run artifacts and bump artifact schema versions.
6. Run focused tests and the workspace baseline.

## Open questions

- Resolved: which number formats the first implementation accepts — see
  decisions 4–6 and the selection examples.
- Resolved: how a numeric prompt literal selects a format — it inherits the
  bound column's selected policy (decision 7).
- Resolved: which Unicode case policy text equality uses — Unicode full
  lowercase conversion (decision 1).
- Resolved: case-insensitive behavior applies to explicit-column filter
  execution here; Epic 007 grounding must reuse the same helper.
- Remaining for a later slice: whitespace trimming in text equality, explicit
  format overrides, and Unicode case folding if real documents require it.

## Tasks

### Task 1: Text-match policy and plan schema version 3

Status: complete

Add `TextMatchPolicy` and its folding helper to `baho-model` per Locked
decisions 1–2. Add plan schema version 3 in `baho-plan`, derive the policy
from `Plan::schema_version`, accept version 3 in structural validation while
still rejecting other unknown versions, and keep versions 1–2 semantics
unchanged. Record the policy in recognition evidence and move
`RowFilterEvidence::PLAN_SCHEMA_VERSION` to 3 (Locked decision 3).

### Task 2: Case-insensitive text comparison execution

Status: complete (implemented in `baho-exec`; case folding verified by
executor regression tests)

Implement decision 1 in `baho-exec`: `=`/`!=` text comparisons fold both
sides with the policy derived from the plan schema version, including the
mixed-column string-compare path. Three-valued semantics and provenance are
unchanged. Update the executor text-comparison test per the fixture strategy
and pin version 1/2 exact-match behavior with version 3 case-insensitive
behavior.

### Task 3: Numeric separator policies and exact parsing

Status: complete

Implement Locked decisions 4, 5 (shape classification helpers), and 12 in
`baho-model`: the two grouping policy variants, separator-aware parsing on
top of `ExactDecimal`, and `DecimalParseError::InvalidGrouping`. Unit-test
the full decision-5 classification matrix and policy parse/refuse cases.
No policy selection here; that is Task 4.

### Task 4: Column policy selection and format diagnostics

Status: complete

Implement Locked decisions 5–6 and 9 in `baho-ingest-csv`: select a
`NumericParsePolicy` from column evidence with the locked preference rules,
refuse conflicts with a new `parse.format_ambiguous` diagnostic, record the
selected policy and supporting evidence, and run existing typed parsing and
the 10% mixed-column threshold under the selected policy. Cover the numeric
policy selection examples table.

### Task 5: Deferred prompt numeric literals under column policy

Status: complete

Implement Locked decisions 7–8 in `baho-core` (and a deferred-literal
contract in `baho-plan` that structural validation rejects in persisted
plans). Format-shaped non-strict numerics defer during recognition and
resolve under the bound column's selected policy before compilation;
refusals use `intent.literal_invalid` with the policy named. `1e5` and
`1.2.3` still refuse at recognition. Update the tests named in the fixture
strategy with replacement coverage.

### Task 6: Orchestration, run artifacts, and CLI regressions

Status: complete

Implement Locked decisions 8 and 14 across `baho-core` orchestration,
`baho-run`, and `baho-cli`: profile and select policies before typed parsing
and literal resolution, record policy and evidence in run artifacts, bump
`RECOGNITION_EVIDENCE_SCHEMA_VERSION` to 3 and `PLAN_ARTIFACT_SCHEMA_VERSION`
to 5 with compatibility coverage, and add end-to-end CLI regressions for
grouped/dot/comma columns, case-insensitive filters, `parse.format_ambiguous`,
and unchanged raw provenance. Keep the GUI a pass-through of core rules.
