---
title: How to change the shape of a table that holds rows
---

# How to change the shape of a table that holds rows

This guide shows how to rename, add, retype and constrain the columns of a
table that already holds rows, so that the schema and every row change
together, in one revision. Every operation, and what it does to schemas and
rows, is listed in the [migrations reference]({{ site.baseurl }}/migrations).

## Preview the change

Run the operation with `--dry-run` first. It plans and validates the whole
change and lists every file it would write:

```console
$ reldir --dry-run migrate rename-column posts title headline
description
-------------------------------------
rename column posts.title to headline
(1 row)

action | path              | planned
-------+-------------------+--------
write  | posts/p1.json     | true
write  | posts/p2.json     | true
write  | schema/posts.json | true
(3 rows)
dry run: 3 file(s) would change; nothing was written
```

If the list holds files you did not expect to change, stop and look at them
before going on.

## Make one change with a command

When the change is one operation, run its command without `--dry-run`:

```console
$ reldir migrate rename-column posts title headline
description
-------------------------------------
rename column posts.title to headline
(1 row)

action | path              | planned
-------+-------------------+--------
write  | posts/p1.json     | false
write  | posts/p2.json     | false
write  | schema/posts.json | false
(3 rows)
migrated: 3 file(s), revision #2 ([..])
$ cat posts/p1.json
{
  "id": "p1",
  "user_id": "ada",
  "headline": "Engines",
  "tags": [
    "machines",
    "history"
  ]
}
```

## Make several changes together with a migration file

When several operations must land together, or the same steps must run
against more than one database, write them in a migration file and apply it.
Keep migration files beside the database, not inside it -- a directory in the
database folder that no schema governs is reported as `UNGOVERNED_DIRECTORY`.
If they must live inside it, add their directory to `ignore` in
[`.db/config`]({{ site.baseurl }}/configuration#files).

Name each file so that the names sort in the order the files apply:

```console
$ mkdir ../migrations
$ echo '{"operations": [{"op": "add_column", "table": "posts", "column": "status", "type": "string", "default": "draft"}, {"op": "add_index", "table": "posts", "columns": ["status"]}]}' > ../migrations/0001-status.json
$ reldir migrate apply ../migrations/0001-status.json --dry-run
description
--------------------------------
add column posts.status (string)
index posts(status)
(2 rows)

action | path              | planned
-------+-------------------+--------
write  | posts/p1.json     | true
write  | posts/p2.json     | true
write  | schema/posts.json | true
(3 rows)
dry run: 3 file(s) would change; nothing was written
$ reldir migrate apply ../migrations/0001-status.json
description
--------------------------------
add column posts.status (string)
index posts(status)
(2 rows)

action | path              | planned
-------+-------------------+--------
write  | posts/p1.json     | false
write  | posts/p2.json     | false
write  | schema/posts.json | false
(3 rows)
migrated: 3 file(s), revision #3 ([..])
$ cat posts/p2.json
{
  "id": "p2",
  "user_id": "grace",
  "headline": "Compilers",
  "tags": [
    "languages"
  ],
  "status": "draft"
}
```

The whole file is one transaction: if any operation is refused, nothing is
written, and the error names the operation by its position.

## Narrow what a column admits

To restrict a column to known values, change its type to `enum` and list them:

```console
$ reldir migrate change-type posts status enum --values draft,published
description
---------------------------
change posts.status to enum
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/posts.json | false
(1 row)
migrated: 1 file(s), revision #4 ([..])
$ reldir sql "UPDATE posts SET status = 'archived' WHERE id = 'p1'"
error[SCHEMA_VIOLATION]: the change was refused because the database would be invalid (1 fault); nothing was written. First: the value at /status: "archived" is not one of "draft" or "published"
[..]
```

To convert the values themselves, give `--using` an SQL expression over the
row; see [change-type]({{ site.baseurl }}/migrations#change-type).

## When a migration is refused

Nothing is written when a migration is refused. Find the case below that
matches the message.

### A new column admits no null, and the table has rows

Give every row a value with `--default`, or let rows leave the column out
with `--nullable`:

```console
$ reldir migrate add-column posts summary --type string
error[ROW_MISSING_FIELD]: operation 1 (add column posts.summary (string)): posts has rows, and summary admits no null, so every row needs a value
   = help: give a --default, or add the column as --nullable
```

### A rule of the column does not carry over to the new type

If the rule should go, name it with `--drop-constraint`. If it should stay, the
new type is the wrong one:

```console
$ reldir migrate change-type posts headline int
error[MIGRATION_REFUSED]: operation 1 (change posts.headline to int): `minLength` 1 does not carry over to a int column; name it with --drop-constraint minLength to drop it
```

### Some rows break the new constraint

The error locates the first row that would break it. Change those rows first,
with SQL or the row commands, or choose a rule the rows satisfy:

```console
$ reldir migrate add-constraint posts '{"kind": "check", "name": "headline_short", "expr": "length(headline) <= 7"}'
error[CHECK_VIOLATION]: the change was refused because the database would be invalid (1 fault); nothing was written. First: row violates check "headline_short"
  --> posts/p2.json:1:1
[..]
```

### Other refusals

- **A value cannot be converted** (`TYPE_MISMATCH`, naming the file): give
  `--using` an expression that computes the new value.
- **A rename cannot be made exactly** (`MIGRATION_REFUSED` on
  `rename-column`): the schema reaches the column through `$ref`, a
  `patternProperties` pattern matches the name, or a check names it in a way
  the rename cannot rewrite. Edit the schema file by hand, then run
  `reldir check`.
- **A re-keyed row is still referenced** (`FOREIGN_KEY_VIOLATION` from
  `change-identity`): give the foreign key an `onUpdate` action that carries
  the new key, then re-key.
- **The change discards data** (`DECISION_REQUIRED` from `drop-column` or
  `drop-table`): on a terminal, answer the question; in a script, review the
  `--dry-run` output and pass `--yes`.

## Commit exactly the change you reviewed

When the change is reviewed by someone, or applied later, take the state the
dry run planned against from its `data.state`, and commit with it. If the
database changed in between, the change is refused with `PLAN_CHANGED` and
nothing is written; preview it again.

```sh
reldir --format json --dry-run migrate apply ../migrations/0002-rename.json   # note data.state
reldir --expect-state <data.state> migrate apply ../migrations/0002-rename.json
```

## Run the same migrations on another database

Keep the migration files in version control, beside the database.

- **If the database's files are in the same repository**, a clone receives the
  migrated schemas and rows themselves. Do not run the migration again there.
- **If each environment keeps its own database** -- a folder per deployment,
  say -- apply the files to each one in order, each once.

Give each application of a migration file an idempotency key -- the file's
name and the database's, say. If you cannot tell whether it was applied --
the command was killed, or its output was lost -- run it again with the same
key: an applied migration is reported, not made again.

```sh
reldir --idempotency-key 0002-rename@production migrate apply ../migrations/0002-rename.json
```

See [How to retry a change without applying it twice]({{ site.baseurl }}/retry-safely).
