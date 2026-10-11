---
title: Errors
---

# Errors and exit statuses

Codes are a stable contract: each keeps its meaning, so scripts and agents may
depend on them. A code always carries the same exit status -- the status is a
property of the failure, never of the command that met it -- and this page is
generated from the one registry the binary itself uses.

## Exit statuses

<!-- generated:exits -->
| Status | Meaning |
|---|---|
| `0` | success; for `check`, a valid database |
| `1` | a flag, argument, path, setting or input the command cannot accept |
| `2` | the database is invalid, or a change was refused because it would make it so |
| `3` | another writer or an outside edit moved what the command depended on; nothing was written, and retrying is safe |
| `4` | a name that does not resolve, or a query that cannot be answered as asked -- outside the SQL surface, or beyond its configured limits |
| `5` | an interrupted transaction cannot be completed without a decision |
| `6` | the format, history or metadata cannot be trusted, or I/O failed |
| `7` | findings under `--strict` |
| `8` | inference could not describe the data |
| `9` | a decision is required: confirm with `--yes`, choose a policy, or authorise |
| `10` | there is no database here, and `--no-auto` forbade making one |
| `11` | the filesystem cannot keep the commit protocol's promises |
| `12` | the change is committed and visible, but finishing it on disk failed; recovery completes it -- do not retry the change |
| `13` | whether the change committed cannot be known; inspect the database before retrying |
| `70` | reldir itself failed; please report it |
<!-- end:exits -->

When a command meets several faults at once, the most severe decides its
status: untrustworthy metadata or format (`6`) outranks an interrupted
transaction (`5`), which outranks an invalid state (`2`). A warning is reported
and never sets a status; lint findings set one only under `--strict` (`7`).

A change that fails with `12` (`COMMITTED_RECOVERY_PENDING`) **was made**: it
is durable and every reader already sees it; only finishing it on disk is
outstanding, and the next writing command or `reldir recover` completes it.
Retrying the change would apply it twice. A change that fails with `13`
(`COMMIT_UNKNOWN`) may or may not have been made; look before retrying, or give
every change an `--idempotency-key`, which makes a retry safe whatever happened.

## Codes

<!-- generated:codes -->
### Finding and opening a database

| Code | Exit | Meaning |
|---|---|---|
| <a id="path_not_found"></a>`PATH_NOT_FOUND` | `1` | `--db` or `RELDIR_DB` names something that does not exist |
| <a id="path_not_directory"></a>`PATH_NOT_DIRECTORY` | `1` | `--db` or `RELDIR_DB` names a file |
| <a id="root_ambiguous"></a>`ROOT_AMBIGUOUS` | `1` | a folder of pins without `.db/` sits inside another database; name the one you mean |
| <a id="root_json_ambiguous"></a>`ROOT_JSON_AMBIGUOUS` | `1` | JSON files at the root belong to no table |
| <a id="uninitialized"></a>`UNINITIALIZED` | `10` | there is no `.db/`, and nothing may create one |
| <a id="already_initialized"></a>`ALREADY_INITIALIZED` | `1` | `init` where `.db/` exists |
| <a id="format_unsupported"></a>`FORMAT_UNSUPPORTED` | `6` | `.db/` or a revision is in a format this binary does not read |
| <a id="format_missing"></a>`FORMAT_MISSING` | `6` | `.db/` has no format marker and holds history or a transaction; `--rebuild-metadata` discards the history |
| <a id="format_upgrade_required"></a>`FORMAT_UPGRADE_REQUIRED` | `6` | `.db/` is in format 2; `reldir upgrade` converts it |
| <a id="config_invalid"></a>`CONFIG_INVALID` | `1` | `.db/config` is unreadable, too large, or holds an unknown or unusable setting |
| <a id="unsafe_filesystem"></a>`UNSAFE_FILESYSTEM` | `11` | a write on a network, cluster, FUSE, FAT, shared-folder or unrecognised filesystem without `allow_remote_filesystem` |
| <a id="read_only"></a>`READ_ONLY` | `1` | a change through a database opened read-only |
| <a id="usage"></a>`USAGE` | `1` | an argument the command cannot accept |
| <a id="input_invalid"></a>`INPUT_INVALID` | `1` | input given to a command is not what it accepts (malformed JSON, CSV or JSON Lines) |
| <a id="io_error"></a>`IO_ERROR` | `6` | a file operation failed; the path is named |

