---
title: Configuration
---

# Configuration

`.db/config` holds the settings a person chooses for a database. It is a JSON
object, versioned with the rows, so every clone reads, limits and infers in the
same way. A change to it is recorded in history like any other change.

A key the file may not hold is an error (`CONFIG_INVALID`), never something
quietly ignored, and every value is checked before anything is read. A missing
key takes its default, and a missing file means every default.

## Settings

`reldir init` and the adoption of a folder write every setting with its
default:

```console
$ cat .db/config
{
  "enum_max_values": 10,
  "unique_min_rows": 20,
  "max_json_file_size": 67108864,
  "max_nesting_depth": 128,
  "max_result_rows": 1000000,
  "max_query_memory": 268435456,
  "max_transaction_size": 1073741824,
  "timeout_seconds": null,
  "wait_seconds": 5.0,
  "transaction_timeout_seconds": 300.0,
  "ignore": [
    ".DS_Store",
    "*~",
    "*.swp",
    ".gitkeep"
  ],
  "allow_remote_filesystem": false,
  "portable_names": false,
  "reference_naming": [
    "{singular}_id",
    "{table}_id",
    "{singular}_ids",
    "{singular}_ref",
    "{singular}_refs",
    "{table}_ref",
    "{table}_refs",
    "{singular}",
    "{table}"
  ],
  "reference_min_values": 1
}
```

### Limits

| Key | Default | Meaning |
|---|---|---|
| `max_json_file_size` | `67108864` (64 MiB) | The largest file read, in bytes. A larger row file is `FILE_TOO_LARGE` and is not read. |
| `max_nesting_depth` | `128` | The deepest JSON nesting read. A deeper document is `NESTING_TOO_DEEP`. |
| `max_result_rows` | `1000000` | The most rows a query may return. |
| `max_query_memory` | `268435456` (256 MiB) | The bytes a query may allocate, sorting and temporary storage included. |
| `max_transaction_size` | `1073741824` (1 GiB) | The most bytes one change may write. |

Every limit must be greater than zero. Going past a limit is `RESOURCE_LIMIT`,
never a shortened answer.

### Time

| Key | Default | Meaning |
|---|---|---|
| `timeout_seconds` | `null` | The seconds a query may run. `null` means no limit; otherwise it is a finite number greater than zero. |
| `wait_seconds` | `5.0` | The seconds a writer waits for the writer lock before it reports `LOCK_CONTENDED`. `0` tries once. It must be finite and not negative. |
| `transaction_timeout_seconds` | `300.0` | The seconds a transaction may hold the writer lock, from its first statement to its commit. After that it is rolled back, so a session left open cannot hold every other writer off. It must be finite and greater than zero. |

See [transactions]({{ site.baseurl }}/transactions) for the lock and for what a
transaction is.

### Files

| Key | Default | Meaning |
|---|---|---|
| `ignore` | `.DS_Store`, `*~`, `*.swp`, `.gitkeep` | Glob patterns for files and directories reldir does not govern. A pattern matches a path relative to the root, or a bare name. |
| `allow_remote_filesystem` | `false` | Allow writes on a filesystem whose locking and renames reldir cannot verify: the kinds listed below. |
| `portable_names` | `false` | Report two row files whose names are equal under case or Unicode folding as an error, not a warning (`PORTABILITY_COLLISION`), for folders that move between Linux, macOS and Windows. |

An `ignore` pattern that does not parse is `CONFIG_INVALID`. `reldir init
--ignore PATTERN` begins a database with patterns added to the defaults.

Without `allow_remote_filesystem`, a write is refused with `UNSAFE_FILESYSTEM`
on a filesystem of these kinds:

| Kind | Examples |
|---|---|
| network | NFS, SMB/CIFS, 9p, AFS, network drives on Windows |
| cluster | Ceph, Lustre, GPFS, BeeGFS |
| FUSE | sshfs, virtiofs, sync clients |
| FAT | FAT32, exFAT |
| shared folders of virtual machines | VirtualBox's |
| unrecognised | any filesystem not named here |

ext4, XFS, Btrfs, ZFS, F2FS, bcachefs, tmpfs, overlayfs, APFS, HFS+, UFS, NTFS
and ReFS are local, and writable. Reading is allowed on every filesystem; on
one of the kinds above, a read takes no lock and writes no derived state.

### Inference and lint

