---
title: Errors
---

# Errors and exit codes

Diagnostics are a stable contract. Every code below keeps its meaning across a
major version, so scripts may depend on them.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | success; database `VALID` |
| `1` | usage error: bad flag, unknown command, missing argument |
| `2` | database `INVALID` |
| `3` | `CONCURRENT_MODIFICATION` or lock timeout |
| `4` | query error: unsupported SQL, type error, unknown table or column |
| `5` | `TRANSACTION_INCOMPLETE`: recovery required or failed |
| `6` | `FORMAT_UNSUPPORTED` or `INTERNAL_METADATA_CORRUPT` |
| `7` | lint findings with `--strict` |
| `8` | inference failed (`INFER_*`) |
| `9` | confirmation declined, or `--yes` required |
| `10` | `UNINITIALIZED` |

When several faults are present, the most severe wins: metadata and format
corruption outrank an incomplete transaction, which outranks an ordinary invalid
state.

## Diagnostic shape

Human output is compiler-style:

```text
error[FOREIGN_KEY_VIOLATION]: posts.user_id references a row that does not exist
  --> posts/42.json:4:15
   |
 4 |   "user_id": "missing",
   |               ^
   = constraint: posts.user_id -> users.id
   = expected: existing users.id
   = observed: "missing"
   = fixes: FIX_ORPHAN_SET_NULL, FIX_ORPHAN_DELETE_ROW
   = help: run `db doctor` for fix options
```

Machine output carries the same information, one object per diagnostic:

```json
{
  "kind": "diagnostic",
  "severity": "error",
  "code": "FOREIGN_KEY_VIOLATION",
  "message": "posts.user_id references a row that does not exist",
  "table": "posts",
  "path": "posts/42.json",
  "location": { "line": 4, "column": 15 },
  "field": "user_id",
  "constraint": "posts.user_id -> users.id",
  "expected": "existing users.id",
  "observed": "\"missing\"",
  "fixes": ["FIX_ORPHAN_SET_NULL", "FIX_ORPHAN_DELETE_ROW"]
}
```

Optional members are omitted rather than emitted as `null`.

## Structure

| Code | Meaning |
|---|---|
| `UNINITIALIZED` | no `.db/` metadata was found |
| `ALREADY_INITIALIZED` | `init` ran where a database already exists |
| `FORMAT_UNSUPPORTED` | on-disk format or schema dialect version this binary cannot interpret |
| `FORMAT_MISSING` | `.db/` declares no format version and holds state that cannot be rebuilt; pass `--rebuild-metadata` to discard it |
| `INTERNAL_METADATA_CORRUPT` | unreadable or inconsistent internal metadata |
| `UNEXPECTED_FILE` | non-`.json` file inside a governed table directory |
| `UNGOVERNED_DIRECTORY` | top-level directory with no schema |
| `PATH_VIOLATION` | a path that escapes the root or aliases internal metadata |
| `PATH_COLLISION` | two paths that collide by case or Unicode normalisation |
| `NON_REGULAR_FILE` | symlink, socket, FIFO, device, or hard-linked file in a governed location |
| `USAGE` | a flag, argument, or combination the command cannot accept |
| `CONFIG_INVALID` | `.db/config` is unreadable or exceeds the bootstrap size limit |
| `ROOT_JSON_AMBIGUOUS` | JSON files sit at the database root with no table identity |
| `IO_ERROR` | a file operation failed; the path is named |

## Schema structure

| Code | Meaning |
|---|---|
| `SCHEMA_ALREADY_PINNED` | `db schema pin` would replace a different declaration; pass `--overwrite` |
| `SCHEMA_NOT_PINNED` | `db schema restore` found no pin to rebuild from |
| `SCHEMA_PINNED` | inference would diverge from a pinned declaration |
| `SCHEMA_INVALID_JSON` | a schema file is not valid JSON |
| `SCHEMA_MISSING_REQUIRED` | a required member is absent: `x-jdb`, `x-jdb.table`, `x-jdb.primaryKey`, `x-jdb.columnOrder`, or `properties` |
| `SCHEMA_UNKNOWN_KEY` | unrecognised `x-jdb` key, or a column member meaningless for its type; the message names the nearest valid key |
| `SCHEMA_UNSUPPORTED_KEYWORD` | a JSON Schema keyword outside the jdb dialect, which would change which rows are valid |
| `SCHEMA_TABLE_NAME_MISMATCH` | `x-jdb.table` does not equal the file stem |
| `SCHEMA_INVALID_TABLE_NAME` | table name breaks the naming rule |
| `SCHEMA_COLUMN_TYPE_MISSING` | a column is not a subschema object |
| `SCHEMA_TYPE_UNKNOWN` | unrecognised column type |
| `SCHEMA_COLUMN_UNKNOWN` | a constraint names a column that does not exist |
| `SCHEMA_PK_COLUMN_UNKNOWN` | the primary key names an unknown or repeated column |
| `SCHEMA_PK_NULLABLE` | a primary-key column is declared nullable |
| `SCHEMA_DEFAULT_TYPE_MISMATCH` | a default or generator does not match its column |
| `SCHEMA_FILENAME_NOT_UNIQUE` | `x-jdb.filename` is not a NOT NULL unique key |
| `SCHEMA_CHECK_INVALID` | a check has no name, no expression, or is not boolean; or a `pattern` is not a valid regular expression |

