---
title: Concepts
---

# Concepts

## The filesystem is authoritative

There is no hidden copy of your data. A row *is* a JSON file; a table is a
directory; a table's schema is a JSON Schema document. Everything reldir keeps
beside them is either history or derived state it can rebuild.

```text
database/
├── schema/              pins: schemas you declare and keep in version control
│   └── users.json
├── users/               one file per row
│   ├── u1.json
│   └── u2.json
└── .db/
    ├── format           how to interpret the directory          (versioned)
    ├── config           operational settings                    (versioned)
    ├── schema/          schemas reldir inferred for unpinned tables
    ├── provenance/      the history of accepted states
    ├── objects/         every recorded row and schema, by hash
    ├── snapshots/       named copies
    ├── mirror.sqlite    derived: the index SQL runs on
    ├── transactions/    ephemeral: staging and journals
    └── lock             ephemeral: the writer lock
```

`.db/format` and `.db/config` are needed to interpret the directory, so
`.db/.gitignore` keeps them under version control and ignores the rest.
Deleting `.db/mirror.sqlite`, `.db/schema/` or all of `.db/` loses nothing a
pin or a row says: the next command rebuilds it from the files.

## Rows and identity

A row is one JSON object in one file. Its identity is its primary key, read
from the file's body; its file name is derived from that key by a fixed rule
(see [Schemas]({{ site.baseurl }}/schemas#identity-and-file-names)), so each row has exactly one
correct path. A file whose name is not the one its key gives is
`IDENTITY_MISMATCH`, and `reldir doctor` renames it -- it never rewrites a
body to match a name.

reldir writes a row with exactly the members it has, in the order it has them:
an omitted member stays omitted (it reads as its default where the row is
read), and a file reldir rewrites keeps its members where its author put them.
Member order and formatting are never part of identity; the state hash is taken
over a key-sorted canonical form.

## Validity

Every command begins by observing the directory and judging it:

| State | Meaning |
|---|---|
| `VALID` | the files are a valid database, and history has recorded them |
| `VALID_CHANGED_EXTERNALLY` | valid, and different from history: recorded as a new revision now, with origin `external` |
| `VALID_UNRECORDED` | valid and different from history, observed with `--readonly`, which records nothing |
| `INVALID` | one or more faults, each reported with its file, line, column and JSON Pointer |
| `EMPTY` | no metadata, no tables, no pins: nothing to govern |

An invalid database still answers `status`, `check`, `lint`, `doctor`,
`inspect`, `recover`, `log`, `diff` and `snapshot restore`, because those are
how you return to a valid one. Queries refuse -- an answer computed from a state
that breaks its own rules is not an answer -- unless you pass `--allow-invalid`,
which answers anyway and marks the result `"database_valid": false`. Changes
always refuse: a change to an invalid database would be judged against faults
it did not make.

## Changes made outside reldir

Anything may edit the directory. reldir judges the state it observes when it
runs; a directory that is briefly inconsistent midway through a multi-file edit
is not a problem as long as no command runs at that moment.

```text
observe the files ─→ valid? ── yes ─→ recorded as a new revision (origin: external)
                          └──── no ──→ INVALID, every fault located; nothing recorded
```

A referential action describes what *reldir* does when *it* deletes or re-keys
a row. A row you delete by hand is not cascaded after the fact: the state is
judged as it stands, and a reference to the missing row is a
`FOREIGN_KEY_VIOLATION`. `reldir doctor` then offers, in order: restoring the
missing row exactly as history last recorded it, removing the reference, or
deleting the row that holds it.

## Provenance is not validity

Three questions, kept apart:

| Question | Answered by |
|---|---|
| Is the current state valid? | integrity: `check`, `status` |
| How did the state come to be? | provenance: `log`, `show`, `diff` |
| How is a multi-file change written safely? | [transactions]({{ site.baseurl }}/transactions) |

Each accepted state is a revision in `.db/provenance/`, naming its predecessor
and carrying only what changed; every row and schema it adds is kept, by hash,
in `.db/objects/`, so any recorded state can be reconstructed and any deleted
row restored byte for byte. A revision records *how* a state appeared --
`internal`, `external`, `recovery`, `repair`, `migration`, `import` or
`snapshot_restore` -- and never claims to know who made an outside edit.

History that does not verify -- a record missing, an object altered -- is
reported as `INTERNAL_METADATA_CORRUPT` with the revision named. Reads keep
working; nothing new can be recorded until it is repaired, either by restoring
`.db/provenance/` and `.db/objects/` from a backup, or by
`reldir recover --history new-lineage --allow-destructive`, which moves the old
history aside intact and begins a new one.

## Hashing and determinism

A revision's root hash covers the format, the configuration, each table's
schema identity, and each row's canonical hash, built per table so that
recognising "nothing changed" costs one read per table. Given the same files,
reldir derives the same root, the same inferred schemas and the same findings
on any machine. Nothing machine-specific -- timestamps, absolute paths, binary
versions -- reaches the hash, so roots are comparable between clones.

## One way to change anything

Every change the binary makes converges on one path: plan the row changes,
complete them with their referential actions, judge the state they produce,
write it through the recoverable [transaction protocol]({{ site.baseurl }}/transactions), record
it. SQL, the row commands, imports, migrations, repairs and snapshot restore
all take it; nothing writes a row any other way.

## Files reldir does not recognise

| What | Treatment |
|---|---|
| `.json` in a table directory | a row |
| anything else in a table directory | `UNEXPECTED_FILE` |
| editor artefacts (`.DS_Store`, `*~`, `*.swp`, `.gitkeep`) | ignored |
| paths matching `.db/config` `ignore` | ignored |
| a top-level directory no schema governs | `UNGOVERNED_DIRECTORY` (a warning) |
| symlinks, sockets, FIFOs, devices, hard-linked files | `NON_REGULAR_FILE`, never followed |
| two names equal under case or Unicode folding | `PATH_COLLISION` |

Nothing is silently absorbed, so a typo cannot become invisible state.

## What reldir is not

It is not a server, and not for high write concurrency: one writer at a time
holds the lock, and every change touches files. It does not replace a
general-purpose database for large transactional workloads. It is for data
that people and tools should be able to read, review and edit directly, and
that must nonetheless always be right.
