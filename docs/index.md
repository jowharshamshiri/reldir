---
title: reldir
---

# reldir

**A relational database that lives in a directory of JSON files.**

`reldir` turns an ordinary folder into a validated relational database. Rows stay
human-readable JSON files you can read, edit, `grep` and commit to Git. The
binary governs what they mean: it enforces their schemas and the references
between them, answers SQL, repairs what an outside edit broke, and records how
the state changed -- without taking ownership of your bytes.

```console
$ reldir sql 'SELECT u.name, count(*) AS posts FROM users u JOIN posts p ON p.user_id = u.id GROUP BY u.name ORDER BY u.name'
name  | posts
------+------
Ada   | 1
Grace | 1
(2 rows)
2 row(s)
```

## The idea

The directory is a legitimate interface to the database. You, your editor, a
script, an agent or `git merge` may all change it. reldir does not require that
it wrote every byte; it requires that what it observes at the start of each
command is a valid database, and it never makes an invalid one itself:

- every change reldir makes -- SQL, the row commands, imports, migrations,
  repairs -- is validated as the state it would produce *before* anything is
  written, and refused, with every reason located to the line, if that state
  would be invalid;
- a valid edit made outside reldir is recorded as a new revision; an invalid
  one is reported precisely, and `reldir doctor` offers repairs, least
  destructive first -- a row deleted by mistake is restored from history.

```console
$ rm users/grace.json
$ reldir check
error[FOREIGN_KEY_VIOLATION]: /user_id references ["grace"] in users, and no such row exists
  --> posts/p2.json:3:14
[..]
INVALID: 3 table(s), 6 row(s), 1 violation(s), 0 warning(s), 0 lint finding(s), [..] ms
```

## Guides

- **[Getting started]({{ site.baseurl }}/getting-started)**: query a folder of JSON, then make it stricter
- **[Concepts]({{ site.baseurl }}/concepts)**: validity, external edits, identity and history
- **[Schemas]({{ site.baseurl }}/schemas)**: the dialect, references, identity domains, checks and assertions
- **[SQL]({{ site.baseurl }}/sql)**: querying, and changing rows safely
- **[Validation]({{ site.baseurl }}/validation)**: `check`, `lint` and `doctor`
- **[Transactions]({{ site.baseurl }}/transactions)**: how a change is written, recovery, concurrency
- **[CLI reference]({{ site.baseurl }}/cli)**: every command, flag and output format
- **[Configuration]({{ site.baseurl }}/configuration)**: `.db/config` and resource limits
- **[Errors]({{ site.baseurl }}/errors)**: every diagnostic code and exit status
- **[On-disk format]({{ site.baseurl }}/format)**: format 2, byte for byte
- **[MCP server]({{ site.baseurl }}/mcp)**: the database, served to AI agents

## Install

```sh
cargo install reldir
```

Or from a checkout: `cargo install --path . --locked`. One self-contained
executable: no daemon, no server, no runtime dependencies.
