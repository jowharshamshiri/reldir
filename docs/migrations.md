---
title: Migrations
---

# Migrations

A migration is a list of operations on a database's structure: its tables,
columns, types, keys, constraints, indexes and identity domains. `reldir
migrate <operation>` runs a migration of one operation; `reldir migrate apply
FILE` runs a [migration file](#migration-files). Both run through one engine,
and an operation means the same thing in either form.

An operation edits schema documents and rewrites the rows it reshapes. The
file changes of a whole migration are one transaction, committed only when
every resulting schema and row is valid; a refused operation, or an invalid
result, writes nothing. Each schema is written where it lives: a pin in
`schema/`, an inferred contract in `.db/schema/` (see
[schemas]({{ site.baseurl }}/schemas#where-a-schema-lives)).

| Command | File `op` | Changes |
|---|---|---|
| `add-table TABLE --from FILE` | `add_table` | the schema |
| `drop-table TABLE` | `drop_table` | the schema, every row, the directory |
| `rename-table TABLE NEW` | `rename_table` | the schema, every row's path, every foreign key that targets it |
| `add-column TABLE COLUMN --type T` | `add_column` | the schema; every row, with `--default` |
| `drop-column TABLE COLUMN` | `drop_column` | the schema, every row that holds it |
| `rename-column TABLE COLUMN NEW` | `rename_column` | the schema, every row that holds it, every schema that names it |
| `change-type TABLE COLUMN TYPE` | `change_type` | the schema, every row that holds it |
| `change-identity TABLE IDENTITY` | `change_identity` | the schema, every row's path and, where a key column moves, its body; the references `onUpdate` carries |
| `add-constraint TABLE DEFINITION` | `add_constraint` | the schema |
| `drop-constraint TABLE NAME` | `drop_constraint` | the schema |
| `add-index TABLE COLUMNS` | `add_index` | the schema |
| `drop-index TABLE COLUMNS` | `drop_index` | the schema |
| `set-domain TABLE [DOMAIN]` | `set_identity_domain` | the schema |

## Columns

### add-column

```console
$ reldir migrate add-column users bio --type string --nullable
description
-----------------------------
add column users.bio (string)
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/users.json | false
(1 row)
migrated: 1 file(s), revision #2 ([..])
```

`--type` takes a column type: `bool`, `int`, `integer`, `float`, `number`,
`decimal`, `string`, `bytes`, `date`, `timestamp`, `uuid`, `ulid`, `enum`,
`array`, `object` or `json`. An unknown type is `SCHEMA_TYPE_UNKNOWN`, and a
column that exists is `SCHEMA_COLUMN_EXISTS`.

`--nullable` admits null, and existing rows are left without the column. A
column that admits no null needs a value in every existing row, so a table
with rows needs `--default`:

```console
$ reldir migrate add-column users active --type bool
error[ROW_MISSING_FIELD]: operation 1 (add column users.active (bool)): users has rows, and active admits no null, so every row needs a value
   = help: give a --default, or add the column as --nullable
$ reldir migrate add-column users active --type bool --default true
description
------------------------------
add column users.active (bool)
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/users.json | false
write  | users/ada.json    | false
write  | users/grace.json  | false
(3 rows)
migrated: 3 file(s), revision #3 ([..])
$ cat users/ada.json
{
  "id": "ada",
  "name": "Ada",
  "active": true
}
```

`--default` takes JSON. It becomes the column's `default` and is written into
every existing row.

### rename-column

```console
$ reldir migrate rename-column users name full_name
description
-------------------------------------
rename column users.name to full_name
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/users.json | false
write  | users/ada.json    | false
write  | users/grace.json  | false
(3 rows)
migrated: 3 file(s), revision #4 ([..])
$ cat users/ada.json
{
  "id": "ada",
  "full_name": "Ada",
  "active": true
}
```

The column is renamed everywhere: in `properties`, where it keeps its place;
in `required`, `dependentRequired`, `dependentSchemas` and `propertyNames`; in
the `if`, `then`, `else`, `not`, `allOf`, `anyOf` and `oneOf` branches that
describe the same object; in the table's identity, unique constraints,
indexes and generators; in its reference paths, acyclic edges and checks; in
every foreign key that targets it; in the assertions of every table; and in
every row, where the member keeps its place.

A rename that cannot be made exactly is `MIGRATION_REFUSED`: when the object's
schema reaches a definition through `$ref` or `$dynamicRef`, which may name the
member out of sight, or when a `patternProperties` pattern matches the old or
the new name, so the rename would change which rules govern the member.

### change-type

```console
$ reldir migrate change-type users active string --using "CASE WHEN active THEN 'yes' ELSE 'no' END"
description
-----------------------------
change users.active to string
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/users.json | false
write  | users/ada.json    | false
write  | users/grace.json  | false
(3 rows)
migrated: 3 file(s), revision #5 ([..])
$ cat users/ada.json
{
  "id": "ada",
  "full_name": "Ada",
  "active": "yes"
}
```

Every value is converted. Without `--using`, a value converts only when
nothing is lost, such as `true` to `"true"` or `"42"` to `42`; any other value
is `TYPE_MISMATCH`, naming the file. `--using` gives a SQL expression over the
row that computes each new value. `--values a,b,c` lists the members when the
new type is `enum`; without it, the members of an existing `enum` are kept.

The column's other rules are kept when they still mean something for the new
type:

- annotations, such as `description` and `title`, always;
- `default`, `const`, `enum` and `examples`, converted without loss;
- numeric bounds (`minimum`, `multipleOf` and the like) for a numeric type;
- string rules (`minLength`, `pattern`, `format` and the like) for a string
  type;
- array rules for `array`, and object rules for `object`;
- every rule for `json`.

What the old type wrote itself, such as a `uuid`'s `format` or a `decimal`'s
`pattern`, goes with it. A type change never quietly loosens what a column
admits, so a rule that cannot be kept is refused by name:

```console
$ reldir migrate change-type posts title int
error[MIGRATION_REFUSED]: operation 1 (change posts.title to int): `minLength` 1 does not carry over to a int column; name it with --drop-constraint minLength to drop it
```

`--drop-constraint KEYWORD` drops that rule, and may be repeated. Naming a rule
the column does not have is a usage error.

### drop-column

Dropping a column discards data, so it asks for confirmation. Without a
terminal to ask on, it needs `--yes`:

```console
$ reldir migrate drop-column users bio
error[DECISION_REQUIRED]: this discards data: drop column users.bio; continue -- this needs a decision
   = help: pass --yes to confirm, or --dry-run to see exactly what would change
description
---------------------
drop column users.bio
(1 row)
$ reldir migrate drop-column users bio --yes
description
---------------------
drop column users.bio
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/users.json | false
(1 row)
migrated: 1 file(s), revision #6 ([..])
```

The column is removed from the schema and from every row that holds it.

## Constraints and indexes

### add-constraint

`reldir migrate add-constraint TABLE DEFINITION` adds a constraint written as
JSON, with a `kind` and the members the [schema dialect]({{ site.baseurl }}/schemas#x-reldir)
gives it:

| Kind | Definition |
|---|---|
| `unique` | `{"kind": "unique", "columns": ["email"]}` |
| `foreign_key` | `{"kind": "foreign_key", "from": [...], "to": {...}, ...}`, with the members of a `foreignKeys` entry |
| `check` | `{"kind": "check", "name": "...", "expr": "..."}` |
| `acyclic` | `{"kind": "acyclic", "name": "...", "edges": [...]}` |
| `assertion` | `{"kind": "assertion", "name": "...", "query": "...", ...}`, with the members of an `assertions` entry |

```console
$ reldir migrate add-constraint posts '{"kind": "check", "name": "title_short", "expr": "length(title) <= 80"}'
description
-------------------------
add a constraint to posts
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/posts.json | false
(1 row)
migrated: 1 file(s), revision #7 ([..])
```

The rows must already satisfy the new constraint, or the migration is refused
with the violation it would introduce. A unique constraint or an index that
already exists is `SCHEMA_CONSTRAINT_EXISTS`, and a name already in use is
`SCHEMA_CONSTRAINT_NAME_DUPLICATE`.

### drop-constraint

`reldir migrate drop-constraint TABLE NAME` removes a foreign key, check,
acyclic graph or assertion by its name. A foreign key without a declared name
is named from its paths, as in `fk_user_id`. A unique constraint is named
`unique_` and its columns joined with `_`, as in `unique_team_id_handle`. A
name the table does not have is `UNKNOWN_CONSTRAINT`.

### add-index and drop-index

```console
$ reldir migrate add-index posts user_id
description
--------------------
index posts(user_id)
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/posts.json | false
(1 row)
migrated: 1 file(s), revision #8 ([..])
```

Columns are separated by commas: `reldir migrate add-index posts user_id,title`.
`drop-index` takes the same columns. Every key and reference is indexed
already; an index is only for queries.

## Keys and domains

### change-identity

`reldir migrate change-identity TABLE IDENTITY` gives a table a new
`x-reldir.identity` (see [identity]({{ site.baseurl }}/schemas#identity)):

```console
$ reldir migrate change-identity users '{"source": "filename", "column": "id", "type": "string"}' --dry-run
description
------------
re-key users
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/users.json | true
write  | users/ada.json    | true
write  | users/grace.json  | true
(3 rows)
dry run: 3 file(s) would change; nothing was written
```

Every file is renamed to its new key. A column the old identity read from the
file name becomes a member of each body, and a body column the new identity
reads from the file name leaves the body. The rows' values stay as they are.

Every reference to a row whose key changes is carried by its foreign key's
`onUpdate` action. `restrict` and `no_action` refuse the change with
`FOREIGN_KEY_VIOLATION`. A reference by a composite key, or to a table re-keyed
to or from a composite key, cannot be carried and is `MIGRATION_REFUSED`.

### set-domain

```console
$ reldir migrate set-domain users people
description
------------------------------------
join users to identity domain people
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/users.json | false
(1 row)
migrated: 1 file(s), revision #9 ([..])
$ reldir migrate set-domain users
description
-------------------------------------
remove users from its identity domain
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/users.json | false
(1 row)
migrated: 1 file(s), revision #10 ([..])
```

Without a domain, the table leaves its domain. A key held by two tables of one
domain is `DOMAIN_KEY_VIOLATION`, and the migration is refused.

## Tables

### add-table

`reldir migrate add-table TABLE --from FILE` declares a table from a complete
schema document:

```console
$ echo '{"$schema": "https://reldir.dev/schema/reldir-3", "type": "object", "properties": {"id": {"type": "string"}, "body": {"type": "string"}}, "required": ["id", "body"], "additionalProperties": false, "x-reldir": {"table": "notes", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}}' > notes.schema.json
$ reldir migrate add-table notes --from notes.schema.json
description
---------------
add table notes
(1 row)

action | path              | planned
-------+-------------------+--------
write  | schema/notes.json | false
(1 row)
migrated: 1 file(s), revision #11 ([..])
```

The schema is written to `schema/`: a table a migration declares is pinned,
so a value outside it is an error, whoever writes it. `x-reldir.table` must name the table
(`SCHEMA_TABLE_NAME_MISMATCH`). A table that exists is `SCHEMA_TABLE_EXISTS`.
`reldir schema new TABLE` declares a minimal table as a pin directly.

### rename-table

`reldir migrate rename-table TABLE NEW` renames the table's schema, moves its
rows to the new directory, removes the old one, and points every foreign key
that targets it at the new name. A
name that breaks the naming rule is `SCHEMA_INVALID_TABLE_NAME`, and a table
that exists is `SCHEMA_TABLE_EXISTS`.

### drop-table

`reldir migrate drop-table TABLE` removes the table's schema, every row and
its directory. It
discards data, so it asks for confirmation, or `--yes`. A foreign key of
another table that still targets it is `SCHEMA_FK_TARGET_MISSING`, and the
migration is refused; drop that constraint first, in the same migration or an
earlier one.

## Migration files

`reldir migrate apply FILE` runs a migration file; `-` reads it from standard
input. The file is a JSON object with one member, `operations`, a list run in
order:

```console
$ echo '{"operations": [{"op": "drop_constraint", "table": "posts", "name": "fk_tags"}, {"op": "add_constraint", "table": "posts", "definition": {"kind": "foreign_key", "from": ["tags[]"], "to": {"table": "tags"}, "onDelete": "remove", "onUpdate": "cascade"}}]}' > tags-follow-renames.json
$ reldir migrate apply tags-follow-renames.json
description
----------------------------------
drop constraint fk_tags from posts
add a constraint to posts
(2 rows)

action | path              | planned
-------+-------------------+--------
write  | schema/posts.json | false
(1 row)
migrated: 1 file(s), revision #12 ([..])
```

Each operation is an object whose `op` names it. The members are those of the
command, with the column type as `type`:

| `op` | Members |
|---|---|
| `add_table` | `table`, `schema` (the whole document) |
| `drop_table` | `table` |
| `rename_table` | `table`, `new` |
| `add_column` | `table`, `column`, `type`, `nullable` (default `false`), `default` |
| `drop_column` | `table`, `column` |
| `rename_column` | `table`, `column`, `new` |
| `change_type` | `table`, `column`, `type`, `using`, `values`, `drop_constraints` |
| `change_identity` | `table`, `identity` |
| `add_constraint` | `table`, `definition` |
| `drop_constraint` | `table`, `name` |
| `add_index` | `table`, `columns` |
| `drop_index` | `table`, `columns` |
| `set_identity_domain` | `table`, `domain` (`null` to leave the domain) |

An unknown `op` or member is refused before anything runs, naming what the
file may hold. Each operation sees the state the operations before it left,
and the whole file is one transaction. A refused operation is named by its
position, and nothing is written:

```console
$ echo '{"operations": [{"op": "add_column", "table": "users", "column": "email", "type": "string", "nullable": true}, {"op": "drop_column", "table": "users", "column": "nickname"}]}' > broken.json
$ reldir migrate apply broken.json
error[UNKNOWN_COLUMN]: operation 2 (drop column users.nickname): users.nickname does not exist
```

A file that drops a table or a column asks for confirmation, as the commands
do.

## Flags

| Flag | Effect on a migration |
|---|---|
| `--dry-run` | Plans and validates the whole migration, lists every file it would change, and writes nothing. |
| `--expect-state STATE` | Commits only if the database still holds `STATE`, the `data.state` a dry run reported; otherwise `PLAN_CHANGED`. |
| `--yes` | Confirms an operation that discards data: `drop-column`, `drop-table`. |
| `--idempotency-key KEY` | Records `KEY` in the migration's revision. When history already holds a revision under `KEY`, the migration is not planned or made again, and that revision is reported. |

See [transactions]({{ site.baseurl }}/transactions) for the state a change is
planned against, and for idempotency keys.

## MIGRATION_REFUSED

`MIGRATION_REFUSED` (exit `9`) means the migration cannot be carried out
exactly as the schema is written. The message says what to edit by hand, or
which option to give:

| Operation | Refused when |
|---|---|
| `change-type` | A rule of the column does not carry over to the new type. Name it with `--drop-constraint`. |
| `rename-column` | The object's schema uses `$ref` or `$dynamicRef`, or a `patternProperties` pattern matches the old or the new name. Edit the schema. |
| `rename-column` | A check or an assertion names the column in a way the rename cannot rewrite. Edit it by hand. |
| `change-identity` | A reference by a composite key would have to be carried. Re-key in steps of one single-column key. |

Exit status `9` is shared with `DECISION_REQUIRED`: a decision is needed, and
nothing was written. The [error reference]({{ site.baseurl }}/errors) lists
every code a migration can return.
