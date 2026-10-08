# Role equality near the current grammar

Feature tag: `role_equality`

Generate requests to select whole matching rows using exactly one text equality
predicate. Each request must explicitly name the column `Role` and one literal
value from the supplied dataset profile. The goal of this batch is to probe the
edges of a small closed grammar, so stay close to terse, form-like phrasing that a
spreadsheet user would type. Do not write polite lead-ins, first-person
statements, or trailing requests such as "please".

Every request must begin with the column name or with one of the words `show`,
`list`, or `filter`, followed by an optional `rows where`, then `Role`, one
comparison, and the value. Use these comparison forms, spread across the batch:
the symbol `=`, the word `is`, and the word `equals`.

Vary exactly one of the following per case, keeping everything else minimal:

- the opening form (`Role = X`, `Show rows where Role = X`, `List rows where
  Role is X`, `Filter rows where Role equals X`);
- the value's quoting: none, double quotes, or single quotes;
- trailing punctuation after the value: none, a period, a question mark, or an
  exclamation mark;
- the letter case of the value or of the word `Role`.

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
| Role = Analyst | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |
| Show rows where Role is Analyst | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |
| List rows where Role equals "Coordinator" | `SELECT _row FROM data WHERE lower("Role") = lower('Coordinator') ORDER BY _row` |
| Filter rows where Role = 'Analyst'. | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |

Keep each request to this one row filter. Exclude totals, counts, aggregation,
selected-column projections, distinct values, sorting, limits, additional columns
or predicates, Boolean combinations, negation, ranges, null checks, partial text
matching, and implicit or header-first column references. Do not ask for only a
column's values or mention other source fields.
