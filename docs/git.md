---
title: Git integration
---

# Git integration

`reldir git` gives git a merge driver and a diff driver for row files, and
`reldir diff --git` compares the files with any git revision. To set a
repository up and resolve a conflict, see
[How to merge and diff row files with git]({{ site.baseurl }}/merge-with-git);
to check a database in a pipeline, see
[How to check a database in continuous integration]({{ site.baseurl }}/check-in-ci).

```sh
reldir git setup [--hooks]
reldir git merge-file BASE OURS THEIRS [--path P] [--labels B O T] [--marker-size N]
reldir git textconv FILE
reldir diff --git REV [TABLE]
```

## Setup

`reldir git setup` writes two things, and reports them.

A block of `.gitattributes` in the database root, naming every table's row
files. The block is reldir's; the rest of the file is left as it is, and
running setup again -- after a table is added -- replaces the block:

```text
# reldir: begin -- row files merge member by member and diff as JSON pointers
/posts/*.json merge=reldir diff=reldir
/tags/*.json merge=reldir diff=reldir
/users/*.json merge=reldir diff=reldir
# reldir: end
```

The two drivers, in the repository's local git config:

```text
merge.reldir.name=reldir: merge row files member by member
merge.reldir.driver=reldir --quiet --format table git merge-file %O %A %B --path %P --labels %S %X %Y --marker-size %L
diff.reldir.textconv=reldir --format table git textconv
```

Git does not version its config, so the drivers exist only in a clone where
setup has run. Elsewhere, git merges and diffs row files line by line. Git
runs `reldir` from the `PATH` it runs with.

`--hooks` also writes `post-merge` and `post-checkout` hooks that run
`reldir --readonly check` on the database after every merge and checkout.
A check that fails does not fail the git command. Setup refuses to replace a
hook it did not write, and says what to add to it instead.

## The merge driver

Git calls the merge driver with three versions of a row file: the common
ancestor, ours and theirs. The driver merges them as JSON documents, member by
member:

- a member one side changed and the other left alone takes the change;
- a member both sides changed alike takes it once;
- an object both sides changed is merged member by member, the same way, at
  any depth;
- anything else -- a value or an array both sides changed differently, a
  member one side removed and the other changed -- is a conflict.

Two versions that change different members of one row merge cleanly:

```console
$ echo '{"id": "ada", "name": "Ada", "email": "ada@example.com"}' > base.json
$ echo '{"id": "ada", "name": "Ada Lovelace", "email": "ada@example.com"}' > ours.json
$ echo '{"id": "ada", "name": "Ada", "email": "ada@example.org"}' > theirs.json
$ reldir git merge-file base.json ours.json theirs.json --path users/ada.json
users/ada.json merged member by member
$ cat ours.json
{"id": "ada", "name": "Ada Lovelace", "email": "ada@example.org"}
```

A clean merge is written as edits to our version: every byte it does not
change keeps its spelling, so the file keeps its indentation, member order and
number formatting. Members only their side added follow ours, in their order.

### Conflicts

A member both sides changed differently is marked alone -- not the file --
with git's diff3 markers, and the driver exits with `MERGE_CONFLICT`, naming
every conflicting member by its JSON Pointer:

```console
$ echo '{"id": "ada", "name": "Ada", "age": 36}' > base.json
$ echo '{"id": "ada", "name": "Ada", "age": 37}' > ours.json
$ echo '{"id": "ada", "name": "Ada", "age": 38}' > theirs.json
$ reldir git merge-file base.json ours.json theirs.json --path users/ada.json --labels base HEAD feature
error[MERGE_CONFLICT]: users/ada.json: both sides changed /age differently; each is marked in the file
   = help: keep one side of each conflict and delete its markers
error[MERGE_CONFLICT]: both sides changed /age differently
   = at: /age
$ cat ours.json
{"id": "ada", "name": "Ada"
<<<<<<< HEAD
, "age": 37
||||||| base
, "age": 36
=======
, "age": 38
>>>>>>> feature
}
$ rm base.json ours.json theirs.json
```

