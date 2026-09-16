---
title: reldir
---

# reldir

**A relational database that lives in a directory of JSON files.**

`reldir` turns an ordinary folder into a validated relational database. Rows stay
as human-readable JSON files you can read, edit, `grep`, and commit to Git. The
`reldir` binary governs their interpretation: it enforces schemas, checks referential
integrity, answers SQL, and records how the state changed, without taking
ownership of your bytes.

```console
$ ls ./data
users/  posts/  comments/

$ reldir init ./data --adopt
Scanned 3 directories, 1204 JSON files.
VALID   revision 1   root 6e41f2…

$ reldir 'SELECT name FROM users ORDER BY name'
 name
-------
 Alice
 Bob
(2 rows)
```

## The idea

The directory is a legitimate interface to the database. You, your editor, a
script, an agent, or `git merge` may all change it. `reldir` does not require that
it wrote every byte, only that what it observes at the boundary of each operation
forms a valid relational database.

> **Validity, not provenance, determines whether a state is acceptable.**

When the state is valid, the changes are adopted as a new revision. When it is
not, you get a precise diagnostic and a path back:

```console
$ reldir status
INVALID   revision 1   (1 external change, 1 violation)

error[FOREIGN_KEY_VIOLATION]: posts.user_id references a row that does not exist
  --> posts/broken.json:1:24
   = constraint: posts.user_id -> users.id
   = help: run `reldir doctor` for fix options
```

## Guides

- **[Getting started]({{ site.baseurl }}/getting-started)**: query a folder of JSON, then make it stricter
- **[Concepts]({{ site.baseurl }}/concepts)**: the model behind validity, external edits, and provenance
- **[Schemas]({{ site.baseurl }}/schemas)**: the JSON Schema dialect, writing one by hand, the type system, constraints
- **[CLI reference]({{ site.baseurl }}/cli)**: every command and flag
- **[SQL]({{ site.baseurl }}/sql)**: the supported subset and parameter binding
- **[Validation]({{ site.baseurl }}/validation)**: `check`, `lint`, and `doctor`
- **[Transactions]({{ site.baseurl }}/transactions)**: write safety, recovery, concurrency
- **[Configuration]({{ site.baseurl }}/configuration)**: `.db/config` and resource limits
- **[Errors]({{ site.baseurl }}/errors)**: error codes, exit codes, and what they mean
- **[On-disk format]({{ site.baseurl }}/format-v1)**: format version 1

## Install

```sh
cargo install reldir
```

Published on [crates.io](https://crates.io/crates/reldir). To build from a
checkout instead, `cargo install --path . --locked`.

A single self-contained `reldir` executable. No daemon, no server, no sidecar.
