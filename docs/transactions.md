---
title: Transactions
---

# Transactions

A change to a reldir database is rarely one file. An update writes a row and
records a revision; a delete may cascade through every row that references
the one deleted; a migration rewrites a schema and every row it reshapes. Yet
the folder is the database, read directly by people and programs that know
nothing of reldir, so there is no moment at which half a change may show.
This page is about how reldir makes a change of many files land whole or not
at all, and what that means when something goes wrong.

## One path for every change

Every change reldir makes -- a SQL statement or script, a row command, an
import, a migration, a repair, a snapshot restore -- takes the same path:

```text
take the writer lock (.db/lock), waiting up to wait_seconds
        ↓
finish any interrupted transaction
        ↓
observe the files; record a valid outside edit as its own revision
        ↓
plan the change against the files as they are now
        ↓
complete it with the referential actions its foreign keys declare
        ↓
judge the whole state it would make; refuse it if that state is invalid
        ↓
write it, and its revision, through the commit protocol
        ↓
release the lock
```

There is a reason for one path rather than one per command: every guarantee
reldir makes about a change -- that it is validated as a whole, that it keeps
each table's [contract]({{ site.baseurl }}/concepts#the-observed-contract),
that it is recorded in history, that it survives a crash -- is made once, in
one place, and cannot be forgotten by a command that writes files its own way.

The revision that records a change is written by the change's own
transaction. History and the files therefore never disagree: there is no
window in which the files hold a change history does not know, or the reverse.

### Outside edits come first

The files belong to whoever edits them. An edit made outside reldir is not a
fault to be overwritten but a change reldir did not make, so the next command
observes it and, when it leaves the database valid, records it as a revision
of its own, with origin `external`, before making its own change:

```console
$ echo '{"id": "logic"}' > tags/logic.json
$ reldir sql "UPDATE users SET name = 'Grace Hopper' WHERE id = 'grace'"
recorded revision #2 ([..]) (external): 1 change(s)
action | path             | planned
-------+------------------+--------
write  | users/grace.json | false
(1 row)
committed: 1 file(s), revision #3 ([..])
```

The change is then planned against the files as they are, including the
edit. Nothing made outside reldir is lost by a change made inside it.

### Refusing before writing

A change is refused, when it must be, before anything is written: a state it
would make invalid, a value its contract has never admitted, but also a change
the filesystem has no room for -- a directory where a file must go, a name
longer than the filesystem allows, a table on another device than `.db/`, more
bytes than `max_transaction_size`. Each of these is checked while planning,
because discovering it halfway through writing would leave a choice between
two bad outcomes: a partial change, or an undo that may itself fail.

## The commit point

No filesystem renames many files atomically. A change is made *recoverable*
instead: everything it will write is made durable first, then one small file,
the journal, is made durable, and that is the moment the change happens.

```text
stage every new file's bytes in .db/transactions/<id>/; fsync them  ─┐ nothing visible:
stage the revision's history files; fsync                            │ a crash discards
fsync the transaction directory                                      │ the transaction
write the journal, ending in its own SHA-256; fsync it and its dirs ─┘
── commit point ──
rename each staged file over its target, or remove the target       ─┐ readers see the
remove a dropped table's directory once it is empty                  │ new state; a crash
fsync every directory touched                                        │ is rolled forward
publish the history files; fsync                                     │
remove the journal, then the transaction directory                  ─┘
```

Before the commit point, a crash leaves a transaction directory with no valid
journal, which the next command discards: nothing happened. After it, the
journal says exactly what the change is, and the next command finishes it:
everything happened. There is no third outcome.

The journal names, for every file, the bytes its target held when the change
was planned and the bytes it will hold, by SHA-256 and size. That is what
makes finishing a change safe to repeat: recovery can tell a file already
placed from one still to place, and both from a file someone edited after the
crash. That last edit is kept and recorded as an outside edit, never
overwritten -- the same rule as before a change, applied after one.

### Durability, and why a failed sync is never retried

A file is durable when the operating system has been made to put it on the
disk, which takes a different call on each system: `fdatasync` on Linux,
`F_FULLFSYNC` on macOS -- where a plain `fsync` leaves data in the drive's
cache -- and `FlushFileBuffers` on Windows. Staged files are flushed in
parallel; on Linux 5.8 and later, a change of 256 files or more is flushed
with one `syncfs`, which commits the filesystem's journal once rather than
once per file.

When a sync fails, the kernel may already have discarded the very data it
was flushing, and a second sync that succeeds proves nothing about it. This
was learned the hard way by databases that retried, and reldir does not.
Before the commit point, a failed sync abandons the transaction. After it,
the transaction is marked, and recovery writes its files afresh -- from the
staged copies, or from history -- before syncing them again.