### Structure

| Code | Exit | Meaning |
|---|---|---|
| <a id="unexpected_file"></a>`UNEXPECTED_FILE` | `2` | a file in a table or schema directory that is not `.json` |
| <a id="ungoverned_directory"></a>`UNGOVERNED_DIRECTORY` | `2` | a top-level directory no schema governs |
| <a id="non_regular_file"></a>`NON_REGULAR_FILE` | `2` | a symlink, socket, FIFO, device or hard-linked file where a row or schema belongs |
| <a id="path_collision"></a>`PATH_COLLISION` | `2` | two schema names equal under case or Unicode folding |
| <a id="portability_collision"></a>`PORTABILITY_COLLISION` | `2` | two row files whose names are equal under case or Unicode folding, which one filesystem keeps apart and another merges |
| <a id="path_violation"></a>`PATH_VIOLATION` | `1` | a path that escapes the root or reaches into reldir's metadata, or an unusable name |
| <a id="filename_too_long"></a>`FILENAME_TOO_LONG` | `2` | a row's identity renders to a file name longer than the filesystem allows |
| <a id="filename_rule_undetermined"></a>`FILENAME_RULE_UNDETERMINED` | `2` | a key needs a character the table's file names have never used, so no name can be chosen without inventing a convention |
| <a id="cross_device"></a>`CROSS_DEVICE` | `11` | a table directory is on another device than `.db/`, so a change cannot be renamed into place atomically |

### Schemas

| Code | Exit | Meaning |
|---|---|---|
| <a id="schema_invalid_json"></a>`SCHEMA_INVALID_JSON` | `2` | the schema file is not JSON |
| <a id="schema_missing_required"></a>`SCHEMA_MISSING_REQUIRED` | `2` | a required member of the schema is absent |
| <a id="schema_unknown_key"></a>`SCHEMA_UNKNOWN_KEY` | `2` | a member that is no keyword, at any depth; the message names the nearest one |
| <a id="schema_unknown_format"></a>`SCHEMA_UNKNOWN_FORMAT` | `2` | a `format` that is no format JSON Schema defines |
| <a id="schema_grammar"></a>`SCHEMA_GRAMMAR` | `2` | the document breaks the dialect's grammar, or does not compile |
| <a id="schema_ref_external"></a>`SCHEMA_REF_EXTERNAL` | `2` | a `$ref` leaves the document |
| <a id="schema_table_name_mismatch"></a>`SCHEMA_TABLE_NAME_MISMATCH` | `2` | `x-reldir.table` is not the file's name |
| <a id="schema_invalid_table_name"></a>`SCHEMA_INVALID_TABLE_NAME` | `2` | a table name breaks the naming rule |
| <a id="schema_type_unknown"></a>`SCHEMA_TYPE_UNKNOWN` | `2` | an unknown column type |
| <a id="schema_type_tag_conflict"></a>`SCHEMA_TYPE_TAG_CONFLICT` | `2` | an `x-reldir-type` tag contradicts the subschema's `type` |
| <a id="schema_column_unknown"></a>`SCHEMA_COLUMN_UNKNOWN` | `2` | a constraint names a column that does not exist |
| <a id="schema_column_exists"></a>`SCHEMA_COLUMN_EXISTS` | `2` | a migration adds a column that exists |
| <a id="schema_table_exists"></a>`SCHEMA_TABLE_EXISTS` | `2` | a migration adds a table that exists |
| <a id="schema_constraint_exists"></a>`SCHEMA_CONSTRAINT_EXISTS` | `2` | a migration adds a constraint that exists |
| <a id="schema_identity_invalid"></a>`SCHEMA_IDENTITY_INVALID` | `2` | `x-reldir.identity` names unknown, repeated, nullable, optional or non-scalar columns |
| <a id="schema_default_type_mismatch"></a>`SCHEMA_DEFAULT_TYPE_MISMATCH` | `2` | a default or generator does not fit its column, or a column has both |
| <a id="schema_constraint_name_duplicate"></a>`SCHEMA_CONSTRAINT_NAME_DUPLICATE` | `2` | two constraints share a name |
| <a id="schema_reference_path_invalid"></a>`SCHEMA_REFERENCE_PATH_INVALID` | `2` | a reference path does not parse, or cannot reach a scalar through the schema |
| <a id="schema_fk_target_missing"></a>`SCHEMA_FK_TARGET_MISSING` | `2` | a foreign key's target table or domain has no schema |
| <a id="schema_fk_target_invalid"></a>`SCHEMA_FK_TARGET_INVALID` | `2` | a foreign key's target is malformed |
| <a id="schema_fk_target_not_unique"></a>`SCHEMA_FK_TARGET_NOT_UNIQUE` | `2` | a foreign key's target columns are no key of the target |
| <a id="schema_target_keys_overlap"></a>`SCHEMA_TARGET_KEYS_OVERLAP` | `2` | the tables a reference may name do not share one key namespace |
| <a id="schema_fk_type_mismatch"></a>`SCHEMA_FK_TYPE_MISMATCH` | `2` | a reference cannot equal the key it names |
| <a id="schema_fk_action_invalid"></a>`SCHEMA_FK_ACTION_INVALID` | `2` | a referential action could write a value the schema or contract forbids |
| <a id="schema_key_not_scalar"></a>`SCHEMA_KEY_NOT_SCALAR` | `2` | a key column is an array, object or json |
| <a id="schema_domain_key_invalid"></a>`SCHEMA_DOMAIN_KEY_INVALID` | `2` | the tables of an identity domain do not key on one comparable column |
| <a id="schema_acyclic_invalid"></a>`SCHEMA_ACYCLIC_INVALID` | `2` | an acyclic graph's edges do not lead from a row to keys of its own table |
| <a id="schema_check_invalid"></a>`SCHEMA_CHECK_INVALID` | `2` | a check is not a deterministic expression over the row's own columns |
| <a id="schema_assertion_invalid"></a>`SCHEMA_ASSERTION_INVALID` | `2` | an assertion is not a deterministic query returning the keys of rows that break it |
| <a id="schema_conflict"></a>`SCHEMA_CONFLICT` | `2` | inference disagrees with an existing schema, and the policy is to fail |

