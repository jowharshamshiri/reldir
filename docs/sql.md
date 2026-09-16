---
title: SQL
---

# SQL

SQL is one frontend to the relational model. Queries run against the governed
directory, and mutations rewrite authoritative JSON through the same transaction
path every other write uses.

## Supported subset

```sql
SELECT   INSERT   UPDATE   DELETE

WHERE    INNER JOIN   LEFT JOIN
GROUP BY HAVING       ORDER BY
LIMIT    OFFSET       DISTINCT

COUNT    SUM    AVG    MIN    MAX
```

`EXPLAIN` is supported, along with SQLite-style scalar expressions. Exactly one
statement per invocation.

Anything outside the subset fails with `QUERY_UNSUPPORTED`, naming the
offending token with line and column, rather than being accepted and interpreted
differently than written.

## Queries

```console
$ reldir sql 'SELECT u.name, count(*) AS n
          FROM users u JOIN posts p ON p.user_id = u.id
          GROUP BY u.name ORDER BY n DESC LIMIT 3'
 name  | n
-------+---
 Alice | 41
 Bob   | 37
 Carol | 29
(3 rows)
```

Off a terminal the same query emits JSON Lines, one object per row:

```console
$ reldir sql 'SELECT name FROM users ORDER BY name' | head -2
{"name":"Alice","kind":"row"}
{"name":"Bob","kind":"row"}
```

Large result sets stream rather than buffering the whole set in memory.

SQLite's JSON functions are available, which is how a column holding an array of
references is checked. `json_each` expands one row per element:

```console
$ reldir sql 'SELECT b.id, e.value
          FROM blocks b, json_each(b.objective_refs) e
          WHERE e.value NOT IN (SELECT id FROM objectives)'
(0 rows)
```

No rows means every element resolves. See
[references from inside an array](schemas#references-from-inside-an-array) for
when to model this as its own table instead.

## Parameters

Parameters are JSON literals, typed by the column they bind to, and are always
kept separate from the SQL text:

```sh
reldir sql 'SELECT * FROM users WHERE id = ?'    --param '"u1"'
reldir sql 'SELECT * FROM users WHERE id = :id'  --param id='"u1"'
reldir sql 'SELECT * FROM items WHERE n > ?'     --param 42
```

Because a parameter is a JSON literal, `'"2"'` is the string `2` and `2` is the
number. Binding a string where an `int` column is expected is a `TYPE_MISMATCH`,
not a silent conversion.

## Mutations

```sh
reldir sql "UPDATE users SET name = 'Robert' WHERE id = 'u2'"
reldir sql "INSERT INTO users (id, name) VALUES ('u3', 'Carol')"
reldir sql "DELETE FROM users WHERE id = 'u3'"
```

Every mutation reports the files it changed:

```console
$ reldir sql "DELETE FROM users WHERE id = 'u1'"
changed 2 path(s); revision 4
  posts/p1.json
  users/u1.json
```

Mutations pass through full relational validation, execute declared referential
actions, and commit as a single transaction. A violated constraint aborts the
whole statement and changes nothing.

## Explain

```console
$ reldir sql --explain-analyze 'SELECT * FROM users' --format jsonl
{"kind":"query_plan","logical_plan":"SELECT * FROM users",
 "physical_plan":[{"detail":"SCAN users"}],
 "selected_indexes":[],"rejected_indexes":[],
 "estimated_rows_upper_bound":3,"actual_rows":3,"actual_elapsed_us":214}
```

The plan reports the logical and physical plans, which indexes were chosen,
which were rejected and why, and estimated rows. With `--explain-analyze` it also
reports measured rows and elapsed time. `reldir explain '<sql>'` produces the same
plan without executing the query.

Query correctness never depends on an index being present: indexes are derived
acceleration, and deleting them changes performance, not answers.

## Interactive shell

```console
$ reldir shell
.tables
users posts
.describe users
{ "table": "users", ... }
SELECT name FROM users ORDER BY name;
 name
-------
 Alice
 Bob
.quit
```

Supports history and completion, plus `.tables`, `.describe <table>`, `.status`,
and `.quit`/`.exit`.

## Resource limits

Queries are bounded by the configured limits: result rows, query memory, sort
memory, and an optional timeout. Exceeding one fails with `RESOURCE_LIMIT` rather
than returning a truncated answer. See [Configuration](configuration).
