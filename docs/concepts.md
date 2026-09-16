---
title: Concepts
---

# Concepts

## The filesystem is authoritative

There is no hidden copy of your data. A row *is* a JSON file. The schema the
runtime uses is a JSON file in `.db/schema/`. Like indexes, statistics and the
manifest, it is derived state that can be deleted and rebuilt from your files at
any time. Pinning a schema copies it to `schema/`, where it becomes a declaration
you own and `rm -rf .db` cannot lose.

```text
database/
├── schema/          authoritative: pinned declarations you keep in git
│   └── users.json
├── users/           authoritative: one file per row
│   ├── u1.json
│   └── u2.json
└── .db/             metadata
    ├── format       authoritative: how to interpret the directory
    ├── config       authoritative: operational configuration
    ├── provenance/  authoritative: history of accepted states
    ├── schema/          derived: the working schema each command reads
    ├── manifest.json    derived
    ├── indexes/         derived
    ├── statistics/      derived
    ├── objects/         content-addressed revision objects
    ├── snapshots/
    ├── transactions/    ephemeral: staging and journals
    └── lock             ephemeral
```

## Rows and identity

One JSON object per file. The root must be an object.

A row's relational identity comes from its **primary key as stored in the file
body**, not from its filename. The filename is derived from that identity by a
deterministic rule, so a row always has exactly one correct path:

```text
<table>/<filename-key>.json
```

The filename key comes from the `x-reldir.filename` columns (the primary key by
default). Each value is rendered canonically and percent-encoded for every byte
outside `[A-Za-z0-9._-]`; multiple columns are joined with `,`. A leading `.` is
encoded, so a row can never become a hidden file, and `/` can never appear, so a
row can never escape its table directory.

If a file's name disagrees with its body, that is `IDENTITY_MISMATCH`. `doctor`
offers to rename the file. It will not rewrite the body to match the name unless
you ask.

## Validity states

Every command begins by establishing which of these is true:

| State | Meaning |
|---|---|
| `VALID_UNCHANGED` | The filesystem matches the last recorded state |
| `VALID_CHANGED_EXTERNALLY` | It differs, and the result is still valid; adopted as a new revision |
| `INVALID` | One or more invariants are violated |
| `UNINITIALIZED` | No `.db/` metadata; only `init`, `infer`, `inspect`, and `help` operate |

When the database is `INVALID`, commands whose correctness depends on a valid
state refuse to run. The diagnostic commands keep working, because they are what
you need when something is wrong: `status`, `check`, `lint`, `doctor`, `inspect`,
`recover`, and `snapshot restore`.

## External modification

External edits are a first-class input, not an error:

```text
observe filesystem
        ↓
determine changes since the known state
        ↓
validate the resulting database
        ↓
   valid?  ── yes ──> adopt as a new revision (origin: external)
        └── no ───> INVALID + diagnostics
```

`reldir` judges only the state it observes when invoked. A directory that is
temporarily inconsistent midway through a multi-file edit is not a problem as
long as no command runs at that moment. If one does, it reports the
inconsistency rather than assuming more edits are coming.

If you externally delete a parent row whose foreign key says `onDelete: cascade`,
`reldir` does **not** perform the cascade afterwards. The observed state is valid
only if the dependent changes are already present. This avoids reading an
incomplete edit as transactional intent. `doctor` can offer the cascade as a data
fix.

## Provenance is not integrity

Three separate questions, deliberately kept apart:

| Question | Answered by |
|---|---|
| Is the current relational state valid? | **Integrity**: `check`, `status` |
| What state transitions have been observed? | **Provenance**: `log`, `show` |
| How does the binary safely perform multi-file writes? | **Transactions** |

A state can be valid even though `reldir` did not produce it. Provenance records
*how* a state appeared (`internal`, `external`, `recovery`, `repair`,
`migration`, `import`, or `snapshot_restore`). It never claims to know who made an
external change, because that information is not available.

## Canonical state and hashing

Each accepted revision has a `state_root_hash` over the canonical *logical* state,
so identity does not depend on incidental formatting. Reformatting a file without
changing its values does not create a new revision.

Canonicalisation fixes: row keys in schema column order, nested object keys
lexicographically, NFC strings, shortest round-trip numbers, `-0` normalised to
`0`, UTC timestamps, two-space indentation, and a trailing newline.

`reldir` will not rewrite your files merely to canonicalise them. Only
`doctor --fix --only FIX_CANONICALIZE --allow-data` does that, and only on request.

A schema's identity follows what it *says*, not how it is written. The hash is
taken over a versioned encoding of the relational model rather than over the JSON
Schema document on disk, so reformatting a schema, or changing the file format
itself, leaves every revision's identity intact. Column order is the exception:
rows are written in it, so two schemas that order their columns differently
describe different bytes.

## Determinism

Given identical rows, identical schemas, and the same format version, `reldir`
derives the same logical state, the same root hash, the same inferred schemas,
and the same lint findings, on any machine and in any order.

Nothing machine-specific reaches logical identity. Timestamps, paths outside the
root, binary versions, and the contents of `.db/` beyond `format` and `config`
are recorded as history, never mixed into the hash, so a root hash is comparable
between two clones.

## One mutation path

Every change the binary makes converges on the same transaction and validation
machinery: SQL, CRUD commands, import, migrations, cascades, doctor fixes, schema
pin, and snapshot restore. No feature writes authoritative JSON any other way.

## Unknown files

| What | Treatment |
|---|---|
| `.json` inside a table directory | a row |
| Anything else inside a table directory | `UNEXPECTED_FILE` |
| Editor artefacts (`.DS_Store`, `*~`, `*.swp`, `.gitkeep`) | ignored by default |
| Paths matching `.db/config` `ignore` globs | ignored |
| A top-level directory with no schema | `UNGOVERNED_DIRECTORY` (warning; error under `check --strict`) |

Nothing is silently absorbed, so a typo cannot become invisible state.
