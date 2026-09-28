---
title: On-disk format
---

# On-disk format 2

This page fixes what format 2 writes and reads. A binary reads exactly its own
format: any other version is `FORMAT_UNSUPPORTED`, never interpreted.

## Namespaces

| Path | What |
|---|---|
| `<table>/<name>.json` | one row: one JSON object |
| `schema/<table>.json` | a pin: the table's schema, declared |
| `.db/format` | `format_version = 2` |
| `.db/config` | settings (see [Configuration]({{ site.baseurl }}/configuration)) |
| `.db/schema/<table>.json` | a working schema: inferred for a table nobody pinned |
| `.db/provenance/<revision>.json` | one recorded revision; the name is the revision, zero-padded to 20 digits |
| `.db/objects/<sha256>.json` | a recorded row, schema, format or configuration, named by its hash |
| `.db/snapshots/<name>/` | a complete copy of the authoritative files |
| `.db/provenance-quarantine/<time>/` | history moved aside by a new lineage |
| `.db/mirror.sqlite` | derived: the relational index (layout `reldir-mirror-2`) |
| `.db/transactions/<uuid>/` | ephemeral: a transaction's staged bytes and journal |
| `.db/lock` | ephemeral: the advisory writer lock |

Table and schema names are lowercase ASCII identifiers (`^[a-z][a-z0-9_]*$`),
never `schema`, never a Windows device name. Governed files must be private
regular files: symlinks, sockets, FIFOs, devices and hard links are refused and
never followed.

## Row file names

A row's file name is built from its `filename` columns -- its primary key unless
`x-reldir.filename` says otherwise. Each value is rendered as text (a timestamp in
UTC, a number in its shortest round-trip form) and encoded byte by byte as UTF-8:
ASCII letters, digits, `.`, `_` and `-` stay literal, except a leading `.`; every
other byte is `%HH` in uppercase hex. A single value that would be a Windows
device name (`con`, `prn`, `aux`, `nul`, `com1`-`com9`, `lpt1`-`lpt9`, any case,
any extension) has its first byte encoded. Values are joined with `,` and `.json`
is appended. A name longer than 255 bytes is refused (`FILENAME_TOO_LONG`).

## Rows as written

reldir writes a row with exactly the members it has, in the order it has them;
a row it creates takes its members in the order it was given them. Strings and
member names are NFC-normalized, `-0` is `0`, timestamp columns are written in
UTC, and the file is indented with `indentation_width` spaces and ends with one
newline. A file reldir did not write is never rewritten merely to reformat it;
`doctor --fix --only FIX_CANONICALIZE --allow-data` does that on request.

## Identity and the state hash

A **row's hash** is the SHA-256 of its compact canonical rendering with object
members sorted, so neither member order nor formatting is part of it.

A **schema's identity** is the SHA-256 of the RFC 8785 (JCS) canonical form of
its document with `title`, `description`, `$comment` and `examples` removed at
every depth.

A **table's digest** is SHA-256 over its rows in path order, each as path, a NUL
byte, the row hash and a newline. The **root** is SHA-256 over
`reldir-state-v2` and a NUL byte, then `format` and `config` with their entries'
hashes, then each table in name order with its schema identity and digest.

## History

A revision is a JSON object:

| Member | |
|---|---|
| `revision` | its number, from 1 |
| `timestamp` | RFC 3339, UTC |
| `previous_revision`, `previous_root_hash` | its predecessor; absent for the first |
| `new_root_hash` | the root of the state it records |
| `origin` | `internal`, `external`, `recovery`, `repair`, `migration`, `import` or `snapshot_restore` |
| `changes` | path → `{ "kind", "hash" }` for each entry added or changed, `null` for each removed |
| `binary_version`, `format_version` | what wrote it |
| `transaction_id` | the transaction that made the change, for changes reldir made |
| `lineage` | for the first revision of a new lineage: `{ "quarantined", "reason" }` |

An entry's `kind` is `format`, `config`, `schema` (keyed `schema/<table>.json`
wherever the schema lives) or `row`. History verifies when each revision names
its predecessor, every object it adds exists and hashes to its entry, and each
recorded root is the root of the entries replayed to that point.

## Transactions

`.db/transactions/<uuid>/` holds `staged/` (the new bytes, one file per written
path), `journal.json` (`{ "id", "origin", "changes": [{ "path", "stage" }] }`,
with a null `stage` for a removal) and, once committing, `COMMITTING`. The
protocol and recovery are described in [Transactions]({{ site.baseurl }}/transactions).

## Schemas

A schema is a JSON Schema 2020-12 document in the dialect
`https://reldir.dev/schema/reldir-2`, described in full in
[Schemas]({{ site.baseurl }}/schemas). `reldir schema dialect` prints the dialect's
meta-schema.
