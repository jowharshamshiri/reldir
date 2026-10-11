---
title: SQL
---

# SQL

SQL is SQLite's dialect, run over the mirror: one SQL table per governed table,
one column per schema column, every value held so that SQL compares it as its
type means. A query reads the mirror. A change is computed there, completed
with its referential actions, judged as the state it would make, and only then
written to the files -- by the one validated path every change takes.

The examples on this page run against a folder of `users`, `posts`, `tags` and
`orders`. An order has a generated integer key, a `decimal` total, a
`timestamp` written with its offset, and an optional `note` that may be null:

```console
$ reldir describe orders
[..]
name    | type      | nullable | required | default | generated | description
--------+-----------+----------+----------+---------+-----------+------------
id      | int       | false    | true     | NULL    | sequence  | NULL
user_id | string    | false    | true     | NULL    | NULL      | NULL
total   | decimal   | false    | true     | NULL    | NULL      | NULL
placed  | timestamp | false    | true     | NULL    | NULL      | NULL
note    | string    | true     | false    | NULL    | NULL      | NULL
(5 rows)
[..]
```

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

Anything SQLite computes in a `SELECT` is available: joins, subqueries,
common table expressions, window functions, aggregates and compound queries. A
column holding an array or an object is its JSON text, so `json_each` expands
it and `json_extract` reaches inside:

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

`reldir 'SELECT ...'` is shorthand for `reldir sql 'SELECT ...'`, and
`reldir sql -` reads the statement from standard input. `reldir sql` with
`--allow-invalid` answers from a database that has faults, reporting them
first; without it, a query of an invalid database is refused.

## Parameters

Values are bound, never spliced into the statement. `--param VALUE` binds the
next `?`; `--param name=VALUE` binds `:name`. A value is always JSON -- text is
quoted, numbers, `true`, `false` and `null` are bare -- so `7` and `"7"` are
never confused:

```console
$ reldir sql 'SELECT id FROM users WHERE name = ?' --param '"Grace"'
id
-----
grace
(1 row)
1 row(s)
$ reldir sql 'SELECT total FROM orders WHERE user_id = :who ORDER BY id' --param 'who="ada"'
total
-----
19.99
120
(2 rows)
2 row(s)
```

The [Python]({{ site.baseurl }}/python), [Node]({{ site.baseurl }}/node) and
[Rust]({{ site.baseurl }}/rust) libraries bind their own values the same way:
a sequence for `?`, a mapping for `:name`.

## Types in SQL

Each column type is held so that comparison, ordering and arithmetic mean what
the type means, and a value is returned as the file spells it:

| Type | In SQL | Compares |
|---|---|---|
| `bool` | `0` / `1`, returned as `true` / `false` | as a boolean |
| `int` | a 64-bit integer | as an integer |
| `integer` | the digits, as written | as a number of any size, exactly |
| `float`, `number` | a double, or an integer | as SQLite compares numbers |
| `decimal` | the text, as written | as a number, exactly |
| `timestamp` | the text, offset and all | as an instant |
| `date`, `uuid`, `ulid`, `enum`, `bytes`, `string` | the text | as text, byte for byte |
| `array`, `object`, `json` | JSON text, as written | as text; reach in with the JSON functions |

A `decimal` or `integer` column compares a literal or a parameter as a number
-- `total > 10` is a numeric comparison, not a text one -- and orders exactly,
at any size:

```console
$ reldir sql 'SELECT id, total FROM orders WHERE total > 10 ORDER BY total'
id | total
---+------
1  | 19.99
3  | 120
(2 rows)
2 row(s)
```

SQLite's own arithmetic is binary floating point. For exact decimal arithmetic
use `dec_add(a, b)`, `dec_sub(a, b)`, `dec_mul(a, b)`, the aggregate
`dec_sum(x)`, and `dec_cmp(a, b)` (`-1`, `0` or `1`); each takes numbers or
their text and returns the exact result as text:

```console
$ reldir sql "SELECT dec_sum(total) AS revenue, dec_add('0.1', '0.2') AS exact, 0.1 + 0.2 AS binary FROM orders"
revenue | exact | binary
--------+-------+--------------------
145.49  | 0.3   | 0.30000000000000004
(1 row)
1 row(s)
```

A `timestamp` keeps the offset it was written with, and is compared and
ordered as the instant it names, so `10:00:00+02:00` equals `08:00:00Z`:

```console
$ reldir sql 'SELECT id, placed FROM orders ORDER BY placed'
id | placed
---+--------------------------
1  | 2024-03-01T10:00:00+02:00
2  | 2024-03-01T08:30:00Z
3  | 2024-03-02T09:15:00-05:00
(3 rows)
3 row(s)
$ reldir sql "SELECT id FROM orders WHERE placed = '2024-03-01T08:00:00Z'"
id
--
1
(1 row)
1 row(s)
```

