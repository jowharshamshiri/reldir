---
title: Transactions
---

# Transactions and safety

Every change reldir makes is one transaction: it lands whole, or not at all,
and a crash at any instant leaves the files either exactly as they were or
exactly as the change makes them.

## The commit path

```text
take the writer lock (.db/lock), waiting up to wait_seconds
        ↓
finish any interrupted transaction, observe the files again
        ↓
record a valid outside edit made since the command began, as its own revision
        ↓
plan the change against what is on disk now
        ↓
complete it with the referential actions its foreign keys declare
        ↓
judge the state it would produce, from memory -- no copy of the database is made
        ↓
write it through the recoverable protocol below
        ↓
record the revision, release the lock
```

The judgement is an observation of the directory as the change leaves it, so
once the change is written that observation is the new state: it is kept, not
made again. Observation itself re-reads only files whose size, times or inode
moved -- or were modified within two seconds of being read, where a timestamp
cannot prove a file unchanged -- and what changed since the last revision is
tracked as it happens, so a one-row change costs a stat of each file, not a
read of each file.

A change planned before the lock was taken -- a repair, a migration -- names
the bytes each path held when it was planned; if any moved, the change is
refused with `CONCURRENT_MODIFICATION` and nothing is written. SQL and the row
commands plan under the lock, so concurrent writers simply queue.

## The protocol

Renaming many files is not atomic on any real filesystem, so a transaction is
made *recoverable* instead:

```text
stage every new file's bytes; fsync each             ─┐
write the journal; fsync; rename into place             │ nothing visible changed:
fsync the transaction directory                         │ recovery discards it
write COMMITTING; fsync; fsync the directory           ─┘
for each change: write a synced copy beside the target ─┐ partly visible:
  and rename it over, or remove the target;              │ recovery rolls it forward
  fsync its directory                                    │ from the staged bytes
remove COMMITTING; fsync                               ─┘
remove the transaction directory                        ─ complete
```

Every step goes through one filesystem interface, and the test suite runs the
protocol against a simulated filesystem in which only fsynced data under
fsynced directory entries survives, crashing it at every single operation and
injecting I/O errors, full disks and torn writes: every outcome recovers to the
state before or the state after.

## Recovery

Recovery runs at the start of any writing command, under the lock, and says
what it did. `reldir recover` runs it explicitly.

- A transaction without `COMMITTING` never began to apply: it is discarded.
- One with `COMMITTING` is rolled forward from its staged bytes; every step is
  idempotent, so a crash during recovery is recovered from too.
- A journal that cannot be read under a `COMMITTING` marker is
  `TRANSACTION_INCOMPLETE`: it is never guessed through.

A reader (`--readonly`) never recovers. It answers normally while a transaction
is merely staged, and refuses with `RECOVERY_REQUIRED` only while one is
partway through its renames, when the rows on disk are part old and part new.

## Concurrency

Many readers, one writer.

- `--readonly` takes no lock and writes nothing, so readers never wait for each
  other or for a writer.
- A writing command waits for the lock for `wait_seconds` (5 by default, or
  `--wait`), polling with jittered backoff, and then fails with `LOCK_CONTENDED`
  (exit `3`). `--wait 0` tries once. There is no "wait forever": a stuck writer
  never becomes a hung caller.
- The lock is an advisory lock the kernel releases when its holder dies, so a
  crashed writer never strands it.

All three refusals below exit `3`, and in each **nothing was written**:

| Code | What happened | What to do |
|---|---|---|
| `LOCK_CONTENDED` | another writer held the lock past the wait | retry; always safe |
| `CONCURRENT_MODIFICATION` | a file the change was planned against moved | retry; it is planned again |
| `PATH_INTERFERENCE` | a directory the change needs is no longer one | look at it; a retry meets the same path |

## Filesystems

The protocol relies on two promises of a local POSIX filesystem: an exclusive
lock excludes every other writer, and a rename replaces its target atomically.
Network filesystems (NFS, SMB/CIFS, AFS, Ceph, Lustre, 9p) and FUSE-based sync
clients often keep neither -- a lock silently local to one machine, a rename
emulated as copy-then-delete -- and a database that trusted them would lose
changes without an error.

So reldir asks the operating system what the filesystem is, and refuses to
*write* to one of those with `UNSAFE_FILESYSTEM` (exit `11`) unless
`.db/config` sets `"allow_remote_filesystem": true`: a decision someone made
about a filesystem they know. Reading is always allowed; on such a filesystem
reads simply take no lock and write no derived state.

## Snapshots

```sh
reldir snapshot create before-import
reldir snapshot list
reldir --yes snapshot restore before-import
reldir --yes snapshot delete before-import
```

A snapshot is a complete copy of the authoritative files -- rows, pins, working
schemas, configuration -- including files that are not valid rows, so a
snapshot taken before a repair can put back exactly what the repair changed. It
is built beside its destination and renamed into place, so it is complete or
absent. Restoring is an ordinary validated transaction with origin
`snapshot_restore`.

## History

Recorded history is kept so that it verifies: each revision names its
predecessor and root, every object it references is checked against its hash,
and a revision's root must be the root of the entries it describes. Objects are
written first and made durable together, the revision after them; objects a
crash leaves unreferenced are removed by `reldir gc`, and an object found
damaged is simply written again, because an object is named by its content.

## Derived state

`.db/mirror.sqlite` is an index over the files: rebuildable from them at any
moment, never consulted to decide what a row *is*. A mirror that is missing,
from another layout or unreadable is rebuilt, and the command says so. Deleting
it changes how fast the next command is, never its answer.