Each side's section carries its own comma, so any one side of each conflict,
with the markers deleted, is valid JSON. Where every member of an object
conflicts, the object is marked whole. Markers are `--marker-size` characters
long; git passes its own (`%L`).

The markers name the sides with `--labels`: git passes the base, our branch
and theirs (`%S`, `%X`, `%Y`) from version 2.44. A git before 2.44 passes the
placeholders as they are, and the driver then names the sides `base`, `ours`
and `theirs`. The merge is the same either way.

A row file holding conflict markers is not JSON. `reldir check` reports it as
`MERGE_CONFLICT` on that file, not as a parse error, and reads and checks the
rest of the database.

### Files that are not JSON

When any of the three versions is not a JSON document -- it holds markers from
an earlier merge, or was never JSON -- the driver says so and merges the file
as text, with `git merge-file --diff3`, as git does.

### Constraints across files

The driver sees one file at a time. Two branches can each add or change rows
validly and together break a constraint -- a unique value both add, a row one
deletes and the other's new row refers to. Every file merges cleanly; the
database is invalid, and `reldir check` reports the fault on the files
involved.

## The diff driver

`reldir git textconv` prints a row file as one line per value -- its JSON
Pointer and the value -- in document order:

```console
$ reldir git textconv posts/p1.json
/id = "p1"
/user_id = "ada"
/title = "Engines"
/tags/0 = "machines"
/tags/1 = "history"
```

With setup done, `git diff`, `git log -p` and `git show` diff those lines, so
a diff names the values that changed:

```diff
diff --git a/posts/p1.json b/posts/p1.json
--- a/posts/p1.json
+++ b/posts/p1.json
@@ -1,5 +1,5 @@
 /id = "p1"
 /user_id = "ada"
-/title = "Engines"
+/title = "Engines of Analysis"
 /tags/0 = "machines"
 /tags/1 = "history"
```

A file that is not JSON is shown as it is.

## Diff against a git revision

`reldir diff --git REV [TABLE]` compares the files with a commit, branch or
tag, row by row and member by member, for every table and schema, or for one
table. Nothing is checked out.

```sh
reldir diff --git main
reldir diff --git HEAD~3 users
```

```text
change  | pointer | old     | new                 | path
--------+---------+---------+---------------------+--------------
changed | /title  | Engines | Engines of Analysis | posts/p1.json
(1 row)
1 change(s)
```

## What git versions

`.db/.gitignore` decides what of `.db/` is versioned. By default it is:

| Versioned | Not versioned |
|---|---|
| `.db/format`, `.db/config`, the inferred contracts in `.db/schema/` | recorded history, the mirror, caches, transactions in progress, the lock |

The unversioned parts belong to one clone; each clone builds its own from the
files.

A database made with `reldir init --track-provenance` also versions recorded
history: `.db/objects/pack/` and `.db/history/heads/`, `refs/` and
`quarantine/`. Every history file is named by its content, so two branches'
histories merge in git without conflict. A merged history has two heads, which
a command that records nothing -- such as a `--readonly` one -- reports as the
warning `HISTORY_DIVERGED`. The next command that records joins them in a
revision of origin `merge` that follows both. Every history object is verified
against its hash when it is read.

## The check action

`.github/actions/check` in the reldir repository is a composite GitHub Action.
It builds reldir from its own source at the ref the workflow names it by, runs
`reldir --db PATH --readonly --format sarif check`, prints the same check as a
table in the job log, uploads the SARIF log to code scanning, and ends with the
check's exit status.

| Input | Default | Is |
|---|---|---|
| `path` | `.` | the database's root, relative to the repository |
| `strict` | `false` | `true` adds `--strict`: warnings and lint findings fail too |
| `upload` | `true` | upload the results as code scanning alerts (needs `security-events: write`) |