The protocol is tested against a simulated filesystem that keeps only what was
made durable. The test suite crashes it at every operation, under crash models
that persist any subset of unsynced work, tears writes, fails syncs and drops
their data, and fills the disk; every outcome must recover to the state
before or the state after.

### A failure after the commit point

Most failures happen before the commit point, and mean that nothing was
written. A failure after it is different in kind, and is never reported as if
nothing happened. `COMMITTED_RECOVERY_PENDING` (exit `12`) says the change
was made -- every reader sees it -- and only finishing it on disk is
outstanding; retrying it would make it twice. `COMMIT_UNKNOWN` (exit `13`)
says the journal itself could not be made durable or retracted, so whether
the change happened cannot be known until recovery looks. A caller needs to
tell these apart from a refusal, which is why they have exit statuses of their
own; what to do about each is in
[How to retry a change without applying it twice]({{ site.baseurl }}/retry-safely)
and [How to recover]({{ site.baseurl }}/recover).

## Recovery

Recovery runs at the start of every command that takes the writer lock, so
an interrupted change is finished, or discarded, before anything is planned on
top of it. It never guesses: a journal that cannot be read is reported
(`TRANSACTION_INCOMPLETE`), not interpreted, and a committed file that cannot
be placed -- a directory now stands where it goes, the disk is full -- stops
every writer (`RECOVERY_BLOCKED`) until it can be, because no change may be
planned on a state that is still being put in place. Readers, meanwhile, see
the change as made: it was committed.

## Readers and writers

One writer at a time holds the writer lock, from observing the folder to
committing; other writers queue for it. Because each writer plans against the
state it commits to, two processes incrementing the same counter at once both
take effect: the second plans after the first has committed.

The wait is bounded. A writer waits up to `wait_seconds`, with jittered,
growing pauses, and then fails with `LOCK_CONTENDED`, naming the process that
holds the lock, its command, and since when. Waiting forever would turn one
stuck writer into any number of hung callers. The lock is an advisory lock
the kernel releases when its holder dies, so a crashed writer never strands
it. A [script]({{ site.baseurl }}/sql) or an open transaction holds the lock
from its first statement to its commit, and one held longer than
`transaction_timeout_seconds` is rolled back at its next statement, so a
forgotten session cannot hold every writer off.

Readers do not queue. A read takes the lock only if it is free, and only for
as long as it takes to observe the files; when a writer holds it, the read
answers at once from the state as of the writer's last commit, and says so
with `SNAPSHOT_READ`. It never sees half a change: the mirror the query runs
on is in SQLite's write-ahead mode, which gives every reader a consistent
state while a writer works. `--readonly` goes further: it takes no lock and
writes nothing at all, and sees a change that is committed but not yet
finished on disk as made.

A change planned before the lock is taken -- a repair, a migration, a plan
an agent reviews -- carries the bytes each file held when it was planned. If
any of them moved by the time it commits, it is refused
(`CONCURRENT_MODIFICATION`) rather than applied to a state nobody reviewed.

## Filesystems

The protocol rests on two promises: that an exclusive lock excludes every
other writer, and that a rename replaces its target atomically, across a
crash. A filesystem that does not keep them would lose changes without an
error, which is worse than refusing to write. So reldir asks the operating
system which filesystem holds the database, and refuses to write
(`UNSAFE_FILESYSTEM`) to one whose promises it cannot rely on:

- **network filesystems** -- NFS, SMB/CIFS, 9p, AFS, network drives on
  Windows -- where locks and renames are not coherent between the machines
  sharing them;
- **cluster filesystems** -- Ceph, Lustre, GPFS, BeeGFS -- where they depend
  on the cluster's lock manager and its configuration;
- **FUSE** -- sshfs, virtiofs, sync clients -- where a user-space server
  decides whether locks exclude and renames are atomic;
- **FAT** -- FAT32, exFAT -- which has no journal, so a replacing rename is
  not atomic across a crash;
- **shared folders** of virtual machines, such as VirtualBox's, which forward
  locks and renames to the host without guarantees;
- **anything unrecognised**, about which nothing is known.

ext4, XFS, Btrfs, ZFS, F2FS, bcachefs, tmpfs, overlayfs, APFS, HFS+, UFS, NTFS
and ReFS are local. Someone who knows that a particular filesystem keeps the
promises can say so with `allow_remote_filesystem` in
[`.db/config`]({{ site.baseurl }}/configuration). Reading is always allowed:
on such a filesystem a read takes no lock and writes no derived state.

## What is derived

`.db/mirror.sqlite`, the index SQL runs on, is never consulted to decide what
a row is: the files are. A mirror that is missing, from another layout, or
unreadable is rebuilt from them, and deleting it changes how fast the next
command is, never its answer. Keeping the authority in one place is what lets
every other piece of state be thrown away and rebuilt; see
[performance]({{ site.baseurl }}/performance).
