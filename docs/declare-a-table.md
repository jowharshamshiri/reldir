---
title: How to declare a new table
---

# How to declare a new table

This guide shows how to add a table to a database by declaring its schema
before any row exists. To bring an existing directory of JSON files under
reldir instead, see
[How to put an existing folder of JSON under reldir]({{ site.baseurl }}/adopt-a-folder).

## Start the table

`reldir schema new` declares a table with a string key `id`, as a pin in
`schema/`:

```console
$ reldir schema new comments
action | path                 | planned
-------+----------------------+--------
write  | schema/comments.json | false
(1 row)
declared: 1 file(s), revision #2 ([..])
```

The table has no directory yet. The first row inserted creates it.

## Add columns

Add each column with its type. While the table is empty, a column may be
required without a default:

```console
$ reldir migrate add-column comments post_id --type string
description
------------------------------------
add column comments.post_id (string)
(1 row)

action | path                 | planned
-------+----------------------+--------
write  | schema/comments.json | false
(1 row)
migrated: 1 file(s), revision #3 ([..])
$ reldir migrate add-column comments body --type string
[..]
migrated: 1 file(s), revision #4 ([..])
```

If rows may leave a column out or hold null in it, add it with `--nullable`.
The types are listed in the [schema reference]({{ site.baseurl }}/schemas#column-types).

## Reference another table

Add a foreign key, and say what happens to a comment when its post is deleted
or re-keyed. A foreign key that says nothing refuses both:

```console
$ reldir migrate add-constraint comments '{"kind": "foreign_key", "from": ["post_id"], "to": {"table": "posts"}, "onDelete": "cascade", "onUpdate": "cascade"}'
description
----------------------------
add a constraint to comments
(1 row)

action | path                 | planned
-------+----------------------+--------
write  | schema/comments.json | false
(1 row)
migrated: 1 file(s), revision #5 ([..])
```

The actions are listed under
[referential actions]({{ site.baseurl }}/schemas#referential-actions). Unique
keys, checks and assertions are added the same way, with a `kind` of `unique`,
`check` or `assertion`.

## Insert the first row

```console
$ reldir insert comments '{"id": "c1", "post_id": "p1", "body": "Which engine?"}'
id | post_id | body
---+---------+--------------
c1 | p1      | Which engine?
(1 row)

action | path             | planned
-------+------------------+--------
write  | comments/c1.json | false
(1 row)
inserted: 1 file(s), revision #6 ([..])
$ cat comments/c1.json
{
  "id": "c1",
  "post_id": "p1",
  "body": "Which engine?"
}
```

A row that names a post that does not exist is refused, and nothing is
written:

```console
$ reldir insert comments '{"id": "c2", "post_id": "p9", "body": "Lost"}'
error[FOREIGN_KEY_VIOLATION]: the change was refused because the database would be invalid (1 fault); nothing was written. First: /post_id references ["p9"] in posts, and no such row exists
[..]
```

## Write the schema yourself

If you would rather write the whole document -- a generated key, a format, a
nested object, rules the migration commands do not take -- write
`schema/<table>.json` in your editor. The file name and `x-reldir.table` are
the table's name. Then have reldir judge the schemas before you insert a row:

```console
$ echo '{"$schema": "https://reldir.dev/schema/reldir-3", "type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string", "minLength": 1}}, "required": ["id", "text"], "additionalProperties": false, "x-reldir": {"table": "notes", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}}' > schema/notes.json
$ reldir schema validate
recorded revision #7 ([..]) (external): 1 change(s)
5 schema(s), 0 fault(s)
$ reldir insert notes '{"id": "n1", "text": "first"}'
[..]
inserted: 1 file(s), revision #8 ([..])
```

A fault names the file, line and keyword, and an unknown keyword names the one
you probably meant. Every keyword is described in the
[schema reference]({{ site.baseurl }}/schemas).

If the document is in a file somewhere else, `reldir migrate add-table` writes
it as the table's pin, through the same validation:

```sh
reldir migrate add-table visits --from visits.json
```

## Keep the pin

`schema/<table>.json` is yours. Commit it beside the table's directory, so
every clone of the repository has the same rules.
