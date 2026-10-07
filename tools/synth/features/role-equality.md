# Explicit Role equality

Feature tag: `role_equality`

Generate requests to select whole matching rows using exactly one text equality
predicate. Each request must explicitly name the column `Role` and one literal
value from the supplied dataset profile. Ask for rows or records, so the intended
result is unambiguous.

Allowed phrasing includes “show rows where Role is …”, “keep records with Role
equal to …”, “filter rows by Role = …”, and “return rows whose Role matches …”.
Vary the verb, word order, punctuation, quoting, and letter case while keeping the
same meaning. “Matches” means full text equality here, never substring or pattern
matching.

Baho compares text case insensitively by lowercasing both operands. These cases
use ASCII values, for which DuckDB's `lower` agrees. Do not trim whitespace,
normalize punctuation, infer synonyms, or change the literal except for letter
case. The prompt and SQL must use the same literal under this lowercase policy.

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
| Show rows where Role is Analyst. | `SELECT _row FROM data WHERE lower("Role") = lower('Analyst') ORDER BY _row` |
| Keep records with Role equal to "Coordinator". | `SELECT _row FROM data WHERE lower("Role") = lower('Coordinator') ORDER BY _row` |
| For Role = 'ANALYST', return the matching rows. | `SELECT _row FROM data WHERE lower("Role") = lower('ANALYST') ORDER BY _row` |
| Return rows whose Role equals "Lead's aide". | `SELECT _row FROM data WHERE lower("Role") = lower('Lead''s aide') ORDER BY _row` |

Keep each request to this one row filter. Exclude totals, counts, aggregation,
selected-column projections, distinct values, sorting, limits, additional columns
or predicates, Boolean combinations, negation, ranges, null checks, partial text
matching, and implicit or header-first column references. Do not ask for only a
column's values or mention other source fields.
