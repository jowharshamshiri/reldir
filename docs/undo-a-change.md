---
title: How to undo a change
---

# How to undo a change

This guide puts back what a change did: the files one change wrote, a row it
deleted, or the whole database as it was at a moment you named. Every command it uses is
described in [History and snapshots]({{ site.baseurl }}/snapshots).

## Before a change you may want to undo, take a snapshot

A snapshot names the whole database as it is -- rows, schemas, settings, and
files with faults -- so that it can be put back exactly:

```console
$ reldir snapshot create before-cleanup
took snapshot before-cleanup
```

Name it for what you are about to do. `reldir doctor` takes one by itself
before every repair that rewrites rows or renames files.

Then make the change. Here a tag is deleted, and its foreign key removes it
from the posts that carry it; then a title is changed:

```console
$ reldir sql "DELETE FROM tags WHERE id = 'history'"
remove posts/p1.json (posts.tags[] -> tags of ["history"] in tags/history.json)
action | path              | planned
-------+-------------------+--------
delete | tags/history.json | false
write  | posts/p1.json     | false
(2 rows)
committed: 2 file(s), revision #2 ([..])
$ reldir sql "UPDATE posts SET title = 'Engines of Analysis' WHERE id = 'p1'"
action | path          | planned
-------+---------------+--------
write  | posts/p1.json | false
(1 row)
committed: 1 file(s), revision #3 ([..])
```

## Find the change

`reldir log` lists revisions, newest first; `--limit N` shows the last few.
`reldir show` lists the rows a revision changed, and `reldir diff` shows the
values -- the old one beside the new:

```console
$ reldir show 3
action   | path
---------+--------------
modified | posts/p1.json
(1 row)
revision #3 ([..]) (user, [..]): 1 row(s) changed
$ reldir diff --from 2 --to 3
change  | pointer | old     | new                 | path
--------+---------+---------+---------------------+--------------
changed | /title  | Engines | Engines of Analysis | posts/p1.json
(1 row)
1 change(s)
```

## Put files back as they were before the change

`reldir restore` writes files back byte for byte as a revision recorded them.
To undo revision 3, restore what it changed from the revision before it:

```console
$ reldir restore posts/p1.json --at 2
path          | revision
--------------+------------------
posts/p1.json | #2 ([..])
(1 row)

action | path          | planned
-------+---------------+--------
write  | posts/p1.json | false
(1 row)
restored: 1 file(s), revision #4 ([..])
```

A restored file is whole: anything written to it after that revision is undone
too. To undo only some members and keep later edits to the same file, write
the old values back instead -- `reldir update posts p1 '{"title": "Engines"}'`,
or one SQL statement or script for many rows.

## Bring back a deleted row

Without `--at`, each file comes from the newest revision that holds it:

```console
$ reldir restore tags/history.json
path              | revision
------------------+------------------
tags/history.json | #1 ([..])
(1 row)

action | path              | planned
-------+-------------------+--------
write  | tags/history.json | false
(1 row)
restored: 1 file(s), revision #5 ([..])
```

The delete also removed the tag from the posts that carried it, through the
foreign key's `remove` action. Restoring the tag does not put those references
back; restore the posts as well -- several files restore as one change:

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
restored: 1 file(s), revision #6 ([..])
```

A restore is judged like any other change: a row that references a row no
longer there is refused with `FOREIGN_KEY_VIOLATION`, and restoring both in one
command brings them back together.

## Put the whole database back

To return to a snapshot, restore it. Everything made since the snapshot is
undone with it -- every row, schema and setting becomes what the snapshot
holds -- so see first what the restore would change:

```console
$ reldir sql "UPDATE users SET name = 'Ada Lovelace' WHERE id = 'ada'"
action | path           | planned
-------+----------------+--------
write  | users/ada.json | false
(1 row)
committed: 1 file(s), revision #7 ([..])
$ reldir snapshot restore before-cleanup --dry-run
action | path           | planned
-------+----------------+--------
write  | users/ada.json | true
(1 row)
dry run: 1 file(s) would change; nothing was written
$ reldir snapshot restore before-cleanup --yes
action | path           | planned
-------+----------------+--------
write  | users/ada.json | false
(1 row)
restored: 1 file(s), revision #8 ([..])
```

The restore is itself a change, recorded as a revision, so it can be undone
the same way.

## Remove the snapshot

When you no longer need it, delete the snapshot. What only it kept is removed
from disk by the next `reldir gc`:

```console
$ reldir snapshot delete before-cleanup --yes
deleted snapshot before-cleanup
```
