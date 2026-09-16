---
title: Transactions
---

# Transactions and safety

Every change `jdb` makes is transactional. A failed or interrupted write never
leaves a partially committed logical state.

## The write sequence

```text
validate the starting state
        ↓
acquire writer coordination (.db/lock)
        ↓
construct the complete mutation set
        ↓
validate the prospective state
        ↓
stage the replacement bytes and fsync them
        ↓
write and fsync the journal
        ↓
write the COMMITTING marker
        ↓
replace each authoritative file by atomic rename; sync parents
        ↓
validate the resulting state
        ↓
refresh derived state, record provenance
        ↓
COMPLETE
```

Renaming many files is not globally atomic on real filesystems, so the protocol
is built to be recoverable instead. After any crash, the state of a transaction
is determinable: not committed, fully committed, or partially materialised but
recoverable.

## Recovery

```sh
db recover
```

Recovery normally happens automatically when it is unambiguous and safe, and
prints a one-line notice when it does:

```console
$ db status
recovered an interrupted committed transaction
VALID   revision 3   root 71ab3c9d   external changes: none
```

- A journal that never reached `COMMITTING` is discarded, because the
  transaction had not begun materialising.
- A journal with `COMMITTING` is rolled forward idempotently from the immutable
  staged bytes.
- A malformed journal fails with `TRANSACTION_INCOMPLETE` rather than being
  guessed through.

A journal is replayed only if it is internally consistent: its id must be a UUID
matching its directory, its origin must be one of the known origins, and its
change set must be unambiguous, with no repeated paths, no shared staged objects,
and no paths that escape the database root.

## Concurrency

The model is multiple concurrent readers, one binary-managed writer.

**Readers.** `--readonly` works from an in-memory observation and writes
nothing: no derived state, no provenance. Any number may run at once.

```sh
db --readonly sql 'SELECT * FROM users'
```

Read-only mode is selected automatically when `.db/` is not writable, and
reports when metadata is stale but the authoritative state is valid.

**Writers.** A default (non-`--readonly`) invocation may record an externally
observed revision and refresh derived state, so it takes the writer lock, even
for a query. If another writer holds it, the command fails with
`CONCURRENT_MODIFICATION` and exit code `3` rather than interleaving.

> For many concurrent reads, pass `--readonly`. It is faster and not subject to
> lock contention.

## External writers

External tools cannot be made to take the lock, so binary transactions use
optimistic validation:

```text
observe root R1
        ↓
plan the mutation against R1
        ↓
stage it
        ↓
re-observe the authoritative state
        ↓
still compatible with R1?  ── yes ──> commit
                            └── no ──> CONCURRENT_MODIFICATION, naming the changed paths
```

If files changed underneath a transaction in a way that invalidates its
assumptions, it aborts and names the paths that moved, rather than losing the
update silently.

An external edit that arrives *before* a mutation is planned is adopted as a new
revision first, so editing one row by hand and then updating a different one
through the CLI both succeed.

## Snapshots

```sh
db snapshot create before-import
db snapshot list
db snapshot restore before-import
db snapshot delete before-import
```

A snapshot preserves the logical database state. Restoration is transactional and
records provenance with origin `snapshot_restore`. `doctor` creates one
automatically before any fix that touches data or layout.

## Derived state is disposable

Indexes, statistics, and the manifest can always be deleted and rebuilt from the
authoritative files:

```sh
db reindex
db analyze
```

A corrupt or stale index never makes valid JSON unrecoverable, and changes only
how fast a query is computed, not its answer.

## Internal metadata corruption

Authoritative data and internal metadata are treated differently:

- Corrupt **derived** metadata is rebuilt, and the command says so.
- Corrupt **provenance** is reported; history is never fabricated.
- Invalid **authoritative** rows or schemas make the database `INVALID`.
