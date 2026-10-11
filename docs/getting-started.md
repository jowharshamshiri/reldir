---
title: Getting started
---

# Getting started

In this tutorial we will make a small blog's data -- two users and their posts
-- as a folder of JSON files, and turn that folder into a database. We will
ask it questions in SQL, change it, watch it refuse changes that would break
it, edit its files by hand, repair a mistake, and look back through its
history.

You need the `reldir` command; [How to install reldir]({{ site.baseurl }}/install)
shows how to get it. Begin in a new, empty folder:

```sh
mkdir blog
cd blog
```

## Make a folder of JSON

First, make a directory for each table:

```console
$ mkdir users posts
```

Now write one file for each row. Every user has an `id` and a `name`, and every
post names the user who wrote it:

```console
$ echo '{"id": "ada", "name": "Ada Lovelace"}' > users/ada.json
$ echo '{"id": "grace", "name": "Grace Hopper"}' > users/grace.json
$ echo '{"id": "p1", "user_id": "ada", "title": "Notes on the Engine"}' > posts/p1.json
$ echo '{"id": "p2", "user_id": "grace", "title": "Compilers"}' > posts/p2.json
```

Let's check that the files are where we expect:

```console
$ ls
posts
users
$ ls users
ada.json
grace.json
$ cat posts/p1.json
{"id": "p1", "user_id": "ada", "title": "Notes on the Engine"}
```

So far this is an ordinary folder, and nothing about it is special to reldir.

## Ask the folder a question

Ask for the users, in SQL:

```console
$ reldir sql 'SELECT id, name FROM users ORDER BY id'
initialized the database, inferring schemas for posts, users
recorded revision #1 ([..]) (inference): 8 change(s)
id    | name
------+-------------
ada   | Ada Lovelace
grace | Grace Hopper
(2 rows)
2 row(s)
```

Notice the two lines before the answer. reldir had never seen this folder, so
it *adopted* it first: it worked out a schema for each directory from the
files in it, and recorded the folder as it is as revision #1 of its history.
It changed none of our files to do that.

Now list the tables:

```console
$ reldir tables
table | rows | primary_key | pinned
------+------+-------------+-------
posts | 2    | ["id"]      | false
users | 2    | ["id"]      | false
(2 rows)
2 table(s)
```

Each directory is a table, each file a row, and `id` is each table's key.

## See what reldir learned

Let's look at the schema reldir inferred for `posts`:

```console
$ reldir schema show posts
{
  "$schema": "https://reldir.dev/schema/reldir-3",
  "type": "object",
  "properties": {
    "id": {
      "type": "string"
    },
    "user_id": {
      "type": "string"
    },
    "title": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "user_id",
    "title"
  ],
  "additionalProperties": false,
[..]
    "foreignKeys": [
      {
        "from": [
          "user_id"
        ],
        "to": {
          "table": "users"
        },
        "onDelete": "restrict",
        "onUpdate": "restrict"
      }
    ]
  }
}
```

