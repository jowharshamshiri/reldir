---
title: Getting started
---

# Getting started

## Install

```sh
cargo install reldir
```

`reldir` is published on [crates.io](https://crates.io/crates/reldir). To build
from a checkout instead:

```sh
cargo install --path . --locked
```

Either way you get a single `reldir` executable.

## Point it at a folder

If you already have folders of JSON files, there is nothing to set up. Run a
query and `reldir` infers a schema for each directory, validates every row, and
records the first revision as it answers:

```console
$ ls
users/  posts/

$ reldir 'SELECT * FROM users ORDER BY id'
initialized database; inferred schemas for posts, users
id | name
---+------
u1 | Alice
u2 | Bob
(2 rows)
```

Run `reldir` with no arguments and you get a shell over the same model:

```console
$ reldir
reldir> .tables
users posts
reldir> SELECT name FROM users ORDER BY name;
 name
-------
 Alice
 Bob
reldir> .quit
```

Inference is all-or-nothing. If it fails for any table, the command fails as a
whole and writes nothing, so you are never left with a half-initialised
directory.

### Adopting explicitly

`reldir init --adopt` does the same work as a separate, deliberate step, which
is what you want in a script or a CI job where the setup should not be a side
effect of the first query:

```console
$ reldir init ./data --adopt
Scanned 2 directories, 5 JSON files.
VALID   revision 1   root a8a1104e
```

The outcome is identical either way: the same revision 1, the same root hash,
the same schema files.

### What inference wrote

One working schema per table, in `.db/schema/`:

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
lost. `reldir schema pin <table>` copies it to `schema/`, where it becomes a
declaration you own and keep in version control.

## Start empty instead

```sh
reldir init ./data
```

This creates `.db/` and nothing else. Nothing is governed until a schema
exists, so `reldir check` reports `0 tables, 0 rows`. That is not an error; it means
no table is under governance yet. Add schemas with `reldir schema new <table>`, or
point inference at a directory:

```sh
reldir infer users            # print a proposed schema
reldir infer users --write    # write .db/schema/users.json
reldir infer --write          # write a schema for every table that lacks one
```

`reldir infer` never writes without `--write`, and never overwrites an existing
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

$ reldir status
VALID   revision 2   root b119300e   external changes: accepted

$ reldir 'SELECT title FROM books'
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

`reldir schema new <table>` scaffolds one to edit, and `reldir schema dialect` writes out
the dialect so your editor can complete it. [Schemas]({{ site.baseurl }}/schemas) documents every
keyword.

## Query

```console
$ reldir 'SELECT * FROM users ORDER BY id'
id | name
---+------
u1 | Alice
u2 | Bob
(2 rows)
```

Output is a table on a terminal and JSON Lines when redirected, so piping into
other tools just works:

```console
$ reldir 'SELECT * FROM users ORDER BY id' | head -1
{"id":"u1","name":"Alice","kind":"row"}
```

Pick an encoding explicitly with `--format table|json|jsonl|csv`.

## Change data

Through SQL, or through the equivalent CRUD commands. Both go through the same
validation and transaction machinery:

```sh
reldir insert users '{"id":"u3","name":"Carol"}'
reldir update users u3 '{"name":"Caroline"}'
reldir delete users u3

reldir "UPDATE users SET name = 'Rob' WHERE id = 'u2'"
```

Every mutation prints the files it touched and the new revision:

```console
$ reldir update users u1 '{"name":"Alicia"}'
changed 1 path(s); revision 2
  users/u1.json
```

Preview without writing using `--dry-run`.

## Edit the files directly

Change a row with your editor, add a file, or delete one, then ask what
happened:

```console
$ echo '{"id":"u9","name":"Zoe"}' > users/u9.json
$ reldir status
VALID   revision 3   root 71ab3c9d   external changes: accepted
changed:
  A users/u9.json
```

The new state was valid, so it became a new revision with origin `external`.

If an edit breaks an invariant, the state is `INVALID`, the command exits non-zero,
and nothing is adopted:

```console
$ echo '{"id":"u8","name":4}' > users/u8.json
$ reldir check
error[TYPE_MISMATCH]: field "name" does not match type String
  --> users/u8.json:1:24
```

## Next steps

- [Validation]({{ site.baseurl }}/validation): the `check` → `lint` → `doctor` workflow
- [Schemas]({{ site.baseurl }}/schemas): tighten what inference guessed
- [CLI reference]({{ site.baseurl }}/cli): the full command surface
