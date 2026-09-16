---
title: Validation
---

# Validation

Three commands, three jobs:

| Command | Question | Writes? |
|---|---|---|
| `check` | Is the database valid? | rebuilds derived state only |
| `lint` | Could the schemas be stronger? | never |
| `doctor` | What can be repaired, and how? | only with `--fix` |

## check

```sh
db check                # full validation
db --readonly check     # validate, mutate no data
db check --strict       # lint findings also fail
```

A full check validates directory structure, schema syntax and semantics, every
row's JSON and conformance, primary keys and identity, unique constraints,
foreign keys, `CHECK` constraints, path rules, indexes, provenance consistency,
transaction recovery state, manifest consistency, and the canonical state hash.

```console
$ db check
VALID: 2 tables, 5 rows, 0 violations, 3 lint findings ({"info": 1, "suggestion": 2}), 4 ms
```

Indexes may be repaired automatically because they are derived, and `check`
reports when it did so. `db --readonly check --format json` is the documented CI
and pre-commit entry point: it exits `2` on `INVALID` and, with `--strict`, `7`
on lint findings.

## lint

Lint reports how an already-valid schema could be tightened. It never modifies
anything. Every finding carries a stable code, a severity, the evidence, and the
`doctor` fix that would apply it.

| Code | Meaning |
|---|---|
| `LINT_SCHEMA_UNPINNED` | the working schema is not pinned in `schema/` |
| `LINT_NULLABLE_NEVER_NULL` | column is nullable but no row is null |
| `LINT_WIDER_TYPE` | `string` holding only uuid/ulid/timestamp/date, or `float` holding only integers |
| `LINT_ENUM_CANDIDATE` | low-cardinality string column |
| `LINT_UNIQUE_CANDIDATE` | all values distinct, no unique constraint |
| `LINT_FK_CANDIDATE` | values match another table's key, no foreign key declared |
| `LINT_FK_NO_INDEX` | foreign key column without an index |
| `LINT_FK_ACTION_DEFAULTED` | foreign key relying on default actions |
| `LINT_CHECK_CANDIDATE` | never-negative int, never-empty string |
| `LINT_COLUMN_NEVER_POPULATED` | column declared, every row null or absent |
| `LINT_INCONSISTENT_PRESENCE` | nullable column present in some rows, absent in others |
| `LINT_ADDITIONAL_FIELDS_ALLOWED` | schema accepts unknown fields |
| `LINT_PK_NOT_GENERATED` | uuid/ulid primary key without a generator |
| `LINT_NON_CANONICAL_FORMATTING` | rows not in canonical formatting (informational) |
| `LINT_NO_DESCRIPTION` | table or column lacks a description (opt in with `--descriptions`) |

Thresholds are configurable. See [Configuration](configuration).

### A note on inferred enums

Inference marks a low-cardinality string column as an `enum` containing **only
the values it observed**. That is the strictest schema consistent with your data,
but a new value later becomes a `TYPE_MISMATCH`. Before pinning, widen any enum
that should be an open `string`.

## doctor

`doctor` unifies `check` and `lint` and attaches fixes.

```sh
db doctor                       # diagnose; print a plan; change nothing
db doctor --fix                 # apply schema and layout fixes
db doctor --fix --allow-data    # also apply fixes that rewrite row files
db doctor --only <CODE|FIX_ID>  # restrict to one category or fix
db doctor --explain <FIX_ID>    # what it does, what it touches, why it is safe
db doctor --dry-run             # show the exact diffs
db doctor --yes                 # non-interactive confirmation
```

### Fix classes

| Class | Touches | Applied when | Confirmation |
|---|---|---|---|
| `derived` | indexes, statistics, manifest | rebuilt on any command that may write | none |
| `schema` | working schemas, and the pin of a pinned table; `FIX_PIN_SCHEMA` creates one | `--fix` | per fix, unless `--yes` |
| `layout` | file renames; bodies unchanged | `--fix` | per fix, unless `--yes` |
| `data` | row file bodies | `--fix --allow-data` | always shown as a diff |
| `manual` | nothing | never | doctor explains what you must decide |

```console
$ db doctor
Doctor plan:
  layout (1):
    FIX_RENAME_TO_IDENTITY  rename users/moved.json to u1.json
      -> users/moved.json
  schema (1):
    FIX_PIN_SCHEMA  pin schema users
      -> schema/users.json
```

### Available fixes

| Fix | Class | From |
|---|---|---|
| `FIX_REINDEX` | derived | stale or corrupt indexes; rebuilt automatically, so the plan reports it only in read-only mode |
| `FIX_MANIFEST` | derived | stale manifest; rebuilt automatically, on the same terms |
| `FIX_TIGHTEN_NULLABLE` | schema | `LINT_NULLABLE_NEVER_NULL` |
| `FIX_NARROW_TYPE` | schema | `LINT_WIDER_TYPE` |
| `FIX_ADD_ENUM` | schema | `LINT_ENUM_CANDIDATE` |
| `FIX_ADD_UNIQUE` | schema | `LINT_UNIQUE_CANDIDATE` |
| `FIX_ADD_FK` | schema | `LINT_FK_CANDIDATE` |
| `FIX_ADD_CHECK` | schema | `LINT_CHECK_CANDIDATE` |
| `FIX_ADD_INDEX` | schema | `LINT_FK_NO_INDEX` |
| `FIX_ADD_GENERATOR` | schema | `LINT_PK_NOT_GENERATED` |
| `FIX_PIN_SCHEMA` | schema | `LINT_SCHEMA_UNPINNED` |
| `FIX_RENAME_TO_IDENTITY` | layout | `IDENTITY_MISMATCH` |
| `FIX_COERCE_VALUE` | data | `TYPE_MISMATCH`, lossless conversions only; a value that misses a `pattern` has none, so it is reported for a person to decide |
| `FIX_RENAME_FIELD` | data | `ROW_UNKNOWN_FIELD` near-matching a missing column |
| `FIX_DROP_UNKNOWN_FIELD` | data | `ROW_UNKNOWN_FIELD` |
| `FIX_ORPHAN_SET_NULL` | data | `FOREIGN_KEY_VIOLATION` on a nullable column |
| `FIX_ORPHAN_DELETE_ROW` | data | `FOREIGN_KEY_VIOLATION` |
| `FIX_CANONICALIZE` | data | `LINT_NON_CANONICAL_FORMATTING`, opt-in only |

Where several fixes could resolve one violation, doctor presents the
alternatives and defaults to the least destructive. An orphan, for example, can
be nulled, deleted, or given a parent.

`FIX_RENAME_FIELD` fires only when the intent is unambiguous: the candidate
column must be a close match, must be missing from the row, and must be strictly
closer than every other candidate. Doctor does not resolve a tie.

### Safety

- A run that applies anything is one transaction: all confirmed fixes commit, or
  none do.
- Before any `data` or `layout` fix, doctor creates a snapshot named
  `pre-doctor-<revision>` unless `--no-snapshot` is given, and prints how to
  restore it.
- Doctor re-validates the full prospective state before committing. A fix set that
  would leave the database invalid is rejected as a whole, with the residual
  violations listed.
- Nothing lossy happens until you have seen the affected paths in the plan and
  confirmed them.
- In `--format json` mode, `--yes` is required to apply.

Automatic repair applies only to rebuildable derived state. `jdb` never
rewrites your JSON without confirmation because it violates a schema.
