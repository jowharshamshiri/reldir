---
title: reldir
---

# reldir

**A relational database that lives in a folder of JSON files.**

reldir governs an ordinary folder -- one directory per table, one JSON file per
row -- as a relational database. The files stay the source of truth: you, your
editor, your scripts and `git` read and write them directly. reldir adds a
schema for every table, references between rows, SQL over all of it, changes
validated before a byte is written, and a verified history of every state the
folder has been in.

```console
$ reldir sql 'SELECT u.name, count(*) AS posts FROM users u JOIN posts p ON p.user_id = u.id GROUP BY u.name ORDER BY u.name'
name  | posts
------+------
Ada   | 1
Grace | 1
(2 rows)
2 row(s)
$ reldir delete users ada
error[FOREIGN_KEY_VIOLATION]: deleting users/ada.json refused: posts/p1.json still references it at /user_id (posts.user_id -> users, onDelete restrict)
  --> posts/p1.json:3:14
[..]
```

- **Adopts a folder as it is**, inferring each table's schema from its files
  and changing none of them.
- **Keeps the folder's contract**: nothing reldir writes leaves the shape the
  files had, unless you widen it.
- **Writes only what changes**, down to number spelling and indentation, and
  many files at once or not at all.
- **Answers SQL** in SQLite's dialect, and enforces keys, references, checks
  and assertions beside every rule of JSON Schema 2020-12.
- **Judges outside edits** to their file, line and column, and repairs them.
- **Keeps history**, merges with git, serves AI agents over MCP, and embeds in
  Rust, Python and Node.js.

## Tutorial

| | |
|---|---|
| [Getting started]({{ site.baseurl }}/getting-started) | from an empty folder to a database we query, change, repair and look back through |

## How-to guides

| | |
|---|---|
| [Install reldir]({{ site.baseurl }}/install) | the command and the Python and Node.js libraries |
| [Put an existing folder under reldir]({{ site.baseurl }}/adopt-a-folder) | adopt a folder of JSON without changing a file |
| [Declare a new table]({{ site.baseurl }}/declare-a-table) | write a table's schema yourself |
| [Write a value the contract does not admit]({{ site.baseurl }}/widen-a-contract) | widen a table's contract, deliberately |
| [Change the shape of a table]({{ site.baseurl }}/change-a-schema) | migrate schemas and rows together |
| [Import and export rows]({{ site.baseurl }}/import-and-export) | JSON, JSONL, CSV and Parquet, in bulk |
| [Repair an invalid database]({{ site.baseurl }}/repair-a-database) | from `INVALID` back to `VALID` |
| [Undo a change]({{ site.baseurl }}/undo-a-change) | find a change and go back before it |
| [Recover]({{ site.baseurl }}/recover) | after an interrupted change, or damaged metadata |
| [Retry a change safely]({{ site.baseurl }}/retry-safely) | never apply a change twice |
| [Merge and diff with git]({{ site.baseurl }}/merge-with-git) | row files that merge member by member |
| [Check a database in CI]({{ site.baseurl }}/check-in-ci) | fail a build on a fault, annotate the pull request |
| [Give an AI agent access]({{ site.baseurl }}/connect-an-agent) | the database over MCP |
| [Keep a large folder fast]({{ site.baseurl }}/keep-a-large-folder-fast) | up to a million rows |

## Reference

| | |
|---|---|
| [CLI]({{ site.baseurl }}/cli) | every command and flag |
| [SQL]({{ site.baseurl }}/sql) | queries, changes, types, scripts, the shell |
| [Schemas]({{ site.baseurl }}/schemas) | the schema dialect |
| [Validation]({{ site.baseurl }}/validation) | `check`, `lint` and `doctor` |
| [Migrations]({{ site.baseurl }}/migrations) | every migration operation |
| [History and snapshots]({{ site.baseurl }}/snapshots) | revisions, snapshots, `gc` |
| [Git integration]({{ site.baseurl }}/git) | the merge and diff drivers, the CI action |
| [MCP server]({{ site.baseurl }}/mcp) | tools, resources and prompts |
| [Machine protocol]({{ site.baseurl }}/protocol) | the JSON every command speaks |
| [Errors]({{ site.baseurl }}/errors) | every code and exit status |
| [Configuration]({{ site.baseurl }}/configuration) | `.db/config` |
| [Storage format]({{ site.baseurl }}/format) | what is on disk, byte for byte |
| [Python]({{ site.baseurl }}/python), [Node.js]({{ site.baseurl }}/node), [Rust]({{ site.baseurl }}/rust) | the libraries |
| [Stability]({{ site.baseurl }}/stability) | what semantic versioning covers |

## Explanation

| | |
|---|---|
| [Concepts]({{ site.baseurl }}/concepts) | the folder as the database, the observed contract, identity, history |
| [Transactions]({{ site.baseurl }}/transactions) | how a change across many files lands whole |
| [Performance]({{ site.baseurl }}/performance) | why a command costs what changed, not what exists |
| [Security]({{ site.baseurl }}/security) | what reldir trusts, and why |
| [reldir beside other tools]({{ site.baseurl }}/comparison) | SQLite, DuckDB, Dolt and flat-file stores |
