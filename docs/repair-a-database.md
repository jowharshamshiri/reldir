---
title: How to repair an invalid database
---

# How to repair an invalid database

This guide takes a database that `reldir check` reports `INVALID` back to
`VALID`, using the repairs `reldir doctor` proposes. Every fault, fix and
option it uses is listed in the [validation reference]({{ site.baseurl }}/validation).

## Find the faults

Run `reldir check`. Each fault names the file, the line and column, the value
at fault and, under `fixes`, the repairs that apply:

```console
$ rm users/grace.json
$ reldir check
error[FOREIGN_KEY_VIOLATION]: /user_id references ["grace"] in users, and no such row exists
  --> posts/p2.json:3:14
   |
 3 |   "user_id": "grace",
   |              ^
   = at: /user_id
   = constraint: posts.user_id -> users (onDelete: restrict)
   = expected: an existing row of users
   = observed: ["grace"]
   = fixes: FIX_RESTORE_TARGET, FIX_ORPHAN_DELETE_ROW
run `reldir doctor` for repairs, least destructive first
INVALID: 3 table(s), 6 row(s), 1 violation(s), 0 warning(s), 0 lint finding(s), [..] ms
```

If the database is invalid because of an interrupted transaction or history
that does not verify (exit `5` or `6`), doctor cannot help: follow
[How to recover]({{ site.baseurl }}/recover) instead.

## See the repairs

Run `reldir doctor`. It writes nothing; it lists every repair, least
destructive first, and marks the one `--fix` would apply as `default`:

```console
$ reldir doctor
apply with `reldir doctor --fix`; choose an alternative with --only <FIX_ID>
id                    | class | description                                                                               | resolves              | at            | default | destructive | paths
----------------------+-------+-------------------------------------------------------------------------------------------+-----------------------+---------------+---------+-------------+---------------------
FIX_RESTORE_TARGET    | data  | restore users/grace.json from revision #1 ([..]), which posts/p2.json still names | FOREIGN_KEY_VIOLATION | posts/p2.json | true    | false       | ["users/grace.json"]
FIX_ORPHAN_DELETE_ROW | data  | delete posts/p2.json, which holds the dangling reference                                  | FOREIGN_KEY_VIOLATION | posts/p2.json | false   | true        | ["posts/p2.json"]
(2 rows)
0 fix(es) would apply, 0 need a decision, 2 alternative(s) in all; 1 rewrite rows and need --allow-data
```

To know exactly what a fix does before you choose it, run
`reldir doctor --explain FIX_ID`.

## Choose a repair other than the default

If the default is not what you want -- here, the post should go rather than
the user come back -- name the repair with `--only`. A repair that rewrites
rows needs `--allow-data`, and one that removes data needs `--yes`:

```console
$ reldir doctor --fix --allow-data --only FIX_ORPHAN_DELETE_ROW --yes
snapshot pre-doctor-1 taken; `reldir snapshot restore pre-doctor-1 --yes` puts everything back
[..]
action | path          | planned
-------+---------------+--------
delete | posts/p2.json | false
(1 row)
repaired: 1 file(s), revision #2 ([..]) (1 fix(es): FIX_ORPHAN_DELETE_ROW)
```

To see what a repair would write first, add `--dry-run`.

## Undo a repair

Before it rewrites a row or renames a file, doctor takes a snapshot and names
it. If the repair was the wrong one, restore that snapshot: the database
returns to exactly the state before the repair, faults included.

```console
$ reldir snapshot restore pre-doctor-1 --yes
action | path          | planned
-------+---------------+--------
write  | posts/p2.json | false
(1 row)
restored: 1 file(s), revision #2 ([..])
```

## Apply the default repairs

Run `reldir doctor --fix --allow-data` to apply every fault's default repair.
Without `--allow-data`, only repairs to schemas and file names are applied:

```console
$ reldir doctor --fix --allow-data
snapshot pre-doctor-2 taken; `reldir snapshot restore pre-doctor-2 --yes` puts everything back
[..]
action | path             | planned
-------+------------------+--------
write  | users/grace.json | false
(1 row)
repaired: 1 file(s), revision #3 ([..]) (1 fix(es): FIX_RESTORE_TARGET)
$ reldir check
VALID: 3 table(s), 7 row(s), 0 violation(s), 0 warning(s), 1 lint finding(s), [..] ms
```

If doctor says a fix was deferred because another fix touched the same file,
run it again.

If `--fix` exits `9` (`DECISION_REQUIRED`), the repairs left rewrite rows: add
`--allow-data`. If it exits with a fault's own status after repairing, faults
remain that it did not address: run `reldir doctor` again to see them.

## Repair a misnamed file

A file whose name is not the one its row's key gives is `IDENTITY_MISMATCH`.
The repair renames the file and leaves its contents alone, so `--fix` applies
it without `--allow-data`:

```console
$ echo '{"id": "p4", "user_id": "ada", "title": "Notes"}' > posts/draft.json
$ reldir doctor --fix
snapshot pre-doctor-3 taken; `reldir snapshot restore pre-doctor-3 --yes` puts everything back
[..]
action | path             | planned
-------+------------------+--------
delete | posts/draft.json | false
write  | posts/p4.json    | false
(2 rows)
repaired: 2 file(s), revision #4 ([..]) (1 fix(es): FIX_RENAME_TO_IDENTITY)
```

If the name is right and the key is wrong, edit the key in the file instead.

## When doctor has no repair

A fault whose only fix is `FIX_MANUAL` needs a person. The fix's description
says what to look at; edit the file, then run `reldir check` again.

- **`MERGE_CONFLICT`**: a git merge left conflict markers in a row file. Keep
  one side of each conflict, delete the markers, and finish the merge. To have
  git merge row files member by member, so that fewer conflicts reach you, see
  [How to merge and diff row files with git]({{ site.baseurl }}/merge-with-git).
- **`INVALID_JSON`**: the file is not JSON. Fix it in an editor at the line
  and column the fault names, or delete it.
- **`UNEXPECTED_FILE`** or **`UNGOVERNED_DIRECTORY`**: a file or directory
  reldir does not recognise. Move it out of the folder, add it to `ignore` in
  [`.db/config`]({{ site.baseurl }}/configuration), or, for a directory of
  rows, [declare a table]({{ site.baseurl }}/declare-a-table) for it.

## When the fault is a warning

`CONTRACT_EXCEEDED` is a warning, not a fault: a program outside reldir wrote
a value the table's contract has never admitted, and the database stays
valid. To admit it, see [How to write a value a table's contract does not
admit]({{ site.baseurl }}/widen-a-contract).
