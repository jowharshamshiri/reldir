---
title: Storage format
---

# Storage format 3

This page fixes what a database is on disk: the files a person writes, and the
metadata reldir keeps beside them in `.db/`. A binary reads exactly its own
format. A database of format 2 is refused with `FORMAT_UPGRADE_REQUIRED` until
`reldir upgrade` converts it; any other version is `FORMAT_UNSUPPORTED`, never
interpreted.

```console
$ cat .db/format
format_version = 3
```

## The folder

| Path | What |
|---|---|
| `<table>/<name>.json` | one row: one JSON object |
| `schema/<table>.json` | a pinned schema: the table's rules, declared |
| `.db/format` | `format_version = 3` |
| `.db/config` | settings; see [configuration]({{ site.baseurl }}/configuration) |
| `.db/schema/<table>.json` | a working schema: the contract observed for a table nobody pinned |
| `.db/objects/pack/` | history's objects, in packs |
| `.db/history/heads/<id>` | one empty file per tip of history |
| `.db/history/refs/snapshots/<name>` | a [snapshot]({{ site.baseurl }}/snapshots) |
| `.db/history/quarantine/<stamp>/` | heads moved aside when a new lineage began (`heads/`, `reason.json`), and the packs that failed verification then (`packs/`) |
| `.db/mirror.sqlite` | derived: the relational index (layout `reldir-mirror-5`) |
| `.db/cache/` | derived: the scan marker, and the folder monitor's cookies |
| `.db/serve.json` | ephemeral: how to reach a running folder monitor |
| `.db/transactions/<id>/` | ephemeral: a transaction's staged files and journal |
| `.db/lock`, `.db/lock.holder` | ephemeral: the writer lock, and who holds it |
| `.db/.gitignore` | what of `.db/` version control keeps |

The rows, the pins, `.db/format`, `.db/config` and `.db/schema/` are the
database's state. History records it. Everything else in `.db/` is derived or
ephemeral, and is rebuilt or removed as needed: deleting `.db/mirror.sqlite`
or `.db/cache/` changes how fast the next command is, never its answer.

A table is a directory at the top of the folder whose name is a lowercase
identifier (`^[a-z][a-z0-9_]*$`), never `schema` and never a name Windows
reserves for a device.

### What the folder may hold

| Path | Is |
|---|---|
| a `.json` file in a table directory | a row |
| any other file, or a directory, in a table directory | `UNEXPECTED_FILE` |
| a symlink, socket, FIFO, device or hard-linked file where a row or schema belongs | `NON_REGULAR_FILE`; never followed or read |
| a row file holding a merge's conflict markers | `MERGE_CONFLICT` |
| a path matching an `ignore` pattern in `.db/config` (by default `.DS_Store`, `*~`, `*.swp`, `.gitkeep`) | not governed |
| a top-level directory that no schema governs and that holds anything | `UNGOVERNED_DIRECTORY`, a warning |
| an empty top-level directory; a file at the top of the folder | not governed |

## Row file names

A row's identity comes from its file name or its body, as its schema's
`x-reldir.identity` says; see [schemas]({{ site.baseurl }}/schemas). A folder
adopted with names of its own keeps them: reldir learns the convention its
files follow and names new rows the same way. When identity is in the body and
the names follow reldir's own rule (`"naming": {"rule": "reldir"}`), a file
name is built from the key's values as written:

- each value's UTF-8 bytes are written as they are when they are ASCII
  letters, digits, `_` or `-`, or a `.` that is neither first nor last; every
  other byte is `%HH`, in uppercase hex;
- several values are joined with `,`;
- a single value that is a Windows device name (`con`, `prn`, `aux`, `nul`,
  `com1`-`com9`, `lpt1`-`lpt9`, in any case, before any `.`) has its first
  byte encoded;
- `.json` is appended. A name longer than 255 bytes is refused with
  `FILENAME_TOO_LONG`.

The name follows the key as it is spelled: a string exactly as written, an
integer by its digits, a timestamp with the offset it was written with.

## Rows as written

reldir changes a row file only where its row changed. Whitespace, member order, the spelling of every number (`1.50`,
`1e2`), the escapes and Unicode form of every string, line endings, a byte
order mark and the trailing newline all stay as they were. New content is
written in the file's own style -- its indentation, spacing and line endings,
measured from the file. A new file is written in the style the table records
in `x-reldir.style`, or, for a new table, indented two spaces with one member
per line and a final newline.

A file reldir did not change is never rewritten, and an edit that would not
change a value under its type -- the same number with other digits, the same
instant in another offset -- is not made.

## Identity

