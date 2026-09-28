---
title: Errors
---

# Errors and exit statuses

Codes are a stable contract: each keeps its meaning, so scripts and agents may
depend on them.

## Exit statuses

| Status | Meaning |
|---|---|
| `0` | success; for `check`, a valid database |
| `1` | usage: a flag, argument, path or setting the command cannot accept |
| `2` | the database is invalid, or a change was refused because it would make it so |
| `3` | contention or interference: `LOCK_CONTENDED`, `CONCURRENT_MODIFICATION`, `PATH_INTERFERENCE`; nothing was written |
| `4` | a query or a name that does not resolve: unsupported SQL, an unknown table, column, row, revision or snapshot |
| `5` | an interrupted transaction needs recovery |
| `6` | the format, metadata or history cannot be trusted, or an I/O failure |
| `7` | findings under `--strict` |
| `8` | inference failed |
| `9` | a decision is required: confirm with `--yes`, choose a policy, or authorise with `--allow-destructive` |
| `10` | there is no database here, and `--no-auto` forbade making one |
| `11` | the filesystem cannot keep the commit protocol's promises (`UNSAFE_FILESYSTEM`) |

When several faults are present, the most severe decides: metadata and format
outrank an interrupted transaction, which outranks an invalid state. A warning
never sets a status.

## Finding and opening a database

| Code | Meaning |
|---|---|
| `PATH_NOT_FOUND` | `--db` or `RELDIR_DB` names something that does not exist |
| `PATH_NOT_DIRECTORY` | `--db` or `RELDIR_DB` names a file |
| `ROOT_AMBIGUOUS` | a folder of pins without `.db/` sits inside another database; name the one you mean |
| `UNINITIALIZED` | there is no `.db/`, and nothing may create one |
| `ALREADY_INITIALIZED` | `init` where `.db/` exists |
| `ROOT_JSON_AMBIGUOUS` | JSON files at the root belong to no table |
| `FORMAT_UNSUPPORTED` | `.db/` or a revision is in a format this binary does not read |
| `FORMAT_MISSING` | `.db/` has no format marker and holds history; `--rebuild-metadata` discards it |
| `CONFIG_INVALID` | `.db/config` is unreadable, too large, or holds an unknown or unusable setting |
| `UNSAFE_FILESYSTEM` | a write on a network or FUSE filesystem without `allow_remote_filesystem` |
| `INTERNAL_METADATA_CORRUPT` | history, an object, the mirror or other metadata fails verification |
| `IO_ERROR` | a file operation failed; the path is named |
| `USAGE` | an argument the command cannot accept |

## Structure

| Code | Meaning |
|---|---|
| `UNEXPECTED_FILE` | a file in a table or schema directory that is not `.json` |
| `UNGOVERNED_DIRECTORY` | a top-level directory no schema governs (warning) |
| `NON_REGULAR_FILE` | a symlink, socket, FIFO, device or hard-linked file where a row or schema belongs |
| `PATH_COLLISION` | two names equal under case or Unicode folding |
| `PATH_VIOLATION` | a path that escapes the root or reaches into reldir's metadata; an unusable snapshot name |

## Schemas

| Code | Meaning |
|---|---|
| `SCHEMA_INVALID_JSON` | the schema file is not JSON |
| `SCHEMA_MISSING_REQUIRED` | a required member is absent (`$schema`, `type`, `properties`, `additionalProperties`, `x-reldir`, `table`, `primaryKey`) |
| `SCHEMA_UNKNOWN_KEY` | a member that is no keyword, at any depth; the message names the nearest one |
| `SCHEMA_GRAMMAR` | the document breaks the dialect's grammar, or does not compile |
| `SCHEMA_REF_EXTERNAL` | a `$ref` leaves the document |
| `SCHEMA_TABLE_NAME_MISMATCH` | `x-reldir.table` is not the file's name |
| `SCHEMA_INVALID_TABLE_NAME` | a table name breaks the naming rule |
| `SCHEMA_TYPE_UNKNOWN` | an unknown column type |
| `SCHEMA_COLUMN_UNKNOWN` | a constraint names a column that does not exist |
| `SCHEMA_PK_COLUMN_UNKNOWN` | the primary key names an unknown or repeated column |
| `SCHEMA_PK_NULLABLE` | a primary-key column admits null |
| `SCHEMA_PK_NOT_REQUIRED` | a primary-key column is not in `required` |
| `SCHEMA_KEY_NOT_SCALAR` | a key column is an array, object or json |
| `SCHEMA_DEFAULT_TYPE_MISMATCH` | a default or generator does not fit its column, or a column has both |
| `SCHEMA_FILENAME_NOT_UNIQUE` | `x-reldir.filename` is not a key over columns that are never null |
| `SCHEMA_CONSTRAINT_NAME_DUPLICATE` | two constraints share a name |
| `SCHEMA_REFERENCE_PATH_INVALID` | a reference path does not parse, or cannot reach a scalar through the schema |
| `SCHEMA_FK_TARGET_MISSING` | a foreign key's target table or domain has no schema |
| `SCHEMA_FK_TARGET_INVALID` | a multi-table target keys on several columns |
| `SCHEMA_FK_TARGET_NOT_UNIQUE` | the target columns are no key of the target |
| `SCHEMA_FK_TYPE_MISMATCH` | a reference and its target key can never be equal |
| `SCHEMA_FK_ACTION_INVALID` | an action the reference's shape cannot carry out |
| `SCHEMA_DOMAIN_KEY_INVALID` | an identity domain's tables key on different types |
| `SCHEMA_ACYCLIC_INVALID` | an acyclic graph's edges do not reach the table's key |
| `SCHEMA_CHECK_INVALID` | a check does not compile, is not deterministic, or reads another table |
| `SCHEMA_ASSERTION_INVALID` | an assertion is not one deterministic `SELECT` of the table's key |
| `SCHEMA_CONFLICT` | inference differs from an existing schema, under `--on-schema-conflict fail` |
| `SCHEMA_TABLE_EXISTS` | a table or schema that already exists |
| `SCHEMA_COLUMN_EXISTS` | a column that already exists |
| `SCHEMA_CONSTRAINT_EXISTS` | a constraint or index that already exists |
| `UNKNOWN_CONSTRAINT` | no constraint or index by that name |

