# Changelog

Every release of reldir, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions are
numbered as the [stability policy](https://jowharshamshiri.github.io/reldir/stability)
describes: format 3, protocol 1, the error-code registry and the
`reldir-core` API change incompatibly only with the major version.

## [Unreleased]

## [0.104.1719] - 2026-10-11

The first release published on GitHub: a new on-disk format, a versioned
machine protocol, libraries for Python, Node and Rust, an engine whose cost
follows the change rather than the folder, and a binary for every major
platform that updates itself. Databases in format 2 are converted, history
included, by `reldir upgrade`.

### Releases

- Every release carries `reldir-v<version>-<target>.tar.gz` for Linux
  (x86-64, ARM64), macOS (Intel, Apple silicon) and Windows (x86-64, also as
  a `.zip`), each with its `.sha256`.
- `reldir self update [VERSION]` installs the latest release's build for the
  running platform, or the release named, after checking the download
  against its checksum and that the binary in it runs as that version;
  `--dry-run` names it and writes nothing. New codes `RELEASE_UNREACHABLE`,
  `RELEASE_NOT_FOUND` and `RELEASE_UNVERIFIED`.
- Every command runs on a 64 MiB stack on every platform, so a deep
  document or schema is judged the same on Windows as on Linux.
- On macOS, `reldir serve` sees the folder's changes: events reported under
  the folder's resolved path (`/private/var` for `/var`) were dropped.

### The observed contract

- A folder's files define its tables' contract. Adoption infers it from
  what the files hold -- types, presence, nullability, member sets,
  integer and float spellings -- and writes nothing to the files.
- No change reldir makes on its own authority writes a value the contract
  never admitted (`CONTRACT_WIDENING_REQUIRED`); widening is explicit:
  `--widen`, `reldir schema widen`, or doctor's `FIX_WIDEN_CONTRACT`.
- An outside program's out-of-contract value in an inferred table is a
  `CONTRACT_EXCEEDED` warning, recorded, never imitated.
- Statistical facts (enums, unique columns, bounds) are lint proposals, not
  inferred rules.
- Identity: a table is keyed by body columns under a naming rule (`reldir`,
  `verbatim`, `template`) or by its files' names; a folder is never
  ambiguous or keyless, and adopting it renames nothing.

### Writes

- A change writes only what it changes: number spellings, big integers,
  Unicode forms, timestamp offsets, member order, indentation, line endings
  and byte order marks of everything else are kept byte for byte.
- SQL write-back respects presence: SQL `NULL` maps through the contract,
  `absent()` removes a member, unnamed columns stay absent, `RETURNING` sees
  generated values.
- Multi-statement transactions: scripts (`reldir sql "a; b"`, `-f FILE`,
  `BEGIN`/`COMMIT`/`ROLLBACK`), the shell's `.begin`/`.commit`/`.rollback`,
  and the library and driver APIs. Each statement reads what earlier ones
  did; the state they make is judged once, at commit.
- Idempotency keys make retrying a change safe; `--expect-state` holds a
  commit to the state a dry run planned against (`PLAN_CHANGED`).
- Import streams JSON arrays, JSON lines and CSV, from files or stdin, as one
  change, with `--mode insert|upsert|replace`. Export writes JSON, JSON
  lines, CSV or Parquet, filtered and ordered, and never replaces a file.

### Engine and format 3

- History is a content-addressed DAG of revisions over trees in packs;
  snapshots are refs; heads merge in git; gc keeps quarantined lineages.
- Commits go through a journal with an explicit commit point, fsync policy
  after PostgreSQL's fsyncgate, idempotent recovery, and outcomes that say
  whether a change committed (`COMMITTED_RECOVERY_PENDING`,
  `COMMIT_UNKNOWN`).
- One scan per command with a racy-git stat cache; incremental validation
  judges a change by the paths and keys it touches; readers never wait for a
  writer (`SNAPSHOT_READ`).
- `reldir serve`: a folder monitor; every process opening the database reads
  only the paths that changed since it last looked.
- Batched durability: a large change costs one filesystem flush on Linux.
- The SQL mirror compares decimals exactly, timestamps as instants, and big
  integers exactly; `nfc()` and the `RELDIR_NFC` collation compare Unicode
  normalized.

### Interfaces

- Machine protocol 1: rows nested as `{"type":"row","row":{…}}`, typed
  records, events and diagnostics, a final `result` document carrying the
  protocol and version; `reldir protocol schema` prints its JSON Schema.
- One error-code registry: every code has one exit status, a documented
  meaning and a SARIF rule whose help link resolves.
- The MCP server plans every change before committing it by `plan_id`, asks
  the user (by elicitation, or `confirm`) before a plan that removes data,
  validates arguments against each tool's schema, pages query results with
  cursors, and serves schemas, rows, the dialect, the protocol and the docs
  as resources, with subscriptions and prompts.
- Git: `reldir git setup` installs a merge driver that merges row files
  member by member and a diff driver that shows them as JSON pointers;
  `reldir diff --git REV` compares the files with any revision; a GitHub
  Action checks a database and reports through code scanning.
- Libraries: `reldir_core::Connection` for Rust, a PEP 249 driver for Python
  (with `reldir.aio` and the `reldir` command in the wheel), and
  `@reldir/reldir` for Node with exact `BigInt` values.
- The shell keeps history, completes names and runs transactions; every
  command's help has examples; `reldir docs` prints the documentation.

### Verification

- A contract property suite holds random folders, styles and spellings to
  the observed contract through random changes, with an independent
  validator; incremental validation is checked against whole validation;
  ten fuzz targets; a scale harness (`cargo xtask scale`) with performance
  targets; a Bowtie connector for the JSON Schema test suite.

### Removed

- Format 2 is read only by `reldir upgrade`.
- The subprocess-based Python driver; the in-process driver replaces it.
- `indentation_width` and the strict/balanced/loose inference strictness.
