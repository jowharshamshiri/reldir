---
title: How to put an existing folder of JSON under reldir
---

# How to put an existing folder of JSON under reldir

This guide takes a folder that programs already read and write -- one
directory of JSON files per kind of record -- and makes it a reldir database,
without changing any of its files. What adoption records, and why, is
explained in [Concepts]({{ site.baseurl }}/concepts#the-observed-contract).

The folder here holds two kinds of record and a directory of scripts:

```console
$ ls
authors
books
scripts
$ cat authors/ada.json
{
    "name": "Ada Lovelace",
    "born": 1815
}
$ cat books/b1.json
{"id": "b1", "author_id": "ada", "title": "Sketch of the Analytical Engine", "year": 1843, "tags": ["engines"]}
```

If the folder is in git, commit or stash your own work first, so that
afterwards `git status` shows exactly what adoption added.

## See what adoption would make of the folder

`reldir inspect` reports the tables reldir would infer, or what stops it, and
writes nothing:

```console
$ reldir inspect
error[INFER_NON_JSON_FILE]: this file is not .json, and a table directory holds only rows
  --> books/README.txt
   = help: move it out of the table directory, or leave it ungoverned with an ignore pattern: `reldir init --adopt --ignore books/README.txt` for a folder not yet adopted, `ignore` in .db/config for a database
```

Resolve each fault before you adopt:

- `INFER_NON_JSON_FILE` or `INFER_NESTED_DIRECTORY`: a table's directory holds
  something that is not a row -- a `README.txt`, a directory of images. If it
  belongs there, leave it out of the database with `--ignore PATTERN`;
  otherwise move it out of the directory.
- `INFER_INVALID_JSON` or `INFER_ROOT_NOT_OBJECT`: a file is not one JSON
  object. Fix it, or move it out.
- `ROOT_JSON_AMBIGUOUS`: JSON files sit at the root, in no table. Move them
  into a table's directory, or leave them out with `--ignore`.

Leave out, too, any top-level directory that is not data, such as `scripts`
here. Give `inspect` the same patterns you will adopt with:

```console
$ reldir inspect --ignore books/README.txt --ignore scripts
table   | rows | primary_key | declared
--------+------+-------------+---------
authors | 2    | ["id"]      | false
books   | 2    | ["id"]      | false
(2 rows)
[..]: metadata absent; 2 table(s)
```

Each code is described in the [error reference]({{ site.baseurl }}/errors).

## Adopt the folder

```console
$ reldir init --adopt --ignore books/README.txt --ignore scripts
table   | primary_key | references
--------+-------------+-----------
authors | ["id"]      | 0
books   | ["id"]      | 1
(2 rows)
initialized [..] (VALID, 2 table(s))
```

The patterns are kept in `ignore` in `.db/config`, beside the defaults, so
every later command leaves the same files out.

Adoption writes only `.db/`. Every row file keeps its bytes:

```console
$ cat authors/ada.json
{
    "name": "Ada Lovelace",
    "born": 1815
}
```

In git, `git status` now lists `.db/` and nothing else.

## Check what was inferred

Read each table's contract, and confirm that it says what the programs reading
the files rely on:

```console
$ reldir schema show authors
{
  "$schema": "https://reldir.dev/schema/reldir-3",
  "type": "object",
  "properties": {
    "name": {
      "type": "string"
    },
    "born": {
      "type": "integer",
      "x-reldir-type": "int"
    },
    "email": {
      "type": "string"
    }
  },
  "required": [
    "name",
    "born"
  ],
  "additionalProperties": false,
  "x-reldir": {
    "table": "authors",
    "identity": {
      "source": "filename",
      "column": "id",
      "type": "string"
    },
[..]
}
```

Look in particular at:

- **The key.** `authors` holds no key in its files, so the file name is the
  key, as a column named `id`. `books` holds its key in `id`, which matches its
  file names. If reldir chose a different column from the one you mean, choose
  it with `reldir infer TABLE --pk COLUMN --on-schema-conflict reinfer --write`.
  A column whose values do not spell the file names is refused, because
  adoption never renames a file.
- **What is required.** A member is required only if every file holds it.
  `email` is optional, because one author has none.
- **References.** `books.author_id` became a reference to `authors`, because
  its name says so and every value is an author's key. A reference named some
  other way is proposed by `reldir lint` instead (below).

The keywords are described in the [schema reference]({{ site.baseurl }}/schemas).

## Resolve what check reports

```console
$ reldir check
VALID: 2 table(s), 4 row(s), 0 violation(s), 0 warning(s), [..] lint finding(s), [..] ms
```

If check reports something:

- `UNGOVERNED_DIRECTORY`: a top-level directory is not a table. If it is not
  data, add it to the `ignore` list in `.db/config`, keeping the patterns
  already there; if it is data, adopt it as a table with
  `reldir infer DIRECTORY --write`.
- An error: a file breaks a rule the rest of the folder keeps. See
  [How to repair an invalid database]({{ site.baseurl }}/repair-a-database).

The settings are listed in the [configuration reference]({{ site.baseurl }}/configuration).

## Add the rules the data suggests

An inferred contract states the shape the files have, never a coincidence of
the data such as every value being distinct. `reldir lint` lists what the data
suggests:

```console
$ reldir lint
[..]
warning[LINT_INCONSISTENT_PRESENCE]: authors.email is present in 1 of 2 rows; this row lacks it
  --> authors/ada.json
[..]
3 lint finding(s)
```

For each finding you agree with, apply its fix by name, for example
`reldir doctor --fix --only FIX_ADD_ENUM`. A fix that changes what a table
promises is applied only when you name it. To state every suggested rule at
once, re-infer a table with `reldir infer TABLE --tighten all
--on-schema-conflict reinfer --write`.

## Own the schemas you will maintain

A table you will design further -- add columns to, add rules to, review in pull
requests -- is better pinned. Pinning moves its schema to `schema/`:

```console
$ reldir schema pin books
action | path                  | planned
-------+-----------------------+--------
write  | schema/books.json     | false
delete | .db/schema/books.json | false
(2 rows)
pinned: 2 file(s), revision #2 ([..])
```

A table you leave unpinned keeps its inferred contract in `.db/schema/`.

## Commit the result

`.db/.gitignore` keeps out of version control everything each clone builds for
itself. Commit the rest:

```sh
git add .db schema
git commit -m "govern the folder with reldir"
```

This adds `.db/format`, `.db/config`, the inferred contracts in
`.db/schema/`, `.db/.gitignore` and any pins, so every clone reads the folder
with the same contracts. To merge row files member by member, see
[How to merge and diff row files with git]({{ site.baseurl }}/merge-with-git).