A **row's hash** is the SHA-256 of its compact JSON with object members
sorted by name and numbers in their canonical decimal spelling (`1.50` is
`1.5`); strings and member names are hashed exactly as written. Neither
formatting nor member order is part of it.

A **file's blob id** is the SHA-256 of its exact bytes.

A **schema's identity** is SHA-256 over `reldir-schema-v2`, a NUL byte, and the
document's RFC 8785 (JCS) canonical form with every annotation removed --
`title`, `description`, `$comment`, `examples`, `deprecated`, `readOnly`,
`writeOnly`, and `x-` members that are not reldir's -- wherever they are
keywords. A property named `description` is a column, not an annotation, and
counts. Two documents share an identity exactly when they impose the same
rules.

The **configuration's hash** is that of its compact JSON with members sorted;
the **format's** is that of its trimmed text and a newline.

## History

History is a content-addressed object store. Three kinds of object exist,
each named by the SHA-256 of its bytes:

- a **blob** (kind 1): a file's original bytes, so any recorded file can be
  put back byte for byte;
- a **node** (kind 2): one level of a state's Merkle tree;
- a **revision** (kind 3): one accepted state, and the revisions it followed.

Every object read is checked against its id.

### State trees

A state is a root over tables; a table is 64 mids of 64 leaves; a row lives in
the leaf its path names: the first 12 bits of SHA-256 of the path, six for the
mid and six for the leaf. Every node begins `\0RDN3` and a kind byte:

```text
L leaf    entries by path: varint:len path  32:row hash  32:blob id
M mid     64 × ( 32:child id  32:child identity )          all zero = empty
T table   32:schema identity (zero = none)  64 × ( 32:mid id  32:mid identity )
R root    32:format  32:config  32:extras id (zero = none)
          tables by name: varint:len name  32:table id  32:table identity
X extras  entries by path: varint:len path  32:blob id
```

The extras node holds the bytes of the files that are not rows: the format,
the configuration and each schema file, and -- in a snapshot -- each file
that was not a valid row.

Each node has two digests. Its **object id** is the SHA-256 of its bytes and
covers everything, blob ids included. Its **identity** covers only what
decides the logical state:

```text
leaf   SHA-256("reldir-leaf-v3\0"  ‖ each entry: path ‖ 0x00 ‖ row hash)
mid    SHA-256("reldir-mid-v3\0"   ‖ 64 child identities)
table  SHA-256("reldir-table-v3\0" ‖ schema identity ‖ 64 mid identities)
root   SHA-256("reldir-state-v3\0" ‖ format ‖ config ‖ each table: name ‖ 0x00 ‖ identity)
```

An empty child contributes 32 zero bytes. Blob ids and the extras node are
outside every identity, so reformatting a file is never a change of state.
The root's identity is the **state identity** `reldir status` reports: two
databases with the same rows, schemas, configuration and format have the same
one, however their files are written.

A change rewrites only the nodes on the paths it touches; everything else is
shared with the tree before it. Two states are compared by descending only
into children whose ids differ, so a diff costs what changed. Decoding accepts
exactly the bytes encoding produces -- sorted, unique, nothing empty stored,
nothing trailing -- so one structure has one id.

### Revisions

A revision is a JSON document in its RFC 8785 canonical form -- members
sorted, no whitespace, minimal escapes, integers only -- and its id is the
SHA-256 of those bytes:

```text
{"binary_version":"…","idempotency_key":null|"…",
 "imported":null|{"format":2,"revision":n},
 "lineage":null|{"quarantined":"…","reason":"…"},
 "origin":"…","parents":["<id>",…],"reldir_revision":3,
 "state":"<state identity>","stats":{"added":n,"modified":n,"removed":n},
 "timestamp":"YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ","transaction_id":null|"…",
 "tree":"<root object id>"}
```

| Member | |
|---|---|
| `origin` | `inference`, `user`, `internal`, `external`, `recovery`, `repair`, `migration`, `import`, `snapshot_restore`, `merge` or `upgrade` |
| `parents` | the revisions it follows, sorted; none for the first, several for a merge |
| `state`, `tree` | the state's identity, and its tree |
| `idempotency_key` | the key the change was made under, if any |
| `transaction_id` | the transaction that wrote it |
| `imported` | for a revision brought over from format 2, its number there |
| `lineage` | for the first revision of a new lineage, the heads set aside and why |

A document that means the same but is spelled differently would have another
id, so it is refused, not normalized. Revision numbers (`#12`) are this
clone's numbering of its revisions, oldest first; the id is the name every
clone shares.

The tips of history are empty files named by revision id in
`.db/history/heads/`. One head is the normal case. Two clones' histories
merged by version control simply have two heads (`HISTORY_DIVERGED`, a
warning), and the next writer records a `merge` revision with every head as a
parent.

### Packs