### The observed contract

| Code | Exit | Meaning |
|---|---|---|
| <a id="contract_widening_required"></a>`CONTRACT_WIDENING_REQUIRED` | `2` | a change would write a value the table's contract has never admitted; widening it is a decision |
| <a id="contract_exceeded"></a>`CONTRACT_EXCEEDED` | `2` | a file edited outside reldir holds a value the table's inferred contract never admitted |
| <a id="contract_rederived"></a>`CONTRACT_REDERIVED` | `0` | an inferred contract's file was missing and was restored, byte for byte, from recorded history |

### Rows

| Code | Exit | Meaning |
|---|---|---|
| <a id="invalid_json"></a>`INVALID_JSON` | `2` | the file is not one JSON document |
| <a id="merge_conflict"></a>`MERGE_CONFLICT` | `2` | the file holds a merge's conflict markers: both sides of a merge changed a member differently |
| <a id="row_root_not_object"></a>`ROW_ROOT_NOT_OBJECT` | `2` | a row file holds something other than one JSON object |
| <a id="schema_violation"></a>`SCHEMA_VIOLATION` | `2` | a value breaks its subschema |
| <a id="type_mismatch"></a>`TYPE_MISMATCH` | `2` | a value is not of its column's type |
| <a id="row_missing_field"></a>`ROW_MISSING_FIELD` | `2` | a required member is absent |
| <a id="row_unknown_field"></a>`ROW_UNKNOWN_FIELD` | `2` | a member the schema does not declare |
| <a id="not_null_violation"></a>`NOT_NULL_VIOLATION` | `2` | a null where the column admits none |
| <a id="identity_mismatch"></a>`IDENTITY_MISMATCH` | `2` | a file is not named by its row's identity |
| <a id="key_collision"></a>`KEY_COLLISION` | `2` | two rows' identities name one file |
| <a id="file_too_large"></a>`FILE_TOO_LARGE` | `2` | a file is larger than `max_json_file_size`, so it is not read |
| <a id="nesting_too_deep"></a>`NESTING_TOO_DEEP` | `2` | a document nests deeper than `max_nesting_depth`, so it is not read |

### Keys, references and rules

