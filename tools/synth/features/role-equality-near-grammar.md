# Role equality near the current grammar

Feature tag: `role_equality`

Generate requests to select whole matching rows using exactly one text equality
predicate. Each request must explicitly name the column `Role` and one literal
value from the supplied dataset profile. The goal of this batch is to probe the
edges of a small closed grammar, so stay close to terse, form-like phrasing that a
spreadsheet user would type. Do not write polite lead-ins, first-person
statements, or trailing requests such as "please".

Build controlled contrasts in submission order. Choose one ASCII profiled value
and one opening, `Show rows where Role`, and keep both identical throughout the
first group. The first three cases must differ only in the comparison:

1. `Show rows where Role = VALUE`
2. `Show rows where Role is VALUE`
3. `Show rows where Role equals VALUE`

All three use the exact same SQL. For count 1 or 2, submit that prefix of the
triplet. Complete the triplet before adding another probe. Do not rotate values
or openings to make the batch look varied.

Then keep the first case as the baseline and change just one factor per case:
double quotes around VALUE, single quotes around VALUE, trailing period,
trailing question mark, trailing exclamation mark, or letter case. In particular,
never combine single quotes with punctuation, or a word operator with changed
quoting. For larger batches, repeat a complete operator triplet with a second
profiled value or a different opening, changing only that factor from the first
group. The purpose is to attribute a mismatch to one grammar decision.

Baho compares text case insensitively by lowercasing both operands. These cases
use ASCII values, for which DuckDB's `lower` agrees. Do not trim whitespace,
normalize punctuation, infer synonyms, or change the literal except for letter
case. The prompt and SQL must use the same literal under this lowercase policy.
Quotes and trailing punctuation are not part of the literal: the intended value
is the profiled text alone.

Use exactly this SQL shape, with no semicolon or comments:

```sql
SELECT _row FROM data WHERE lower("Role") = lower('role value') ORDER BY _row
```

Replace `role value` with a profiled value. Escape an apostrophe inside a SQL
string literal by doubling it. `_row` is a source row identifier; the header is
row 0 and the first data row is 1. `ORDER BY _row` is required for deterministic
oracle output; the user prompt must not request sorting.

These examples use neutral synthetic values for illustration. Use them in a
generated case only if they appear in the supplied profile.

| Prompt | SQL |
| --- | --- |
| Show rows where Role = Analyst | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |
| Show rows where Role is Analyst | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |
| Show rows where Role equals Analyst | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |
| Show rows where Role = "Analyst" | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |
| Show rows where Role = 'Analyst' | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |
| Show rows where Role = Analyst. | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |

Keep each request to this one row filter. Exclude totals, counts, aggregation,
selected-column projections, distinct values, sorting, limits, additional columns
or predicates, Boolean combinations, negation, ranges, null checks, partial text
matching, and implicit or header-first column references. Do not ask for only a
column's values or mention other source fields.
