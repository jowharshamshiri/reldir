---
title: How to merge and diff row files with git
---

# How to merge and diff row files with git

This guide sets up a git repository so that merges combine row files member
by member and diffs name the values that changed, then shows what to do when a
merge conflicts or leaves the database invalid. It assumes the database is in
a git repository and `reldir` is on the `PATH`. What the drivers do is
described in [Git integration]({{ site.baseurl }}/git).

## Set up the repository

1. From the database's root, run setup. Add `--hooks` to have every merge and
   checkout followed by a check of the database:

   ```sh
   reldir git setup --hooks
   ```

2. Commit the `.gitattributes` it wrote:

   ```sh
   git add .gitattributes
   git commit -m "merge and diff row files with reldir"
   ```

3. In every other clone, run `reldir git setup` once after cloning. Git does
   not version the driver configuration, so a clone without it merges row
   files line by line.

When you add a table, run `reldir git setup` again and commit
`.gitattributes`, so the new table's files use the drivers too.

## Merge

Merge as usual. Branches that change different members of the same row merge
cleanly:

```text
$ git merge feature
Auto-merging data/posts/p1.json
Merge made by the 'ort' strategy.
 data/posts/p1.json | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)
VALID: 3 table(s), 7 row(s), 0 violation(s), 0 warning(s), 1 lint finding(s), 26 ms
```

The last line is the `post-merge` hook's check. Without `--hooks`, run
`reldir check` yourself after every merge: a merge that every file accepts can
still break a constraint between files.

## If a row file conflicts

When both branches changed the same member differently, the merge stops and
the driver names the member:

```text
$ git merge retitle
error[MERGE_CONFLICT]: data/posts/p1.json: both sides changed /title differently; each is marked in the file
   = help: keep one side of each conflict and delete its markers
error[MERGE_CONFLICT]: both sides changed /title differently
   = at: /title
Auto-merging data/posts/p1.json
CONFLICT (content): Merge conflict in data/posts/p1.json
Automatic merge failed; fix conflicts and then commit the result.
```

Only that member is marked:

```text
{
  "id": "p1",
  "user_id": "ada"
<<<<<<< ours
, "title": "Engines, Analysed"
||||||| base
, "title": "Engines of Analysis"
=======
, "title": "The Analytical Engine"
>>>>>>> theirs
,
  "tags": [
    "machines"
  ]
}
```

1. Keep one side of each marked member, or write the value you want in its
   place, and delete the marker lines. Each side carries its own comma, so
   either side alone leaves valid JSON.
2. Check the database:

   ```sh
   reldir check
   ```

   A file you have not finished resolving is reported as `MERGE_CONFLICT`.
3. When the check is valid, conclude the merge:

   ```sh
   git add data/posts/p1.json
   git commit
   ```

If a file was not JSON on one of the branches, the driver merges it as text,
and the markers are git's own, around lines.

## If the merge leaves the database invalid

Two branches can each make valid changes that are invalid together: one
deletes a user, the other adds a post by that user. Git reports a clean merge,
and the check after it does not:

```text
$ git merge feature
Auto-merging data/posts/p1.json
Merge made by the 'ort' strategy.
 data/posts/p1.json | 2 +-
 data/posts/p3.json | 8 ++++++++
 2 files changed, 9 insertions(+), 1 deletion(-)
 create mode 100644 data/posts/p3.json
error[FOREIGN_KEY_VIOLATION]: /user_id references ["grace"] in users, and no such row exists
  --> posts/p3.json:3:14
   |
 3 |   "user_id": "grace",
   |              ^
   = at: /user_id
   = constraint: posts.user_id -> users (onDelete: restrict)
   = expected: an existing row of users
   = observed: ["grace"]
   = fixes: FIX_RESTORE_TARGET, FIX_ORPHAN_DELETE_ROW
run `reldir doctor` for repairs, least destructive first
INVALID: 3 table(s), 6 row(s), 1 violation(s), 0 warning(s), 0 lint finding(s), 25 ms
```

1. Decide which branch's intent wins, and repair the database to match:
   - to keep the new row, bring back the row it refers to:
     `reldir doctor --fix --allow-data --only FIX_RESTORE_TARGET`;
   - to keep the deletion, delete the row that refers to it:
     `reldir doctor --fix --allow-data --only FIX_ORPHAN_DELETE_ROW`;
   - otherwise change the rows yourself, with SQL or an editor.

   See [How to repair an invalid database]({{ site.baseurl }}/repair-a-database)
   for the other faults a merge can leave.
2. Commit the repair as a commit of its own, or amend the merge:

   ```sh
   git add -A
   git commit -m "reconcile the merge"
   ```

## If history is versioned too

In a database made with `reldir init --track-provenance`, each branch carries
its own revisions, and the merge brings both. The first command after the
merge that may write joins the two heads in one `merge` revision:

```text
$ reldir status
recorded revision #4 (b2872604dbbe) (merge): 1 change(s)
VALID_CHANGED_EXTERNALLY   revision #4 (b2872604dbbe)
$ reldir log --limit 1
revision          | parents                              | timestamp | origin | rows                                 | state
------------------+--------------------------------------+-----------+--------+--------------------------------------+-------------
#4 (b2872604dbbe) | #2 (7c74d0bd7786), #3 (d17c7fce2323) | ...       | merge  | {"added":1,"modified":0,"removed":0} | 9175df129594
(1 row)
1 of 4 revision(s)
```

A `--readonly` command records nothing, so until then it reports the two heads
as the warning `HISTORY_DIVERGED`.

Commit the history files that command wrote together with your next change,
as you commit rows:

```sh
git add -A
git commit -m "join merged history"
```

## Compare with another branch

To see how the rows differ from a branch, a tag or a commit without checking
it out:

```sh
reldir diff --git main
reldir diff --git v1.0 users
```

`git diff`, `git log -p` and `git show` already show row files as one line per
value in a clone where setup has run.