| Code | Exit | Meaning |
|---|---|---|
| <a id="primary_key_violation"></a>`PRIMARY_KEY_VIOLATION` | `2` | two rows share a primary key |
| <a id="unique_violation"></a>`UNIQUE_VIOLATION` | `2` | two rows share a unique key |
| <a id="domain_key_violation"></a>`DOMAIN_KEY_VIOLATION` | `2` | two tables of one identity domain hold the same key |
| <a id="foreign_key_violation"></a>`FOREIGN_KEY_VIOLATION` | `2` | a reference names no row, or a change would leave one that does not |
| <a id="cycle_violation"></a>`CYCLE_VIOLATION` | `2` | a graph declared acyclic holds a cycle |
| <a id="check_violation"></a>`CHECK_VIOLATION` | `2` | a row fails one of its table's checks |
| <a id="assertion_violation"></a>`ASSERTION_VIOLATION` | `2` | a row is named by an assertion over many rows |

### Changes

| Code | Exit | Meaning |
|---|---|---|
| <a id="mutation_conflict"></a>`MUTATION_CONFLICT` | `2` | a change names one file twice, or two changes to one row disagree |
| <a id="migration_refused"></a>`MIGRATION_REFUSED` | `9` | a migration cannot be carried out exactly as the schema is written; the message says what to edit by hand |
| <a id="decision_required"></a>`DECISION_REQUIRED` | `9` | the command needs a confirmation, a policy or an authorisation it was not given |

### Queries and names

| Code | Exit | Meaning |
|---|---|---|
| <a id="resource_limit"></a>`RESOURCE_LIMIT` | `4` | a configured limit was exceeded; the answer is refused, never truncated |
| <a id="query_unsupported"></a>`QUERY_UNSUPPORTED` | `4` | a statement outside the SQL surface, or one that does not parse |
| <a id="query_type_error"></a>`QUERY_TYPE_ERROR` | `4` | a value SQL produced cannot be stored in its column |
| <a id="unknown_table"></a>`UNKNOWN_TABLE` | `4` | no such table |
| <a id="unknown_column"></a>`UNKNOWN_COLUMN` | `4` | no such column |
| <a id="unknown_row"></a>`UNKNOWN_ROW` | `4` | no row has that key |
| <a id="unknown_constraint"></a>`UNKNOWN_CONSTRAINT` | `4` | no constraint has that name |
| <a id="unknown_revision"></a>`UNKNOWN_REVISION` | `4` | no revision has that number or id |
| <a id="unknown_snapshot"></a>`UNKNOWN_SNAPSHOT` | `4` | no snapshot has that name |
| <a id="unknown_resource"></a>`UNKNOWN_RESOURCE` | `4` | no MCP resource has that URI |
| <a id="cursor_stale"></a>`CURSOR_STALE` | `3` | a page cursor belongs to a state the database has since left |

### Transactions and concurrency

| Code | Exit | Meaning |
|---|---|---|
| <a id="lock_contended"></a>`LOCK_CONTENDED` | `3` | another writer held the lock past the wait |
| <a id="concurrent_modification"></a>`CONCURRENT_MODIFICATION` | `3` | a file a change was planned against moved before it was written |
| <a id="path_interference"></a>`PATH_INTERFERENCE` | `3` | a path a change needs is not what it must be (a directory where a file goes, a symlink ancestor) |
| <a id="plan_changed"></a>`PLAN_CHANGED` | `3` | a planned change no longer matches the state it would be committed to |
| <a id="transaction_incomplete"></a>`TRANSACTION_INCOMPLETE` | `5` | an interrupted transaction's journal cannot be read |
| <a id="recovery_blocked"></a>`RECOVERY_BLOCKED` | `5` | recovery could not place a committed file; the path and the failure are named |
| <a id="committed_recovery_pending"></a>`COMMITTED_RECOVERY_PENDING` | `12` | the change is committed and visible, and finishing it on disk failed; do not retry it |
| <a id="commit_unknown"></a>`COMMIT_UNKNOWN` | `13` | making the commit durable failed and could not be retracted |
| <a id="transaction_expired"></a>`TRANSACTION_EXPIRED` | `3` | a transaction held the writer lock longer than `transaction_timeout_seconds` and was rolled back |
| <a id="transaction_statement"></a>`TRANSACTION_STATEMENT` | `1` | a statement that cannot run inside a transaction, or transaction control out of place |
| <a id="snapshot_read"></a>`SNAPSHOT_READ` | `0` | a writer was committing, so a read answered from the database as of its last commit |

### History and snapshots

