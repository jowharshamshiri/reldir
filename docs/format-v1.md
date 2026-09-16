---
title: On-disk format
---

# reldir on-disk format 1

This document fixes the representation emitted and accepted by format version 1.

## Authoritative namespaces

`.db/schema/<table>.json` contains the working schema, as a JSON Schema 2020-12
document in the dialect described in [Schemas]({{ site.baseurl }}/schemas). `schema/<table>.json`,
when present, contains a pinned declaration in the same dialect; the two are
byte-identical when both exist.
`<table>/<filename-key>.json` contains one JSON-object row. Table and schema names
are lowercase ASCII identifiers. Row filename components are the canonical textual
column values encoded byte-for-byte as UTF-8: ASCII letters, digits, `.`, `_`, and
`-` remain literal (except a leading `.`); every other byte is `%HH` with uppercase
hex. A single-component key that would form a Windows reserved device basename
(`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, or `LPT1`–`LPT9`, case-insensitively)
has its first byte percent-encoded. Composite components are joined by a comma and
`.json` is appended.

Symlinks, sockets, FIFOs, devices, and hard-linked files are rejected in governed
schema and table namespaces. On platforms that do not expose link counts through
the standard filesystem API, a hard link is read as an ordinary regular file and
identity remains exclusively path-and-body based. Sparse regular files are read as
their complete logical byte stream; holes therefore behave as zero bytes and will
normally produce `INVALID_JSON`. Case-folded or NFC-normalized path collisions are
rejected independently of host filesystem behavior.

`.db/config` and `.db/format` are authoritative operational/interpretation state.
`.db/provenance/*.json` is the local committed transition history. The manifest,
indexes, statistics, lock, and transaction working directories are derived or
ephemeral. `.db/objects/<sha256>.json` holds content-addressed canonical revision
objects used by semantic historical diff; objects referenced by retained provenance
or snapshots are retained by garbage collection.
When `--track-provenance` is selected, both provenance and its referenced object
store are unignored so a clone retains a complete, verifiable history.

## Canonical logical state and hashing

Rows are normalized to schema column order. Extra fields, when permitted, follow in
lexicographic order. Nested object keys are lexicographic; strings are NFC; integer
and floating values use their shortest lossless JSON representation; timestamps are
UTC RFC 3339. Binary writes use the `.db/config` `indentation_width` (two spaces by
default) and one trailing newline. Because configuration is authoritative, clones
of the same state retain identical output formatting.

The state root is SHA-256 over `reldir-state-v1\0`, the canonical format/config entries,
then (in lexicographic table and primary-key order) each normalized relative path,
a NUL byte, and the lowercase hex SHA-256 of the canonical row, or of the schema's
semantic encoding. Formatting-only row or configuration changes do not alter the
logical hash.

A schema's identity follows its relational content rather than the file that
carries it: the digest is taken over a private, versioned encoding of the model,
not over the JSON Schema document on disk. Changing how a schema is *written*
therefore cannot change what it *is*. Column order does, because it decides the
byte order of every row, and so does anything deciding which rows are valid:
types, nullability, defaults, generators, enumerated values, `pattern`, and
whether an object column admits undeclared members. Annotations do not.

## Metadata

`.db/format` is UTF-8 text containing `format_version = 1`. `.db/manifest.json`
contains `format_version`, `revision`, `root_hash`, and a path-keyed entry map. Each
entry has `kind`, `hash`, and source size. Provenance filenames are zero-padded
20-digit revisions and record previous/new roots, origin, observed paths, binary
version, format version, and UTC timestamp.

## Commit and recovery protocol

A writer exclusively locks `.db/lock`, rechecks its observed starting root, and
constructs a fully validated prospective directory before committing. It writes
replacement bytes beneath `.db/transactions/<uuid>/staged`, fsyncs them, writes and
fsyncs `journal.json`, then writes the `COMMITTING` marker. Each authoritative path
is replaced with a same-directory temporary file plus atomic rename, or deleted,
and its parent directory is synced. The resulting full state is validated before
the manifest and provenance revision are atomically replaced. `COMPLETE` ends the
transaction.

Recovery removes journals that never reached `COMMITTING`. A journal with that
marker is idempotently rolled forward from immutable staged bytes. A malformed
journal fails with `TRANSACTION_INCOMPLETE`; it is never guessed through.

## Compatibility

Format 1 readers reject every other format number with `FORMAT_UNSUPPORTED`.
Schema dialect changes that alter interpretation require a new format or an
explicit `x-reldir.schemaFormat`. `reldir upgrade-format` is a no-op only when format 1 is
already current; no implicit forward interpretation occurs.

## SQL subset

The SQL frontend accepts one statement and supports `SELECT`, `INSERT`, `UPDATE`,
`DELETE`, `WHERE`, inner/left joins, grouping, `HAVING`, ordering, limit/offset,
distinct, `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, and SQLite-style scalar expressions.
`EXPLAIN QUERY PLAN` powers `reldir explain` and `reldir sql --explain`. JSON parameters are
bound separately from SQL text. The ephemeral SQLite engine keeps temporary storage
in memory; `max_query_memory` bounds its loaded relational workspace and
`max_sort_memory` bounds queries requiring sorting, while temporary-disk use is
therefore zero.

## Declarative migrations

`reldir migrate apply` accepts an object with one required `operations` array. Each
operation has an `op` discriminator and the same fields as its CLI counterpart:
`add_table` (whose embedded `schema` is itself a JSON Schema document), `drop_table`,
`rename_table`, `add_column`, `drop_column`, `rename_column`, `change_type`,
`add_constraint`, `drop_constraint`, `add_index`, and `drop_index`. Constraints use
a `definition` with `kind` equal to `unique`, `foreign_key`, or `check`. The complete
array is evaluated in order against one prospective state and committed as one
transaction; intermediate states need not validate, but the final state must.
