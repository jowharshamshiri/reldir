---
title: History and snapshots
---

# History and snapshots

reldir records every state the database is accepted in as a **revision**, and
can name any state as a **snapshot**. Both live in `.db/`, in one
content-addressed store: a row that did not change between two revisions, or
between a revision and a snapshot, is stored once. The layout is described in
[the storage format]({{ site.baseurl }}/format#history). Undoing changes with
them is described in [How to undo a change]({{ site.baseurl }}/undo-a-change).

## Revisions

Every change reldir commits is recorded as a revision by the same transaction
that makes it. A revision holds:

| Member | |
|---|---|
| parents | the revision or revisions it follows; none for the first of a lineage |
| state | the identity of the database's state: rows, schemas and settings |
| tree | every row and schema of that state, by content hash |
| timestamp | when it was made |
| origin | how it was made |
| idempotency key | the key the change was made under, if any |

| Origin | |
|---|---|
| `inference` | the folder adopted, its schemas inferred |
| `user` | SQL, the row commands, widening a contract |
| `external` | an edit made outside reldir, recorded when next seen |
| `migration` | a schema change |
| `repair` | `reldir doctor --fix` |
| `snapshot_restore` | a snapshot put back |
| `merge` | histories joined |
| `import` | `reldir import` |
| `upgrade` | the state a format 2 database was upgraded in |
| `recovery`, `internal` | recovery, and reldir's own bookkeeping |

An edit made outside reldir that leaves the database valid is recorded, with
origin `external`, by the next command that observes the folder -- any command
but a `--readonly` one; a writing command records it before making its own
change. An outside edit that leaves the database invalid is not recorded; the
state is recorded once it is valid. Formatting is not state: a file
reformatted without a change of value is the same state.

### Numbers and ids

Each clone numbers its revisions from `#1`, oldest first. A revision's id is
the SHA-256 of its canonical form, the same in every clone. A command that
takes a revision accepts its number (`12` or `#12`) or the beginning of its id.
An unknown revision is `UNKNOWN_REVISION`.

### Heads and lineages

The newest revisions are *heads*. A database has one head, or two after
history kept in version control is merged: that is the warning
`HISTORY_DIVERGED`, and the next recording writes a `merge` revision that
follows both. A merge revision's `rows` count what changed from the state this
clone had recorded before the merge -- what the merge brought in. A *lineage*
is one history from its first revision.

## Reading history

| Command | Shows |
|---|---|
| `reldir log [--limit N]` | revisions, newest first |
| `reldir show REV` | the rows a revision added, modified or removed |
| `reldir diff` | what changed since the last revision and is not yet recorded |
| `reldir diff --from A --to B` | what changed between two revisions |
| `reldir diff TABLE` | the same, for one table |
| `reldir diff --schema` | schema changes only |
| `reldir diff --git REV` | the files compared with a git commit, branch or tag; see [git]({{ site.baseurl }}/git) |

`diff` lists each changed member by JSON Pointer with its old and new value,
and each added, removed or renamed row by path; `change` says which. It reads
only the parts of the two states' trees that differ.

```console
$ reldir sql "UPDATE posts SET title = 'Engines of Analysis' WHERE id = 'p1'"
action | path          | planned
-------+---------------+--------
write  | posts/p1.json | false
(1 row)
committed: 1 file(s), revision #2 ([..])
$ reldir show 2
action   | path
---------+--------------
modified | posts/p1.json
(1 row)
revision #2 ([..]) (user, [..]): 1 row(s) changed
$ reldir diff --from 1 --to 2
change  | pointer | old     | new                 | path
--------+---------+---------+---------------------+--------------
changed | /title  | Engines | Engines of Analysis | posts/p1.json
(1 row)
1 change(s)
```

## Restoring rows

`reldir restore PATH... [--at REV]` writes row files back byte for byte as
history recorded them: each from the revision `--at` names, or else from the
newest revision that holds it. The files are written as one change, judged
like any other and recorded with origin `user`; a file that already holds
those bytes is not written. Each path is a row file as `show` and `diff` name
it, `<table>/<file>.json`, of a table that exists. A path the revision -- or
every revision -- does not hold is `UNKNOWN_ROW`.

```console
$ reldir restore posts/p1.json --at 1
path          | revision
--------------+------------------
posts/p1.json | #1 ([..])
(1 row)

action | path          | planned
-------+---------------+--------
write  | posts/p1.json | false
(1 row)
restored: 1 file(s), revision #3 ([..])
```

## Snapshots

A snapshot is a name for the whole database as it is: every row, every
schema, the configuration -- and every file that is not a valid row, so a
snapshot of a database with faults holds the faults too.

| Command | Does |
|---|---|
| `reldir snapshot create NAME` | names the database as it is |
| `reldir snapshot list` | lists the snapshots |
| `reldir snapshot restore NAME` | makes the database what the snapshot holds |
| `reldir snapshot delete NAME` | removes the name |

```console
$ reldir snapshot create before-cleanup
took snapshot before-cleanup
$ reldir snapshot list
name           | created [..] | valid | revision
[..]
before-cleanup | [..] | true  | #3 ([..])
(1 row)
1 snapshot(s)
```

`valid` says whether the database was valid when the snapshot was taken, and
`revision` is the revision it was taken at.

A snapshot of the recorded, valid state names a tree history already holds,
and costs one small file. A snapshot of any other state stores only what
history does not already hold. Creating a snapshot is a transaction: it
exists whole, or not at all.

### Names

Snapshot names are lowercase letters, digits, `-`, `_` and `.`, beginning with
a letter or digit, at most 128 bytes. Any other name is `PATH_VIOLATION`. A
name already taken is `SNAPSHOT_EXISTS`; an unknown one is
`UNKNOWN_SNAPSHOT`.

`reldir doctor --fix` takes a snapshot named `pre-doctor-<revision>` before a
repair that rewrites rows or renames files, unless `--no-snapshot` is given.

### Restore

A restore writes every file whose bytes differ from the snapshot's, byte for
byte as the snapshot holds them, and removes every governed file the snapshot
does not hold. It is one transaction, validated like any other, with one rule
of its own: the result must be valid, or be exactly the state the snapshot
recorded. A snapshot with `valid: false` puts its faults back with the state,
and no others.

A restore that leaves the database valid is recorded as a revision with origin
`snapshot_restore`; one that puts faults back is recorded, like any invalid
state, once it is made valid. When the files already hold the snapshot, a
restore writes nothing and asks nothing. Otherwise restoring and deleting ask
for confirmation, or `--yes`; `--dry-run` shows what a restore would change:

```console
$ reldir sql "DELETE FROM posts WHERE id = 'p2'"
action | path          | planned
-------+---------------+--------
delete | posts/p2.json | false
(1 row)
committed: 1 file(s), revision #4 ([..])
$ reldir snapshot restore before-cleanup
error[DECISION_REQUIRED]: restore snapshot before-cleanup, changing 1 file(s) -- this needs a decision
   = help: pass --yes to confirm, or --dry-run to see exactly what would change
$ reldir snapshot restore before-cleanup --yes
action | path          | planned
-------+---------------+--------
write  | posts/p2.json | false
(1 row)
restored: 1 file(s), revision #5 ([..])
```

A snapshot taken by another format is `FORMAT_UNSUPPORTED`.

### Delete

```console
$ reldir snapshot delete before-cleanup --yes
deleted snapshot before-cleanup
```

Deleting a snapshot removes its name; what it alone kept is removed by the
next `reldir gc`.

## History in version control

By default `.db/.gitignore` keeps history out of version control. A database
made with `reldir init --track-provenance` keeps it in: every history file is
named by its content, so two clones' histories merge in git without a
conflict, and the merged history has two heads until the next recording joins
them. See [git]({{ site.baseurl }}/git).

## Garbage collection

Objects nothing reaches -- left by a deleted snapshot, or by a crash between
writing objects and naming them -- stay on disk until `reldir gc` removes
them. It keeps everything a head, a snapshot or a quarantined lineage
reaches, and holds the writer lock while it runs. `--dry-run` says what it would remove.

```console
$ reldir gc
removed [..] object(s) nothing reaches; kept [..]
```

## Verification

Every object is verified against its id when it is read, and every command
verifies history's heads as it opens the database. `reldir check --deep`
verifies all of it: every stored object against its id, every revision
reachable from the heads, and every state tree recomputed from its rows.

History that does not verify is `INTERNAL_METADATA_CORRUPT` (exit `6`), and
no change is recorded until it is repaired. `reldir recover --history
new-lineage --allow-destructive` moves the heads aside to
`.db/history/quarantine/<stamp>/`, with the reason, and every pack that fails
verification to `.db/history/quarantine/<stamp>/packs/`, then records the
current files as the first revision of a new lineage, with origin `recovery`.
The new lineage holds fresh copies of everything it names. The old history
stays on disk, and `gc` keeps everything it reaches through the packs still in
use. See
[How to recover]({{ site.baseurl }}/recover).
