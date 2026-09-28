---
title: SQL
---

# SQL

SQL is SQLite's dialect, run over a mirror of the files: one SQL table per
governed table, one column per schema column. Queries read the mirror; a change
is computed there and then written to the files through the one validated
transaction path every change takes.

## Queries

```console
$ reldir sql 'SELECT u.name, p.title FROM users u JOIN posts p ON p.user_id = u.id ORDER BY u.name'
name  | title
------+----------
Ada   | Engines
Grace | Compilers
(2 rows)
2 row(s)
```

Anything SQLite can compute in a `SELECT` is available: joins, subqueries,
common table expressions, window functions, aggregates, and the JSON functions.
A column holding an array or object is JSON text, so `json_each` expands it and
`json_extract` reaches inside:

```console
$ reldir sql "SELECT p.id, t.value AS tag FROM posts p, json_each(p.tags) t ORDER BY p.id, tag"
id | tag
---+----------
p1 | history
p1 | machines
p2 | languages
(3 rows)
3 row(s)
```

Each column has its type's representation: an `int` is an integer, a `bool` is
`0`/`1` in SQL and `true`/`false` in the results, a `timestamp` is compared as
an instant, a `decimal` is compared numerically and exactly. A column a row
omits reads as its default, or `NULL`.

`reldir 'SELECT ...'` is shorthand for `reldir sql 'SELECT ...'`; `reldir sql -`
and a bare `reldir` with SQL on its standard input read the statement from stdin.

## Parameters

Values are bound, never spliced into the text. `--param` gives the next `?`, or
a named `:name` with `name=value`; a value is JSON, or text when it is not JSON:

```console
$ reldir sql 'SELECT id FROM users WHERE name = ?' --param Grace
id
-----
grace
(1 row)
1 row(s)
```

## Changes

`INSERT` (including `ON CONFLICT ... DO UPDATE`), `UPDATE` and `DELETE`, each
with `RETURNING`:

```console
$ reldir sql "UPDATE posts SET title = 'Analytical Engines' WHERE id = 'p1' RETURNING id, title"
id | title
---+-------------------
p1 | Analytical Engines
(1 row)

action | path          | planned
-------+---------------+--------
write  | posts/p1.json | false
(1 row)
committed: 1 file(s), revision 2
```

A statement's effect is computed under the writer lock, from the state it will
be committed to, and then completed with the referential actions its foreign
keys declare; the whole resulting state is validated before anything is
written. A statement that would leave any row invalid changes nothing and says
why, located in the file that would break:

```console
$ reldir sql "DELETE FROM users WHERE id = 'ada'"
error[FOREIGN_KEY_VIOLATION]: deleting users/ada.json refused: posts/p1.json still references it at /user_id (posts.user_id -> users, onDelete restrict)
  --> posts/p1.json:3:14
[..]
```

Only the files a statement changes are written, and each keeps its members
where they were. `--dry-run` reports the files a statement would change, fully
validated, and writes nothing.

## What a statement may do

One statement per invocation. The surface is enforced by SQLite's authorizer as
the statement is compiled, not guessed from its text:

- read governed tables, and the `json_each` / `json_tree` functions;
- `INSERT`, `UPDATE`, `DELETE` on governed tables, with `ON CONFLICT` and
  `RETURNING`;
- nothing else: no reading reldir's own tables, no `PRAGMA`, no `ATTACH`, no
  `CREATE`/`DROP`/`ALTER` (the structure of the database is its schemas; see
  [migrate]({{ site.baseurl }}/cli#migrate)), no transaction control.

A refused statement is `QUERY_UNSUPPORTED`, naming what was refused; a
statement that does not parse is located to its line and column.

## Plans

```console
$ reldir explain "SELECT * FROM users WHERE id = 'ada'"
id | parent | detail
---+--------+[..]
[..]
```

`reldir explain` shows SQLite's plan without running the statement. Declared
`indexes` and every candidate key are indexed; correctness never depends on an
index, only speed.

## Limits

A query is bounded by the [configured]({{ site.baseurl }}/configuration) result rows, memory
(sorting included) and optional timeout. Exceeding one is `RESOURCE_LIMIT`,
never a truncated answer. Results stream: JSON Lines and CSV rows are written
as they are produced.

## The shell

`reldir` with no arguments on a terminal opens a shell: SQL statements end in
`;`, and `.tables`, `.describe <table>`, `.schema <table>`, `.check`, `.status`
and `.quit` do what they say.