## Rows

| Code | Meaning |
|---|---|
| `INVALID_JSON` | a row is not JSON, or repeats a member; merge-conflict markers are named |
| `ROW_ROOT_NOT_OBJECT` | a row is not one JSON object |
| `ROW_MISSING_FIELD` | a required member is absent |
| `ROW_UNKNOWN_FIELD` | a member the schema does not declare |
| `NOT_NULL_VIOLATION` | null in a column that admits none |
| `TYPE_MISMATCH` | a value of the wrong type or lexical form |
| `SCHEMA_VIOLATION` | a value breaks any other rule of its schema -- a bound, a pattern, a format, a composition, a conditional -- stated for the keyword at fault |
| `KEY_COLLISION` | two members whose names are equal under Unicode normalization |
| `IDENTITY_MISMATCH` | the file is not named by its row's key |
| `FILENAME_TOO_LONG` | a key whose file name would exceed 255 bytes |
| `PRIMARY_KEY_VIOLATION` | two rows share a primary key, or a change would overwrite a row |
| `UNIQUE_VIOLATION` | two rows share a unique key |
| `DOMAIN_KEY_VIOLATION` | two tables of one identity domain share a key |
| `FOREIGN_KEY_VIOLATION` | a reference names no row; or a delete or key change that a `restrict` reference refuses |
| `CYCLE_VIOLATION` | an acyclic graph has a cycle, which the message spells |
| `CHECK_VIOLATION` | a row fails a check |
| `ASSERTION_VIOLATION` | an assertion names a row (an error or, when declared so, a warning) |

## Inference

Every inference failure exits `8` and writes nothing.

| Code | Meaning |
|---|---|
| `INFER_NO_ROWS` | a table directory holds no rows to infer from |
| `INFER_ROOT_NOT_OBJECT` | a file holds an array or a scalar, not a row |
| `INFER_INVALID_JSON` | a file is not JSON; the line and column are named |
| `INFER_NESTED_DIRECTORY` | a directory inside a table directory |
| `INFER_NON_JSON_FILE` | a file that is not `.json` inside a table directory |
| `INFER_TYPE_CONFLICT` | one place holds values of incompatible kinds; `--strictness loose` accepts any JSON there |
| `INFER_UNTYPED_COLUMN` | a column is null or absent in every row, under `--strictness strict` |
| `INFER_NO_PRIMARY_KEY` | no column is present, non-null and distinct in every row; each is named with the reason |
| `INFER_AMBIGUOUS_PRIMARY_KEY` | several columns could be the key; choose one with `--pk` |
| `INFER_FILENAME_INCONSISTENT` | the files are not named by the key inference chose |
| `SCHEMA_INVALID` | a schema that inference, a migration or a repair produced is not valid; the fault is named |

## Queries and changes

| Code | Meaning |
|---|---|
| `QUERY_UNSUPPORTED` | SQL outside the admitted surface, or that does not parse |
| `QUERY_TYPE_ERROR` | a value SQLite produced that its column cannot hold |
| `UNKNOWN_TABLE`, `UNKNOWN_COLUMN`, `UNKNOWN_ROW` | a name or key that does not resolve |
| `UNKNOWN_REVISION`, `UNKNOWN_SNAPSHOT`, `UNKNOWN_RESOURCE` | no revision, snapshot or MCP resource by that name |
| `RESOURCE_LIMIT` | a configured limit was reached |
| `READ_ONLY` | a change under `--readonly` |
| `MUTATION_CONFLICT` | a change names one path twice |
| `INVALID_CSV` | an import file that is not CSV |
| `SNAPSHOT_EXISTS` | a snapshot name already taken |
| `DECISION_REQUIRED` | a confirmation, a policy choice or an authorisation is needed |
| `LOCK_CONTENDED` | another writer held the lock past the wait |
| `CONCURRENT_MODIFICATION` | a file a change was planned against moved before it was written |
| `PATH_INTERFERENCE` | a directory the change needs is no longer a directory |
| `TRANSACTION_INCOMPLETE` | an interrupted transaction cannot be rolled forward safely |
| `RECOVERY_REQUIRED` | a reader met a transaction partway through its renames |

## Warnings

A warning is reported and never sets a status.

| Code | Meaning |
|---|---|
| `TRANSACTION_STAGED` | a transaction is staged but has not begun applying; the rows are unaffected |
| `METADATA_STALE_READONLY` | the state is valid and differs from history; `--readonly` did not record it |

## Lint

Lint findings are warnings, suggestions and information, listed with their
fixes in [Validation]({{ site.baseurl }}/validation#lint). They set a status only under
`--strict`: `7`.
