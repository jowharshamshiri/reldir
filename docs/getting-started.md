---
title: Getting started
---

# Getting started

## Install

```sh
cargo install reldir
```

Or from a checkout: `cargo install --path . --locked`.

## Point it at a folder

A folder of JSON files is already a database. Ask it something and reldir
infers a schema for each directory, validates every row, records the first
revision, and answers:

```console
$ ls
posts
users
$ reldir sql 'SELECT id, name FROM users ORDER BY id'
initialized the database, inferring schemas for posts, users
recorded revision 1 (import): 8 change(s)
id    | name
------+------
ada   | Ada
grace | Grace
(2 rows)
2 row(s)
```

Inference is all or nothing: if any table cannot be inferred, the command fails
and writes nothing, so you are never left with a half-made database. `reldir
init --adopt` does the same work as a deliberate step, which is what a script
or a CI job wants.

Inference declares what the data shows: each column's type, which columns every
row has, the primary key, and references whose column names say what they
point at -- `user_id` is a reference to `users` when every value is a user's
key:

```console
$ reldir schema show posts
{
  "$schema": "https://reldir.dev/schema/reldir-2",
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
  "x-reldir": {
    "table": "posts",
    "primaryKey": [
      "id"
    ],
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

That schema is a working schema in `.db/schema/`. `reldir schema pin posts`
moves it to `schema/posts.json`, where it becomes a declaration you own, edit
and keep in version control.

## Change data

Through SQL, or through the row commands. Every change is judged as the state
it would produce before anything is written:

```console
$ reldir insert users '{"id":"alan","name":"Alan"}'
action | path            | planned
-------+-----------------+--------
write  | users/alan.json | false
(1 row)
inserted: 1 file(s), revision 2
$ reldir delete users ada
error[FOREIGN_KEY_VIOLATION]: deleting users/ada.json refused: posts/p1.json still references it at /user_id (posts.user_id -> users, onDelete restrict)
  --> posts/p1.json:3:14
[..]
```

The delete was refused, and nothing was written: `posts/p1.json` still names
`ada`, and its foreign key says a referenced user may not be deleted. A schema
can say otherwise -- `cascade`, `remove`, `set_null` -- and reldir then carries
out the action in the same change and reports every row it touched.

`--dry-run` shows what any change would do, validated, without writing it.

## Edit the files directly

Your editor, a script and `git merge` are all welcome. The next command judges
what it finds:

```console
$ echo '{"id": "p3", "user_id": "nobody", "title": "Draft"}' > posts/p3.json
$ reldir check
error[FOREIGN_KEY_VIOLATION]: /user_id references ["nobody"] in users, and no such row exists
  --> posts/p3.json:1:25
[..]
INVALID: 2 table(s), 6 row(s), 1 violation(s), 0 warning(s), 0 lint finding(s), [..] ms
$ rm posts/p3.json
$ reldir status
VALID   revision 2   root [..]
```

A valid edit is recorded as a new revision; an invalid one is reported, located
to the line and column, and never recorded. See [Validation]({{ site.baseurl }}/validation) for
`lint` and `doctor`, which repair what can be repaired, least destructive first.

## Output for scripts

Every command speaks JSON with `--format json`: one `command_result` envelope
with `ok`, `exit`, `summary`, `records`, `diagnostics` and `events`. Off a
terminal the default is JSON Lines.

```console
$ reldir --format jsonl sql 'SELECT id FROM users ORDER BY id'
{"id":"ada","kind":"row"}
{"id":"alan","kind":"row"}
{"id":"grace","kind":"row"}
{"kind":"command_result","command":"sql","ok":true,"exit":0,"summary":"3 row(s)","rows":3,"database_valid":true}
```

## Next

- [Schemas]({{ site.baseurl }}/schemas): write one by hand; references, domains, checks, assertions
- [SQL]({{ site.baseurl }}/sql): what queries and changes can say
- [CLI reference]({{ site.baseurl }}/cli): every command