Strings compare exactly as written, so `café` written with a combining accent
is not `café` written precomposed. `nfc(text)` returns the Unicode
normalization form C of a string, and the collation `RELDIR_NFC` compares two
strings as their NFC forms: `WHERE nfc(name) = nfc(?)`, or
`ORDER BY name COLLATE RELDIR_NFC`. Keys compare as written; see
[schemas]({{ site.baseurl }}/schemas) for how names that differ only by case or
Unicode form are reported.

## Changes

`INSERT` (with `ON CONFLICT ... DO NOTHING | DO UPDATE` and `REPLACE`),
`UPDATE` and `DELETE`, each with `RETURNING`:

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
committed: 1 file(s), revision #2 ([..])
```

A statement writes only what it changes. Every member whose value it leaves
as it was keeps its exact bytes -- its spelling, its place, its absence -- and
a changed value is written in the file's own style. A value the statement
assigns that is equal under its type -- the same instant in another offset,
the same number with other digits -- is no change, and the file keeps its
spelling.

### Generated columns and RETURNING

A column the schema generates (`uuid`, `ulid`, `now`, `sequence`) is filled
when an `INSERT` leaves it out, and `RETURNING` sees the value the file will
hold:

```console
$ reldir sql "INSERT INTO orders (user_id, total, placed) VALUES ('grace', '7.25', '2024-03-03T12:00:00+01:00') RETURNING id, total"
id | total
---+------
4  | 7.25
(1 row)

action | path          | planned
-------+---------------+--------
write  | orders/4.json | false
(1 row)
committed: 1 file(s), revision #3 ([..])
$ cat orders/4.json
{
  "id": 4,
  "user_id": "grace",
  "total": "7.25",
  "placed": "2024-03-03T12:00:00+01:00"
}
```

An `INSERT` that names a generated column gives it the value it names; an
explicit `NULL` is an explicit null, not a request to generate one.

### NULL and absent

A JSON row has two ways to hold nothing -- a member set to `null`, and no
member at all -- and SQL has one `NULL`. In a query, both read as `NULL`. In a
change, `NULL` is read through the column's contract:

- where the column admits null, `NULL` writes `null`;
- where it does not, but the column is optional, `NULL` removes the member;
- where it is required, `NULL` writes `null`, and validation refuses it.

`absent()` removes a member explicitly, whatever the column admits; a required
member removed is refused like any other fault. An `INSERT` writes the members
it names and the values generated for it -- a column it does not name stays
absent, never filled with a default.

```console
$ reldir sql 'UPDATE orders SET note = NULL WHERE id = 3'
[..]
$ cat orders/3.json
{
  "id": 3,
  "user_id": "ada",
  "total": "120",
  "placed": "2024-03-02T09:15:00-05:00",
  "note": null
}
$ reldir sql 'UPDATE orders SET note = absent() WHERE id = 3'
[..]
$ cat orders/3.json
{
  "id": 3,
  "user_id": "ada",
  "total": "120",
  "placed": "2024-03-02T09:15:00-05:00"
}
$ reldir sql 'UPDATE orders SET user_id = NULL WHERE id = 3'
error[NOT_NULL_VIOLATION]: the change was refused because the database would be invalid (1 fault); nothing was written. First: "user_id" cannot be null
  --> orders/3.json:3:14
[..]
```

A value a table's [contract]({{ site.baseurl }}/concepts#the-observed-contract) has never admitted --
a type its files never held, a null where none was ever written -- is refused
with `CONTRACT_WIDENING_REQUIRED`, which says what widening would admit it;
`--widen` widens the contract in the same change.

### Referential actions

A statement's effect is completed with the referential actions its foreign
keys declare, at the end of the statement, and then the whole resulting state
is judged. Each action is reported:

```console
$ reldir sql "DELETE FROM tags WHERE id = 'history'"
remove posts/p1.json (posts.tags[] -> tags of ["history"] in tags/history.json)
action | path              | planned
-------+-------------------+--------
delete | tags/history.json | false
write  | posts/p1.json     | false
(2 rows)
committed: 2 file(s), revision #6 ([..])
```

A statement that would leave any row invalid changes nothing and says why,
located in the file that would break:

```console
$ reldir sql "DELETE FROM users WHERE id = 'ada'"
error[FOREIGN_KEY_VIOLATION]: [..]
[..]
```

`restrict` is judged at once; `no_action` when the statement is done, so a
statement that moves a reference out of the way before removing its target is
admitted. Unique keys are checked as each row is written, as in SQL: swapping
two rows' keys takes a script of statements, not one `UPDATE`.

`--dry-run` plans, completes and judges a change exactly as a commit would,
reports every file it would write, and writes nothing.

## Scripts and transactions

Several statements separated by `;` -- given on the command line, read from a
file with `-f FILE`, or from standard input with `-` -- are one transaction:

- each statement sees what the statements before it did;
- referential actions run at the end of each statement;
- the state they make together is judged once, when the script commits, so a
  statement may break a rule in passing as long as a later one mends it;
- nothing is written unless every statement succeeds and that state is valid,
  and the whole script is one revision.

```console
$ reldir sql "INSERT INTO posts (id, user_id, title) VALUES ('p3', 'alan', 'Computable Numbers'); INSERT INTO users (id, name) VALUES ('alan', 'Alan'); SELECT u.name, p.title FROM posts p JOIN users u ON u.id = p.user_id WHERE p.id = 'p3'"
name | title
-----+-------------------
Alan | Computable Numbers
(1 row)