| Key | Default | Meaning |
|---|---|---|
| `enum_max_values` | `10` | The most distinct values a string column may hold and still be proposed as an `enum`. A column is proposed only when it has at least three values for each distinct one. |
| `unique_min_rows` | `20` | The rows a table needs before a `unique` constraint or a check is proposed. Fewer rows are too little evidence. |
| `reference_naming` | see [below](#reference-naming) | The column names that mark a reference. |
| `reference_min_values` | `1` | The distinct values a place must hold, every one of them naming a row, before a reference is proposed. It must be greater than zero. |

These settings decide what `reldir lint` proposes and what
`reldir infer --tighten` writes. The contract that adoption infers states only
what every row structurally is, so these settings do not change it. See
[validation]({{ site.baseurl }}/validation#lint).

### Compatibility

| Key | Default | Meaning |
|---|---|---|
| `requires` | absent | The oldest reldir release that understands the file, as `MAJOR.MINOR.PATCH`. A release that adds a setting writes it. |

An older reldir then refuses the file and names the release it needs, instead
of calling the new setting an unknown key.

## Reference naming

When reldir infers a schema, it declares a reference only where a place is
*named* as one to its target. `reldir lint` proposes the other places whose
values name rows. The names are patterns: `{table}` stands for a table's name,
and `{singular}` for that name without a trailing `s`.

With the defaults, `user_id`, `user_ids`, `lesson_ref` and `objective_refs` name
references to `users`, `lessons` and `objectives`. A column named `users` names
references to `users` too.

Each pattern must contain `{table}` or `{singular}`, and no other placeholder.
A pattern without one would match every table:

```console
$ echo '{"reference_naming": ["{tabel}_ref"]}' > .db/config
$ reldir check
error[CONFIG_INVALID]: invalid .db/config: reference_naming pattern "{tabel}_ref" must name the table with {table} or {singular}, and no other placeholder
  --> .db/config
```

## Unknown keys

A key the file may not hold is refused, and the message lists every key the
file may hold:

```console
$ echo '{"indentation_width": 2}' > .db/config
$ reldir status
error[CONFIG_INVALID]: invalid .db/config: unknown field `indentation_width`, expected one of `requires`, `enum_max_values`, `unique_min_rows`, `max_json_file_size`, `max_nesting_depth`, `max_result_rows`, `max_query_memory`, `max_transaction_size`, `timeout_seconds`, `wait_seconds`, `transaction_timeout_seconds`, `ignore`, `allow_remote_filesystem`, `portable_names`, `reference_naming`, `reference_min_values`
  --> .db/config
```

How reldir formats the files it writes is not a setting. Each table's style is
recorded in its schema, as `x-reldir.style`. See
[schemas]({{ site.baseurl }}/schemas#style).

## Overrides for one command

Flags override the limits and times for one command. The file is left as it
is.

| Flag | Overrides |
|---|---|
| `--max-json-file-size BYTES` | `max_json_file_size` |
| `--max-nesting-depth DEPTH` | `max_nesting_depth` |
| `--max-result-rows ROWS` | `max_result_rows` |
| `--max-query-memory BYTES` | `max_query_memory` |
| `--max-transaction-size BYTES` | `max_transaction_size` |
| `--timeout SECONDS` | `timeout_seconds`; fractions are allowed, as in `0.5` |
| `--wait SECONDS` | `wait_seconds` |

```console
$ echo '{"max_result_rows": 1}' > .db/config
$ reldir sql 'SELECT id FROM users'
recorded revision #2 ([..]) (external): 1 change(s)
error[RESOURCE_LIMIT]: the query produces more than 1 rows, the configured limit; nothing past it is returned, so no partial answer is mistaken for a whole one
[..]
$ reldir --max-result-rows 5 sql 'SELECT id FROM users'
id
-----
ada
grace
(2 rows)
2 row(s)
```

The first command records the edited configuration as a revision, because the
file changed outside reldir.

An override is checked as the file is. A value the file could not hold is
refused with `RESOURCE_LIMIT`:

```console
$ reldir --max-result-rows 0 sql 'SELECT id FROM users'
error[RESOURCE_LIMIT]: invalid command-line limit: max_result_rows must be greater than zero
```

## Environment

| Variable | Meaning |
|---|---|
| `RELDIR_DB` | The database root, as `--db` gives it. `--db` takes precedence. |
| `RELDIR_FORMAT` | The output format when neither `--format` nor `--json` gives one: `table`, `json`, `jsonl`, `csv` or `sarif`. |
| `NO_COLOR` | Any non-empty value turns colour off, as `--no-color` does. |

Without `--db` or `RELDIR_DB`, reldir uses the nearest ancestor of the working
directory that holds `.db/`. Without a format, it writes `table` to a terminal
and `jsonl` otherwise.

## Version control

`.db/.gitignore` keeps derived and per-clone state out of version control. It
versions three things:

- `format`, the on-disk format version;
- `config`, this file;
- `schema/`, the inferred contracts of tables nobody has pinned.

The mirror, the cache, history, snapshots and transaction staging are ignored.
They are derived, belong to one clone, or exist only briefly. After a
`git clone`, the first command reads every row, builds the mirror and records
the first revision.

`reldir init --track-provenance` versions history as well: `objects/pack/`,
`history/heads/`, `history/refs/` and `history/quarantine/`. Packs and heads
are named by their content, so the histories of two branches merge in git
without conflict, and a clone carries a history it can verify.
