---
title: Configuration
---

# Configuration

Operational configuration lives in `.db/config`, which is authoritative state:
every clone reads the same settings, so formatting and limits are reproducible.

## Defaults

`reldir init` writes this file:

```json
{
  "indentation_width": 2,
  "enum_max_values": 10,
  "unique_min_rows": 20,
  "max_json_file_size": 67108864,
  "max_nesting_depth": 128,
  "max_result_rows": 1000000,
  "max_query_memory": 268435456,
  "max_sort_memory": 268435456,
  "max_temporary_disk": 4294967296,
  "max_transaction_size": 1073741824,
  "timeout_seconds": null,
  "wait_seconds": 5.0,
  "ignore": [
    ".DS_Store",
    "*~",
    "*.swp",
    ".gitkeep"
  ]
}
```

An unknown key is rejected rather than ignored, and every limit must be greater
than zero. `wait_seconds` is the one setting with a meaningful zero -- it means
"try once, then report contention" -- so it is required only to be finite and
not negative. A malformed config is `INTERNAL_METADATA_CORRUPT`, not a silent
fallback to defaults.

## Settings

| Key | Meaning |
|---|---|
| `indentation_width` | spaces used when the binary writes a JSON file |
| `enum_max_values` | most distinct values a column may have and still be inferred as an `enum` |
| `unique_min_rows` | rows required before inference will commit to a `unique` constraint or check |
| `max_json_file_size` | largest governed file that will be read |
| `max_nesting_depth` | deepest JSON nesting accepted, enforced while parsing |
| `max_result_rows` | most rows a query may return |
| `max_query_memory` | memory bound for a query's materialised result |
| `max_sort_memory` | memory bound for queries that sort |
| `max_temporary_disk` | reserved: the query engine keeps temporary storage in memory, so nothing currently consumes temporary disk and this bounds nothing. It is validated and must be greater than zero. |
| `max_transaction_size` | total bytes one transaction may stage |
| `timeout_seconds` | query timeout; `null` for none |
| `wait_seconds` | how long a writer waits for the writer lock before reporting `LOCK_CONTENDED`; `0` tries once |
| `ignore` | glob patterns excluded from governance |

Formatting is deliberately almost unconfigurable: only the indentation width can
change, so every clone of a database produces identical bytes.

## Per-invocation overrides

Every limit can be overridden for a single command, which is useful for one-off
imports and for exploring a database you do not control:

```sh
reldir --max-result-rows 50 sql 'SELECT * FROM events'
reldir --max-nesting-depth 512 check
reldir --max-json-file-size 100000000 import blobs --from big.jsonl
reldir --timeout 30 sql 'SELECT ...'
reldir --wait 0 update users u1 '{"name":"Alice"}'   # fail at once if busy
```

| Flag |
|---|
| `--max-json-file-size <BYTES>` |
| `--max-nesting-depth <DEPTH>` |
| `--max-query-memory <BYTES>` |
| `--max-sort-memory <BYTES>` |
| `--max-temporary-disk <BYTES>` (reserved; see above) |
| `--max-result-rows <ROWS>` |
| `--max-transaction-size <BYTES>` |
| `--timeout <SECONDS>` |
| `--wait <SECONDS>` |

Exceeding a limit is always an explicit `RESOURCE_LIMIT` failure, never a
silently truncated result.

## Ignoring files

Add glob patterns to `ignore` to keep non-database files inside governed
directories:

```json
{ "ignore": [".DS_Store", "*~", "*.swp", ".gitkeep", "*.md", "drafts/**"] }
```

A file inside a table directory that is not `.json` is `UNEXPECTED_FILE`, and a
top-level directory with no schema is `UNGOVERNED_DIRECTORY`: a warning in
`status`, an error under `check --strict`. Both are reported so that a typo does
not become invisible state.

## Version control

`reldir init` writes `.db/.gitignore`:

```text
*
!format
!config
```

`.db/format` and `.db/config` are versioned because they are needed to interpret
the database. The manifest, indexes, statistics, snapshots, and transaction
staging are not, because they are derived or ephemeral.

Provenance is per-clone by default. Use `reldir init --track-provenance` to version
history as well, which also retains the content-addressed objects it references so
a clone keeps a complete, verifiable history.

After a `git clone`, the first command observes every row as an external
transition and rebuilds derived state. This works with nothing in `.db/` beyond
`format` and `config`.

For CI:

```sh
reldir --readonly check --format json     # exits 2 on INVALID
reldir --readonly check --strict          # also exits 7 on lint findings
```

Merge conflicts are a Git concern. `reldir` reports the result as `INVALID` with
`INVALID_JSON` on conflict markers, and `doctor` classifies them as `manual`.
