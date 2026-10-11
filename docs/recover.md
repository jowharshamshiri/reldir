---
title: How to recover from an interrupted change or damaged metadata
---

# How to recover from an interrupted change or damaged metadata

This guide brings a database back into use after a command was interrupted --
killed, crashed, cut off by a power failure -- or after something in `.db/`
was lost or damaged. Faults in the rows themselves are repaired with doctor
instead; see [How to repair an invalid database]({{ site.baseurl }}/repair-a-database).
Why recovery works as it does is explained in
[transactions]({{ site.baseurl }}/transactions#recovery).

## After an interrupted command

Run any writing command, or `reldir recover`, which recovers and does nothing
else. An interrupted change is finished if it reached its commit point and
discarded if it did not; either way, the files end up exactly as they were
before the change or exactly as the change makes them:

```console
$ reldir recover
nothing to recover
```

When there was something to recover, it says what it did:

```text
recovered interrupted transactions: 1 finished, 0 discarded
recovered
```

If a file the change was writing was edited after the crash, the edit is kept,
and the message names the file; check that it holds what you want.

## When a change fails with exit 12

`COMMITTED_RECOVERY_PENDING` means the change was made, and every reader
already sees it; only finishing it on disk failed:

```text
error[COMMITTED_RECOVERY_PENDING]: transaction <id> is committed, but <what failed>
   = help: the change is made and every reader sees it; the next writing command or `reldir recover` finishes it on disk. Do not retry the change.
```

Do not run the change again. Fix what failed, if the message names something
-- a full disk, a permission -- and run `reldir recover`.

## When a change fails with exit 13

`COMMIT_UNKNOWN` means the change could not be made durable or retracted, so
whether it happened is not yet known. Run `reldir recover`: it finishes the
change if its journal reached the disk, and discards it otherwise. Then look --
`reldir log`, or a query -- before you run the change again; or, if the change
had an idempotency key, retry it with the same key, which makes it at most
once. See [How to retry a change without applying it twice]({{ site.baseurl }}/retry-safely).

## When recovery is blocked

`RECOVERY_BLOCKED` (exit `5`) means a committed change cannot be put in place,
and names the path and why:

```text
error[RECOVERY_BLOCKED]: committed transaction <id> cannot be finished: users/ada.json could not be placed (<why>)
   = help: the change stands and readers see it; make the path writable (or move what is in its way), then run `reldir recover`
```

Until it is resolved, every change is refused; reads still answer, and see the
change as made.

1. Read the reason. Typical ones are a directory created where the file must
   go, a file or directory made read-only, or a full disk.
2. Move what is in the way, restore write permission, or free space.
3. Run `reldir recover`.

## When an interrupted change cannot be read

`TRANSACTION_INCOMPLETE` (exit `5`) means recovery can neither finish nor
discard a transaction under `.db/transactions/`: its journal cannot be
interpreted, or its directory holds something no transaction stages. reldir
does not guess. The error names the transaction's directory:

```text
error[TRANSACTION_INCOMPLETE]: the transaction holds left-behind, which no transaction stages; look at it before recovery removes the transaction
  --> .db/transactions/01ARZ3NDEKTSV4RRFFQ69G5FAV
   = help: move .db/transactions/01ARZ3NDEKTSV4RRFFQ69G5FAV out of .db/transactions/ to let reldir run again. A journal in it lists every file the change wrote, with the bytes committed for each: compare them with the files, and make what is missing as an ordinary change
```

- If the message says the journal was written by another journal format,
  run `reldir recover` with the reldir binary that wrote it.
- Otherwise, move the named directory out of `.db/`, and run `reldir check`.
  The rows are judged as they stand; any part of the change already in place
  is an outside edit, recorded if it leaves the database valid and reported if
  it does not. To finish the change, compare the files with what the journal
  lists, and make what is missing as an ordinary change.

## When the mirror is damaged

`.db/mirror.sqlite` is derived from the files. A mirror that cannot be read is
rebuilt by the next command, which says so. If `reldir check --deep` reports
`INTERNAL_METADATA_CORRUPT` at `.db/mirror.sqlite`, delete it and the files
beside it:

```sh
rm -f .db/mirror.sqlite .db/mirror.sqlite-wal .db/mirror.sqlite-shm
```

The next command rebuilds it from the files. Nothing is lost.

## When history does not verify

`INTERNAL_METADATA_CORRUPT` at `.db/objects` or `.db/history` means recorded
history fails its hashes. Rows can still be read, but no change is recorded
until history is repaired.

- **If you have a backup** of the database, restore `.db/objects/` and
  `.db/history/` from it, and run `reldir check --deep`.
- **Otherwise**, begin a new lineage of history from the current files:

  ```sh
  reldir recover --history new-lineage --allow-destructive
  ```

  ```text
  history's heads were moved to .db/history/quarantine/
  began a new lineage at revision #4 (b195cf24a38f)
  ```

  The old history's heads are moved aside to
  `.db/history/quarantine/<stamp>/`, with the reason, and any pack that fails
  verification to `.db/history/quarantine/<stamp>/packs/`; nothing is deleted.
  `reldir log` lists the new lineage, which begins with the files as they are,
  and `reldir check --deep` verifies it.

## When `.db/format` is missing

A `.db/` without its format marker is refused with `FORMAT_MISSING`
(exit `6`), so that nothing it holds is mistaken for something else:

```console
$ rm .db/format
$ reldir status
error[FORMAT_MISSING]: .db declares no format version and holds state that cannot be rebuilt: config, history, objects
   = help: restore .db/format, or pass --rebuild-metadata to discard .db and establish it again from your files
```

Restore it -- from version control, a backup, or by writing it again:

```console
$ echo 'format_version = 3' > .db/format
$ reldir status
VALID   revision #1 ([..])
```

If `.db/` is beyond use, `--rebuild-metadata` on any writing command discards
it and adopts the folder again from its files. Everything the error lists is
lost: history, snapshots, the configuration, and the inferred contracts of
unpinned tables, which are inferred again from the files as they are now.