## Schema semantics

| Code | Meaning |
|---|---|
| `SCHEMA_FK_TARGET_MISSING` | the referenced table has no schema |
| `SCHEMA_FK_TARGET_NOT_UNIQUE` | target columns are not a primary key or unique constraint |
| `SCHEMA_FK_TYPE_MISMATCH` | referencing and referenced types differ |
| `SCHEMA_FK_ACTION_INVALID` | malformed key, or an action the columns cannot support |
| `SCHEMA_FK_CYCLE` | a cycle in which every edge cascades |

## Rows

| Code | Meaning |
|---|---|
| `INVALID_JSON` | a row file is not valid JSON |
| `ROW_ROOT_NOT_OBJECT` | the root of a row file is not an object |
| `ROW_UNKNOWN_FIELD` | a field absent from the schema |
| `ROW_MISSING_FIELD` | a required field is absent with no default |
| `TYPE_MISMATCH` | a value does not match its column type, its `pattern`, or a closed object's declared properties |
| `IDENTITY_MISMATCH` | the filename disagrees with the row's identity |
| `PRIMARY_KEY_VIOLATION` | two rows share a primary key |
| `UNIQUE_VIOLATION` | two rows share a unique key |
| `NOT_NULL_VIOLATION` | null in a NOT NULL column |
| `FOREIGN_KEY_VIOLATION` | a reference to a row that does not exist |
| `CHECK_VIOLATION` | a row fails a declared check |

## Inference

All inference failures exit `8`.

| Code | Meaning |
|---|---|
| `INFER_NO_ROWS` | the table directory contains no `.json` files |
| `INFER_ROOT_NOT_OBJECT` | a file's root is an array or scalar |
| `INFER_TYPE_CONFLICT` | one column holds incompatible JSON kinds |
| `INFER_NO_PRIMARY_KEY` | no column can serve as a key; the message says why each was rejected |
| `INFER_AMBIGUOUS_PRIMARY_KEY` | several candidates remain; resolve with `--pk` |
| `INFER_UNTYPED_COLUMN` | a column is null or absent in every row (strict only) |
| `INFER_NESTED_DIRECTORY` | a subdirectory inside a table directory |
| `INFER_NON_JSON_FILE` | a non-`.json` file inside a table directory |
| `INFER_INVALID_JSON` | invalid JSON, with line and column |
| `INFER_FILENAME_INCONSISTENT` | file stems do not follow the filename rule for the chosen key |

## Runtime

| Code | Meaning |
|---|---|
| `CONCURRENT_MODIFICATION` | another writer holds the lock, or files changed mid-transaction |
| `TRANSACTION_INCOMPLETE` | an interrupted transaction needs recovery |
| `QUERY_UNSUPPORTED` | SQL outside the supported subset |
| `QUERY_TYPE_ERROR` | a type error inside a query |
| `UNKNOWN_TABLE` / `UNKNOWN_COLUMN` / `UNKNOWN_ROW` | a name or key that does not resolve |
| `RESOURCE_LIMIT` | a configured limit was exceeded |
| `CONFIRMATION_REQUIRED` | the operation needs confirmation or `--yes` |
| `MUTATION_CONFLICT` | one change plan names the same path twice |
| `READ_ONLY` | the operation would write while `--readonly` is in effect |
| `SNAPSHOT_EXISTS` | a required safety snapshot already exists |

## Warnings

Warnings are reported alongside results and never set an exit code of their own.

| Code | Meaning |
|---|---|
| `INDEX_STALE` | derived indexes are missing, stale, or corrupt |
| `MANIFEST_STALE` | the derived manifest is corrupt and was not rebuilt |
| `METADATA_STALE_READONLY` | state is valid but differs from recorded metadata; read-only did not record it |

## Lint

Lint findings are warnings and suggestions rather than errors, and are listed in
[Validation](validation#lint). They fail the command only under `--strict`,
with exit code `7`.