action | path            | planned
-------+-----------------+--------
write  | posts/p3.json   | false
write  | users/alan.json | false
(2 rows)
committed: 2 file(s), revision #7 ([..])
```

The post names a user the next statement inserts: judged at the end, the
reference holds. A `;` inside a string, a quoted name or a comment never ends
a statement -- statements are cut where SQLite itself would end them.

A script may be framed by `BEGIN` at its start and `COMMIT` (or `END`) or
`ROLLBACK` at its end. `ROLLBACK` runs every statement, shows what they
return, and writes nothing:

```console
$ reldir sql 'BEGIN; DELETE FROM orders; SELECT count(*) AS left FROM orders; ROLLBACK'
left
----
0
(1 row)
2 statement(s) ran and were rolled back; nothing was written
$ ls orders
1.json
2.json
3.json
4.json
```

Transaction control anywhere else -- a `BEGIN` without its `COMMIT`, a
`COMMIT` in the middle, `SAVEPOINT` -- is refused with `TRANSACTION_STATEMENT`
before anything runs. `--param` binds the values of one statement; a script's
statements carry their own. A transaction holds the writer lock from its first
statement to its commit, for at most `transaction_timeout_seconds`; past that
it is rolled back with `TRANSACTION_EXPIRED`. See
[transactions]({{ site.baseurl }}/transactions) for how a commit is made.

## What a statement may do

The surface is enforced by SQLite's authorizer as each statement is compiled,
not guessed from its text:

- a query reads governed tables, and the `json_each` and `json_tree`
  table-valued functions;
- a change also writes governed tables;
- nothing may read reldir's own tables, run a `PRAGMA`, `ATTACH` a database,
  `CREATE`, `DROP` or `ALTER` anything -- the structure of the database is its
  schemas; see [migrations]({{ site.baseurl }}/migrations).

```console
$ reldir sql 'CREATE TABLE notes (id TEXT)'
error[QUERY_UNSUPPORTED]: CREATE is not supported: SQL here reads and changes rows; the structure of the database is its schemas
   = help: change a schema with `reldir migrate`, or by editing its file
```

A refused statement is `QUERY_UNSUPPORTED`, naming what was refused. A
statement that does not parse is located to its line and column; one that
names a table or column the database does not have is `UNKNOWN_TABLE` or
`UNKNOWN_COLUMN`, with the nearest name it might mean.

## Plans

`reldir explain` shows SQLite's plan for a statement without running it:

```console
$ reldir explain "SELECT * FROM users WHERE id = 'ada'"
id | parent | detail
---+--------+[..]
[..]
```

Every candidate key, every foreign key's source columns and every column list
in `x-reldir.indexes` is indexed in the mirror. Correctness never depends on
an index, only speed. The planner's statistics are gathered when a table is
first mirrored and again whenever it grows or shrinks twofold, and
`reldir analyze` gathers them on demand; see
[performance]({{ site.baseurl }}/performance).

## Limits

A query is bounded by the configured result rows (`max_result_rows`), the
memory it may use, sorting included (`max_query_memory`), and an optional
time (`timeout_seconds`, or `--timeout`, which takes fractions). Past one, the
statement stops with `RESOURCE_LIMIT`: a query never returns a truncated
answer as if it were whole. See [configuration]({{ site.baseurl }}/configuration).

## The shell

`reldir` with no arguments on a terminal opens a shell. A statement runs when
SQLite would say it is complete -- at a `;` outside any string or comment -- so
it may span lines. Tab completes tables, columns, keywords and dot-commands,
and history is kept in `$XDG_STATE_HOME/reldir/history`.

Each statement is its own transaction, until `BEGIN;` or `.begin` opens one:
then every statement after it is part of one change -- each sees what the
others did -- until `COMMIT;` or `.commit` writes them as one revision, or
`ROLLBACK;` or `.rollback` discards them. The prompt reads `reldir*>` while a
transaction is open. A statement that fails inside it rolls the whole
transaction back, and leaving the shell with one open rolls it back too.

The other dot-commands are `.tables`, `.describe TABLE`, `.schema TABLE`,
`.status`, `.check`, `.log`, `.param VALUE ...` (the values for the `?` of the
statements that follow; `.param clear` forgets them), `.explain on|off`,
`.timer on|off`, `.format table|json|jsonl|csv`, `.help` and `.quit` (or
`.exit`, or Ctrl-D).