Every post has the same three members, all strings, so the schema requires
exactly those. And because every `user_id` names a user, `posts.user_id` is a
*reference* to `users`. This shape -- what the files actually hold -- is the
table's *contract*: the shape any other program reading these files relies
on. reldir will keep to it. [Concepts]({{ site.baseurl }}/concepts#the-observed-contract)
says more about the contract when you want it.

## Change some rows

Add a user with a row command:

```console
$ reldir insert users '{"id":"alan","name":"Alan Turing"}'
id   | name
-----+------------
alan | Alan Turing
(1 row)

action | path            | planned
-------+-----------------+--------
write  | users/alan.json | false
(1 row)
inserted: 1 file(s), revision #2 ([..])
$ cat users/alan.json
{"id": "alan", "name": "Alan Turing"}
```

Notice that the new file is written on one line, with the same spacing as the
files we wrote ourselves. reldir writes a table's files in the style its files
already have.

Now change a post's title in SQL:

```console
$ reldir sql "UPDATE posts SET title = 'Notes on the Analytical Engine' WHERE id = 'p1'"
action | path          | planned
-------+---------------+--------
write  | posts/p1.json | false
(1 row)
committed: 1 file(s), revision #3 ([..])
$ cat posts/p1.json
{"id": "p1", "user_id": "ada", "title": "Notes on the Analytical Engine"}
```

Each change became a revision of its own: #2, then #3.

## Watch reldir refuse a change

Ada wrote a post, so let's try to delete her:

```console
$ reldir delete users ada
error[FOREIGN_KEY_VIOLATION]: deleting users/ada.json refused: posts/p1.json still references it at /user_id (posts.user_id -> users, onDelete restrict)
  --> posts/p1.json:1:25
[..]
```

reldir refused, and the error points at the exact place -- line 1, column 25
of `posts/p1.json` -- that still names her.

No user has ever been without a name. Let's try to take Grace's away:

```console
$ reldir sql "UPDATE users SET name = NULL WHERE id = 'grace'"
error[CONTRACT_WIDENING_REQUIRED]: the change would write what the contract of users has never admitted: "name" cannot be null; nothing was written
  --> users/grace.json:1:25
[..]
$ cat users/grace.json
{"id": "grace", "name": "Grace Hopper"}
```

The file is exactly as it was. A program reading `users/*.json` has never
seen a null name, so reldir will not write one.

## Make two changes as one

Kurt is new, and has a post ready. If we add the post first, it names a user
who does not exist yet:

```console
$ reldir sql "INSERT INTO posts (id, user_id, title) VALUES ('p3', 'kurt', 'Incompleteness')"
error[FOREIGN_KEY_VIOLATION]: the change was refused because the database would be invalid (1 fault); nothing was written. First: /user_id references ["kurt"] in users, and no such row exists
  --> posts/p3.json:1:25
[..]
```

So give reldir both statements at once. A script is one change: reldir judges
the state the statements make together, at the end:

```console
$ reldir sql "INSERT INTO posts (id, user_id, title) VALUES ('p3', 'kurt', 'Incompleteness'); INSERT INTO users (id, name) VALUES ('kurt', 'Kurt Gödel')"
action | path            | planned
-------+-----------------+--------
write  | posts/p3.json   | false
write  | users/kurt.json | false
(2 rows)
committed: 2 file(s), revision #4 ([..])
$ ls posts
p1.json
p2.json
p3.json
```

Both files arrived together, as revision #4.

## Edit a file by hand

The files are still ours. Let's retitle Grace's post with nothing but `echo`:

```console
$ echo '{"id": "p2", "user_id": "grace", "title": "The First Compiler"}' > posts/p2.json
$ reldir status
recorded revision #5 ([..]) (external): 1 change(s)
VALID_CHANGED_EXTERNALLY   revision #5 ([..])
```

reldir noticed the edit the next time it looked, found the folder still
valid, and recorded the edit as revision #5, made `external`ly.

## Repair a mistake

Now a mistake: we delete Grace's file by hand, forgetting that her post still
names her.

```console
$ rm users/grace.json
$ reldir check
error[FOREIGN_KEY_VIOLATION]: /user_id references ["grace"] in users, and no such row exists
  --> posts/p2.json:1:25
   |
 1 | {"id": "p2", "user_id": "grace", "title": "The First Compiler"}
   |                         ^
   = at: /user_id
   = constraint: posts.user_id -> users (onDelete: restrict)
   = expected: an existing row of users
   = observed: ["grace"]
   = fixes: FIX_RESTORE_TARGET, FIX_ORPHAN_DELETE_ROW
run `reldir doctor` for repairs, least destructive first
INVALID: 2 table(s), 6 row(s), 1 violation(s), 0 warning(s), 0 lint finding(s), [..] ms
```

The folder is invalid, and reldir says where and why. Ask the doctor what it
can do:

```console
$ reldir doctor
apply with `reldir doctor --fix`; choose an alternative with --only <FIX_ID>
id                    | class | description                                                                               | resolves              | at            | default | destructive | paths
----------------------+-------+-------------------------------------------------------------------------------------------+-----------------------+---------------+---------+-------------+---------------------
FIX_RESTORE_TARGET    | data  | restore users/grace.json from revision #5 ([..]), which posts/p2.json still names | FOREIGN_KEY_VIOLATION | posts/p2.json | true    | false       | ["users/grace.json"]
FIX_ORPHAN_DELETE_ROW | data  | delete posts/p2.json, which holds the dangling reference                                  | FOREIGN_KEY_VIOLATION | posts/p2.json | false   | true        | ["posts/p2.json"]
(2 rows)
0 fix(es) would apply, 0 need a decision, 2 alternative(s) in all; 1 rewrite rows and need --allow-data
```

The first fix, the default, brings Grace back from history. It writes a row,
so we allow that:

```console
$ reldir doctor --fix --allow-data
snapshot pre-doctor-5 taken; `reldir snapshot restore pre-doctor-5 --yes` puts everything back
[..]
action | path             | planned
-------+------------------+--------
write  | users/grace.json | false
(1 row)
repaired: 1 file(s), revision #5 ([..]) (1 fix(es): FIX_RESTORE_TARGET)
$ cat users/grace.json
{"id": "grace", "name": "Grace Hopper"}
$ reldir check
VALID: 2 table(s), 7 row(s), 0 violation(s), 0 warning(s), 2 lint finding(s), [..] ms
```

Grace's file is back, byte for byte. Notice the revision number: the repair
put the folder back into a state history already holds, revision #5, so it
needed no new one.

## Look back through history

Every revision remembers what changed. Let's see what our hand edit did:

```console
$ reldir diff --from 4 --to 5
change  | pointer | old       | new                | path
--------+---------+-----------+--------------------+--------------
changed | /title  | Compilers | The First Compiler | posts/p2.json
(1 row)
1 change(s)
```

And what the script added:

```console
$ reldir show 4
action | path
-------+----------------
added  | posts/p3.json
added  | users/kurt.json
(2 rows)
revision #4 ([..]) (user, [..]): 2 row(s) changed
```

## What we built

We started from four JSON files that any program could have written. Without
changing one of them, we made them a database with a schema per table and a
reference between them; we queried it in SQL; we changed it through reldir
and by hand; we saw it refuse a deletion that would leave a dangling
reference, a value its readers had never seen, and a post by a user who did
not exist yet; and we repaired a deleted file from a history that records
every state the folder has been in.

From here:

- [Concepts]({{ site.baseurl }}/concepts) explains the ideas we met: the
  contract, identity, history.
- The how-to guides take on real tasks, such as
  [putting an existing folder under reldir]({{ site.baseurl }}/adopt-a-folder),
  [declaring a table]({{ site.baseurl }}/declare-a-table),
  [writing a value the contract does not admit]({{ site.baseurl }}/widen-a-contract),
  [repairing a database]({{ site.baseurl }}/repair-a-database) and
  [undoing a change]({{ site.baseurl }}/undo-a-change).
- The [CLI]({{ site.baseurl }}/cli), [SQL]({{ site.baseurl }}/sql) and
  [schema]({{ site.baseurl }}/schemas) references describe every command,
  statement and rule.