Objects are stored in immutable packs, `pack-<ulid>.pack` beside
`pack-<ulid>.idx` in `.db/objects/pack/`. Integers are little-endian:

```text
pack:  "RDPK" u32:version=1 u32:count
       count × ( u8:kind  varint:len  len bytes )
       32 bytes: SHA-256 of everything before

idx:   "RDIX" u32:version=1
       256 × u32   fan-out: entry b counts the ids whose first byte <= b
       n × 32      the ids, strictly ascending (n = fan-out[255])
       n × u64     each object's offset in the pack
       n × u32     each object's length
       32 bytes    the pack's trailer, binding this index to it
       32 bytes    SHA-256 of everything before
```

A pack is published by renaming the pack into place and then its index, so a
reader that sees an index always finds its pack; a pack without an index is an
interrupted publish, and is ignored. Each commit publishes one pack. Once
there are more than 16, the smallest are merged so that the rest form a
geometric progression -- each at least twice the size of the next smaller --
and a large pack is rewritten rarely. `reldir gc` rewrites the packs keeping
only what a head, a snapshot or a quarantined head reaches.

Every command verifies the heads as it opens the database: each head's
revision parses, its root is present, and the root's identity is the state
the revision records.
`reldir check --deep` proves everything: every pack against its index and
every object against its id, every revision reachable from the heads, and
every tree recomputed from its leaves up.

### Snapshots and quarantine

A snapshot ref is a canonical JSON document naming a tree:

```text
{"created":"…","kind":"snapshot","reldir_ref":3,"revision":null|"<id>",
 "state":"<state identity>","tree":"<root object id>","valid":true|false}
```

Snapshot names are lowercase letters, digits, `-`, `_` and `.`, beginning with
a letter or digit, not ending with `.`, at most 128 bytes, and never a Windows
device name -- so two names never collide on a case-insensitive filesystem.

`reldir recover --history new-lineage --allow-destructive` sets a history that
does not verify aside: its heads move to `.db/history/quarantine/<stamp>/heads/`
with a `reason.json`, every pack that fails verification -- index first, then
pack -- to `.db/history/quarantine/<stamp>/packs/`, and history begins again
from the files, with fresh copies of every object it names. `gc` collects
nothing a quarantined head reaches through the packs still in
`.db/objects/pack/`; what was set aside stays where it is.

## Transactions

A transaction is staged in `.db/transactions/<ulid>/`: one file per new file
it writes, numbered from `0`, and `journal` -- pretty-printed JSON naming each
change's path, what the file held when the change was planned and what it
will hold (each as SHA-256 and size), the history files the change publishes,
the directories it leaves governing nothing (a dropped table's, removed once
the change empties it) and the idempotency key, followed by a line `#reldir-journal-end <SHA-256 of
everything before>`. A journal without a valid last line was never finished,
and its transaction never happened. See [transactions]({{ site.baseurl }}/transactions).

## Version control

`.db/.gitignore` keeps derived and ephemeral state out of version control, and
always keeps `.db/format`, `.db/config` and `.db/schema/`: a clone must
interpret the folder, and hold it to the contract it was adopted with, exactly
as the original does. `reldir init --track-provenance` versions history too --
`.db/objects/pack/` and `.db/history/` -- whose files are named by content,
so histories merge in git without conflict. See [git]({{ site.baseurl }}/git).

## The mirror

`.db/mirror.sqlite` is a SQLite database derived from the files: a stat cache
of every file with its parsed row and its row-local verdict, one typed table
per governed table, every candidate key and every reference edge. It carries
no constraints, so it can describe an invalid state, and it is never consulted
to decide what a row is. A mirror that is missing, from another layout or
unreadable is discarded and rebuilt from the files, and the command says so. See
[performance]({{ site.baseurl }}/performance).

## Format 2

`reldir upgrade` converts a format 2 database to format 3 in place. It changes
no row file.

| Part | Becomes |
|---|---|
| history | format 3 revisions with the same time, origin, transaction and binary, each marked `imported` with its format 2 number; only history that verifies is converted, and `--new-lineage` begins format 3 history from the files instead, keeping the old aside |
| snapshots | snapshot refs |
| pins | the format 3 dialect, `primaryKey` and `filename` becoming `identity`, with every rule they state |
| working schemas | inferred again as the observed contract when only inference wrote them; converted as written when a migration, a repair or a hand edit changed them |
| configuration | its format 3 settings; the others are dropped |

Format 2's metadata is kept in `.db/format-2/`, and each pin's original in
`.db/format-2/pins/`. The new metadata is built in `.db/upgrade/` and put in
place step by step, `.db/format` last: until then the database is format 2,
and `reldir upgrade` run again resumes.