| Code | Exit | Meaning |
|---|---|---|
| <a id="internal_metadata_corrupt"></a>`INTERNAL_METADATA_CORRUPT` | `6` | history, an object, the mirror or other metadata fails verification |
| <a id="history_diverged"></a>`HISTORY_DIVERGED` | `0` | history has more than one head; the next recording merges them |
| <a id="snapshot_exists"></a>`SNAPSHOT_EXISTS` | `1` | a snapshot with that name exists |

### Inference

| Code | Exit | Meaning |
|---|---|---|
| <a id="infer_no_rows"></a>`INFER_NO_ROWS` | `8` | a table has no rows to infer from |
| <a id="infer_invalid_json"></a>`INFER_INVALID_JSON` | `8` | a file inference must read is not JSON |
| <a id="infer_root_not_object"></a>`INFER_ROOT_NOT_OBJECT` | `8` | a file inference must read is not one JSON object |
| <a id="infer_non_json_file"></a>`INFER_NON_JSON_FILE` | `8` | a table directory holds a file that is not `.json` |
| <a id="infer_nested_directory"></a>`INFER_NESTED_DIRECTORY` | `8` | a table directory holds a directory |
| <a id="infer_non_regular_file"></a>`INFER_NON_REGULAR_FILE` | `8` | a table directory holds a symlink, device or hard-linked file |
| <a id="infer_key_invalid"></a>`INFER_KEY_INVALID` | `8` | a column chosen with `--pk` is absent or null in a row, so it cannot identify every row |
| <a id="infer_filename_inconsistent"></a>`INFER_FILENAME_INCONSISTENT` | `8` | the key chosen with `--pk` does not name the table's files, and adopting a folder never renames them |

### Lint

| Code | Exit | Meaning |
|---|---|---|
| <a id="lint_schema_unpinned"></a>`LINT_SCHEMA_UNPINNED` | `7` | the table is governed by an inferred contract nobody pinned |
| <a id="lint_nullable_never_null"></a>`LINT_NULLABLE_NEVER_NULL` | `7` | a column admits null, and no row is null |
| <a id="lint_wider_type"></a>`LINT_WIDER_TYPE` | `7` | a column's values all have a narrower type than it declares |
| <a id="lint_enum_candidate"></a>`LINT_ENUM_CANDIDATE` | `7` | a string column with few distinct values |
| <a id="lint_unique_candidate"></a>`LINT_UNIQUE_CANDIDATE` | `7` | every row holds a different value |
| <a id="lint_check_candidate"></a>`LINT_CHECK_CANDIDATE` | `7` | an int that is never negative, a string that is never empty |
| <a id="lint_min_items_candidate"></a>`LINT_MIN_ITEMS_CANDIDATE` | `7` | an array that is never empty |
| <a id="lint_fk_candidate"></a>`LINT_FK_CANDIDATE` | `7` | values at some path all name rows of one table, and no foreign key says so |
| <a id="lint_domain_candidate"></a>`LINT_DOMAIN_CANDIDATE` | `7` | values at some path each name a row of one of several tables whose keys never collide |
| <a id="lint_fk_action_defaulted"></a>`LINT_FK_ACTION_DEFAULTED` | `7` | a foreign key does not say what happens when its target is deleted or re-keyed |
| <a id="lint_pk_not_generated"></a>`LINT_PK_NOT_GENERATED` | `7` | a uuid or ulid key every insert must supply |
| <a id="lint_column_never_populated"></a>`LINT_COLUMN_NEVER_POPULATED` | `7` | no row holds a value in a column |
| <a id="lint_inconsistent_presence"></a>`LINT_INCONSISTENT_PRESENCE` | `7` | some rows have a column and others do not |
| <a id="lint_additional_fields_allowed"></a>`LINT_ADDITIONAL_FIELDS_ALLOWED` | `7` | the schema accepts members it does not declare |
| <a id="lint_non_canonical_formatting"></a>`LINT_NON_CANONICAL_FORMATTING` | `7` | rows not in their table's recorded style |
| <a id="lint_no_description"></a>`LINT_NO_DESCRIPTION` | `7` | a table or column without a description |

### reldir itself

| Code | Exit | Meaning |
|---|---|---|
| <a id="internal_error"></a>`INTERNAL_ERROR` | `70` | reldir failed; the message says what it was doing |
<!-- end:codes -->
