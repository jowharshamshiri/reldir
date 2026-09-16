---
title: Getting started
---

# Getting started

## Install

```sh
cargo install --path . --locked
```

This produces a single `db` executable.

## Adopt a directory you already have

If you already have folders of JSON files, `--adopt` infers a schema for each one,
validates everything, and records the initial revision:

```console
$ ls ./data
users/  posts/

$ db init ./data --adopt
Scanned 2 directories, 5 JSON files.
VALID   revision 1   root a8a1104e
```

Adoption is all-or-nothing. If inference or validation fails for any table, the
command fails as a whole and writes nothing, so you are never left with a
half-initialised directory.

Inference writes one working schema per table into `.db/schema/`:

```json
{
  "$schema": "https://reldir.dev/schema/reldir-1",
  "type": "object",
  "properties": {
    "id":   { "type": "string" },
    "name": { "type": "string" }
  },
  "required": ["id", "name"],
  "additionalProperties": false,
  "x-reldir": {
    "table": "users",
    "schemaVersion": 1,
    "primaryKey": ["id"],
    "columnOrder": ["id", "name"]
  }
}
```

That schema lives under `.db/`, so deleting `.db/` discards it and the next
command re-infers it. While a table is unpinned, `status`, `check`, and `lint`
report `LINT_SCHEMA_UNPINNED`, because any refinement you make by hand would be
lost. `db schema pin <table>` copies it to `schema/`, where it becomes a
declaration you own and keep in version control.

## Start empty instead

```sh
db init ./data
```

This creates `.db/` and nothing else. Nothing is governed until a schema
exists, so `db check` reports `0 tables, 0 rows`. That is not an error; it means
no table is under governance yet. Add schemas with `db schema new <table>`, or
point inference at a directory:

```sh
db infer users            # print a proposed schema
db infer users --write    # write .db/schema/users.json
db infer --write          # write a schema for every table that lacks one
```

`db infer` never writes without `--write`, and never overwrites an existing
schema.

## Write a schema yourself

Inference is a bootstrap, not a requirement. A schema is a JSON Schema 2020-12
document, so you can write one by hand and drop it in `schema/`. The next command
picks it up:

```console
$ mkdir -p schema && cat > schema/books.json <<'JSON'
{
  "$schema": "https://reldir.dev/schema/reldir-1",
  "type": "object",
  "properties": {
    "id":    { "type": "string" },
    "title": { "type": "string" },
    "year":  { "type": "integer", "x-reldir-type": "int" }
  },
  "required": ["id", "title", "year"],
  "additionalProperties": false,
  "x-reldir": {
    "table": "books",
    "primaryKey": ["id"],
    "columnOrder": ["id", "title", "year"]
  }
}
JSON

$ db status
VALID   revision 2   root b119300e   external changes: accepted

$ db sql 'SELECT title FROM books'
title
-----
Dune
(1 rows)
```

`properties` declares the columns with standard JSON Schema keywords.
`required` lists the columns a row cannot omit, which is a question of presence
rather than nullability: a nullable column is written `"type": ["string",
"null"]`. `x-reldir` carries what JSON Schema has no keyword for: which table this
is, its primary key, and the order columns are written in.

`db schema new <table>` scaffolds one to edit, and `db schema dialect` writes out
the dialect so your editor can complete it. [Schemas](schemas) documents every
keyword.

## Query

```console
$ db sql 'SELECT * FROM users ORDER BY id'
id | name
---+------
u1 | Alice
u2 | Bob
(2 rows)
```

Output is a table on a terminal and JSON Lines when redirected, so piping into
other tools just works:

```console
$ db sql 'SELECT * FROM users ORDER BY id' | head -1
{"id":"u1","name":"Alice","kind":"row"}
```

Pick an encoding explicitly with `--format table|json|jsonl|csv`.

## Change data

Through SQL, or through the equivalent CRUD commands. Both go through the same
validation and transaction machinery:

```sh
db insert users '{"id":"u3","name":"Carol"}'
db update users u3 '{"name":"Caroline"}'
db delete users u3

db sql "UPDATE users SET name = 'Rob' WHERE id = 'u2'"
```

Every mutation prints the files it touched and the new revision:

```console
$ db update users u1 '{"name":"Alicia"}'
changed 1 path(s); revision 2
  users/u1.json
```

Preview without writing using `--dry-run`.

## Edit the files directly

Change a row with your editor, add a file, or delete one, then ask what
happened:

```console
$ echo '{"id":"u9","name":"Zoe"}' > users/u9.json
$ db status
VALID   revision 3   root 71ab3c9d   external changes: accepted
changed:
  A users/u9.json
```

The new state was valid, so it became a new revision with origin `external`.

If an edit breaks an invariant, the state is `INVALID`, the command exits non-zero,
and nothing is adopted:

```console
$ echo '{"id":"u8","name":4}' > users/u8.json
$ db check
error[TYPE_MISMATCH]: field "name" does not match type String
  --> users/u8.json:1:24
```

## Next steps

- [Validation](validation): the `check` → `lint` → `doctor` workflow
- [Schemas](schemas): tighten what inference guessed
- [CLI reference](cli): the full command surface
