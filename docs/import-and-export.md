---
title: How to import and export rows in bulk
---

# How to import and export rows in bulk

This guide loads many rows into a table from a file or another program, and
writes a table's rows out for another tool. Every import is one change: all of
its rows are written, or none. The flags are listed in the
[CLI reference]({{ site.baseurl }}/cli#bulk-rows).

## Load new rows

Put the rows in a file -- a JSON array, JSON lines, or CSV -- and import it
into the table. The format follows the file's extension:

```console
$ echo '[{"id": "alan", "name": "Alan Turing"}, {"id": "kurt", "name": "Kurt Gödel"}]' > people.json
$ reldir import users --from people.json
action | path            | planned
-------+-----------------+--------
write  | users/alan.json | false
write  | users/kurt.json | false
(2 rows)
imported: 2 file(s), revision #2 ([..]); 2 row(s) read: 2 inserted, 0 updated, 0 deleted, 0 unchanged
```

Each row becomes a file named by its key, written in the table's style, as if
it had been inserted on its own.

## Load rows, some of which exist

By default every imported row must be new. A row whose key the table already
holds refuses the whole import:

```console
$ reldir import users --from people.json
error[PRIMARY_KEY_VIOLATION]: users/alan.json already exists, and the change would replace it
  --> users/alan.json
   = help: update the existing row instead, or choose a different key
```

To update the rows that exist and insert the rest, import with
`--mode upsert`. An existing row takes the members the imported row states,
and keeps the others:

```console
$ echo '{"id": "alan", "name": "A. M. Turing"}' > people.jsonl
$ reldir import users --from people.jsonl --mode upsert
action | path            | planned
-------+-----------------+--------
write  | users/alan.json | false
(1 row)
imported: 1 file(s), revision #3 ([..]); 1 row(s) read: 0 inserted, 1 updated, 0 deleted, 0 unchanged
```

## Make a table exactly what a file holds

To make the table hold the imported rows and no others, import with
`--mode replace`. A row the file does not name is deleted, and every
reference to it is resolved by its foreign key's `onDelete` action -- which
may change, or refuse, rows of other tables. Look first with `--dry-run`:

```console
$ echo '[{"id": "machines"}, {"id": "languages"}]' > tags.json
$ reldir --dry-run import tags --from tags.json --mode replace
remove posts/p1.json (posts.tags[] -> tags of ["history"] in tags/history.json)
action | path              | planned
-------+-------------------+--------
delete | tags/history.json | true
write  | posts/p1.json     | true
(2 rows)
dry run: 2 file(s) would change; nothing was written; 2 row(s) read: 0 inserted, 0 updated, 1 deleted, 2 unchanged
```

Here deleting the `history` tag also removes it from the post that names it.
If that is what you want, import without `--dry-run`:

```console
$ reldir import tags --from tags.json --mode replace
remove posts/p1.json (posts.tags[] -> tags of ["history"] in tags/history.json)
action | path              | planned
-------+-------------------+--------
delete | tags/history.json | false
write  | posts/p1.json     | false
(2 rows)
imported: 2 file(s), revision #4 ([..]); 2 row(s) read: 0 inserted, 0 updated, 1 deleted, 2 unchanged
```

If a reference's action is `restrict`, the replace is refused instead, and
nothing is written.

## If a row is refused

The import is judged whole, so one bad row refuses all of them and nothing is
written. The error names the row's file and the rule it breaks:

```console
$ echo '[{"id": "barbara", "name": "Barbara Liskov"}, {"id": "edsger"}]' > more.json
$ reldir import users --from more.json
error[ROW_MISSING_FIELD]: the change was refused because the database would be invalid (1 fault); nothing was written. First: required field "name" is absent
  --> users/edsger.json:1:1
   |
 1 | {
   | ^
   = constraint: /required
$ ls users
ada.json
alan.json
grace.json
kurt.json
```

Correct the row in the file, and import again. If the row holds a value the
table's contract has never admitted, see
[How to write a value a table's contract does not admit]({{ site.baseurl }}/widen-a-contract).

## Load CSV

A CSV file's header names the table's columns; a header naming any other
column is refused with `UNKNOWN_COLUMN`. Each cell becomes its column's type:
text columns take the cell as written, other columns read it as JSON -- so an
array is written `"[""machines""]"` -- and an empty cell in a column that
admits null is null.

```sh
reldir import users --from new-users.csv
```

## Load from another program

Pipe the rows in with `--from -`, and name their format, since there is no
extension to go by:

```sh
curl -s https://example.com/users.jsonl | reldir import users --from - --input-format jsonl --mode upsert
```

## Retry an import safely

If an import may have failed after it was committed -- a lost connection, a
killed job -- give it an `--idempotency-key`, and run it again with the same
key. See [How to retry a change without applying it twice]({{ site.baseurl }}/retry-safely).

## Export a table

To write a table's rows to a file, name the file. The format follows its
extension, or `--as` names it:

```console
$ reldir export users --out users.csv
4 row(s) to users.csv
$ cat users.csv
id,name
ada,Ada
alan,A. M. Turing
grace,Grace
kurt,Kurt Gödel
```

The file appears whole or not at all, and an export never replaces a file
that exists.

To export some rows, in some order, add a SQL condition and a column:

```console
$ reldir export posts --where "user_id = 'ada'" --order -title --out ada.jsonl
1 row(s) to ada.jsonl
$ cat ada.jsonl
{"id":"p1","user_id":"ada","title":"Engines","tags":["machines"]}
```

For a tool that reads Parquet, export to a `.parquet` file; the columns are
typed from the table's schema, as the
[CLI reference]({{ site.baseurl }}/cli#bulk-rows) describes.

To hand the rows to another program instead of a file, leave out `--out` and
choose a machine format. Each row is a `row` record, and a `result` record
ends the output (see [the machine protocol]({{ site.baseurl }}/protocol)):

```sh
reldir --format jsonl export users | jq -r 'select(.type == "row") | .row.name'
```

From Python, pandas reads rows through the library's DB-API connection; see
[Python]({{ site.baseurl }}/python#pandas).
