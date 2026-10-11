---
title: Validation
---

# Validation

Three commands judge a database:

| Command | Answers | Writes |
|---|---|---|
| `reldir check` | whether the database is valid | only history and derived state |
| `reldir lint` | how the schemas could say more than they do | nothing |
| `reldir doctor` | what can be repaired, and how | only with `--fix` |

Every fault and finding has a code; the [error reference]({{ site.baseurl }}/errors)
lists each with its meaning and exit status. Repairing a database step by step
is described in [How to repair an invalid database]({{ site.baseurl }}/repair-a-database).

## Check

`reldir check` judges the whole database:

- the folder's structure: directories no schema governs, names that
  filesystems fold together, files too large or too deep to read;
- every schema, against the [dialect]({{ site.baseurl }}/schemas#the-dialect);
- every row, against its table's schema and column types;
- every file name, against its row's identity;
- every key, unique constraint and identity domain;
- every reference, acyclic graph, check and assertion;
- interrupted transactions and the state of recorded history.

```console
$ reldir check
VALID: 3 table(s), 7 row(s), 0 violation(s), 0 warning(s), 1 lint finding(s), [..] ms
```

### Faults

A fault names its file, line and column, the JSON Pointer of the value at
fault, the rule it breaks, and the fixes doctor can make for it, when there
are any:

```console
$ echo '{"id": "p9", "user_id": "ada", "title": ""}' > posts/p9.json
$ reldir check
error[SCHEMA_VIOLATION]: the value at /title: "" is shorter than 1 character
  --> posts/p9.json:1:41
   |
 1 | {"id": "p9", "user_id": "ada", "title": ""}
   |                                         ^
   = at: /title
   = constraint: /properties/title/minLength
   = observed: ""
run `reldir doctor` for repairs, least destructive first
INVALID: 3 table(s), 8 row(s), 1 violation(s), 0 warning(s), 0 lint finding(s), [..] ms
$ rm posts/p9.json
```

| Line | Holds |
|---|---|
| `-->` | the file, line and column |
| `at` | the JSON Pointer of the value |
| `constraint` | the schema location or rule broken |
| `expected`, `observed` | what the rule admits, and what the file holds |
| `fixes` | the doctor fixes that apply, least destructive first |
| `help` | what to do when no fix applies |

A row file holding a merge's conflict markers is `MERGE_CONFLICT`, not
`INVALID_JSON`.

### Exit status

| Status | When |
|---|---|
| `0` | the database is valid |
| `2` | the database is invalid |
| `7` | with `--strict`, warnings or lint findings |
| other | a fault that makes the database untrusted or needs recovery; it takes precedence |

Warnings -- `CONTRACT_EXCEEDED`, an assertion of severity `warning` -- are
shown and do not make the database invalid. Lint findings are counted, not
shown, except under `--strict`:

```console
$ reldir check --strict
suggestion[LINT_FK_ACTION_DEFAULTED]: posts.tags[] -> tags does not say what happens when its target is re-keyed, so that is refused
  --> schema/posts.json:50:7
   |
50 |       {
   |       ^
   = at: /x-reldir/foreignKeys/1
VALID: 3 table(s), 7 row(s), 0 violation(s), 0 warning(s), 1 lint finding(s), [..] ms
```

### Options

| Option | Effect |
|---|---|
| `--strict` | shows warnings and lint findings, and exits `7` when there are any |
| `--deep` | also verifies every object recorded history holds, and the mirror under SQLite's integrity check; a fault there is `INTERNAL_METADATA_CORRUPT` |
| `--readonly` (global) | writes nothing at all, not even history or derived state |
| `--format sarif` (global) | prints a SARIF 2.1.0 log; each rule links to its code in the error reference |

### Schemas alone

`reldir schema validate` judges the schemas without reading rows:

```console
$ reldir schema validate
3 schema(s), 0 fault(s)
```

## Lint

`reldir lint` reports how a valid database's schemas could say more than they
do, and changes nothing. Each finding carries its evidence and, where there is
one, the fix that applies it.

```console
$ reldir lint
suggestion[LINT_FK_ACTION_DEFAULTED]: posts.tags[] -> tags does not say what happens when its target is re-keyed, so that is refused
  --> schema/posts.json:50:7
   |
50 |       {
   |       ^
   = at: /x-reldir/foreignKeys/1
1 lint finding(s)
```

| Form | Effect |
|---|---|
| `reldir lint TABLE` | lints one table |
| `--descriptions` | also reports tables and columns without a description |
| `--strict` | exits `7` when there are findings |

| Code | Finding |
|---|---|
| `LINT_SCHEMA_UNPINNED` | The table is governed by an inferred contract nobody pinned. |
| `LINT_NULLABLE_NEVER_NULL` | A column admits null, and no row is null. |
| `LINT_WIDER_TYPE` | A column's values all have a narrower type than it declares. |
| `LINT_ENUM_CANDIDATE` | A string column with few distinct values. |
| `LINT_UNIQUE_CANDIDATE` | Every row holds a different value. |
| `LINT_CHECK_CANDIDATE` | An `int` that is never negative, or a string that is never empty. |
| `LINT_MIN_ITEMS_CANDIDATE` | An array that is never empty. |
| `LINT_FK_CANDIDATE` | The values at some path all name rows of one table, and no foreign key says so. |
| `LINT_DOMAIN_CANDIDATE` | The values at some path each name a row of one of several tables whose keys never collide. |
| `LINT_FK_ACTION_DEFAULTED` | A foreign key does not say what happens when its target is deleted or re-keyed. |
| `LINT_PK_NOT_GENERATED` | A `uuid` or `ulid` key that every insert must supply. |
| `LINT_COLUMN_NEVER_POPULATED` | No row holds a value in a column. |
| `LINT_INCONSISTENT_PRESENCE` | Some rows have a column and others do not. |
| `LINT_ADDITIONAL_FIELDS_ALLOWED` | The schema accepts members it does not declare. |
| `LINT_NON_CANONICAL_FORMATTING` | Rows not written in their table's recorded style. |
| `LINT_NO_DESCRIPTION` | A table or column without a description; only with `--descriptions`. |

The thresholds come from [configuration]({{ site.baseurl }}/configuration#inference-and-lint):
an enum is proposed for at most `enum_max_values` distinct values, and a
unique constraint or a check only over at least `unique_min_rows` rows.

References are found by one analysis that lint shares with inference. Every
scalar place in the rows, such as `modules[].lessons[].lesson_ref` or
`relations[].target`, is a candidate, and it is proposed when every value it
holds is a key. Inference declares a reference itself only when the place is
named as one, by the `reference_naming` patterns; lint reports the rest.

## Doctor

`reldir doctor` turns every fault `check` finds, and every lint finding that
has a remedy, into concrete fixes: the exact documents, rows or renames each
would write.

| Form | Effect |
|---|---|
| `reldir doctor` | lists the fixes; writes nothing |
| `reldir doctor --fix` | applies each fault's default fix that changes schemas or file names |
| `reldir doctor --fix --allow-data` | also applies fixes that rewrite rows |
| `reldir doctor --fix --only ID` | applies one fix, or the default fix for one code |
| `reldir doctor --explain ID` | says what a fix does |
| `--dry-run` | shows what `--fix` would write, and writes nothing |
| `--no-snapshot` | skips the snapshot taken before a repair |

The listing has one row per fix:

| Column | Holds |
|---|---|
| `id` | the fix |
| `class` | `data`, `layout`, `schema` or `manual` |
| `description` | what it would do, to which file |
| `resolves` | the code of the fault or finding |
| `at` | where the fault is |
| `default` | whether `--fix` applies it |
| `destructive` | whether it removes data |
| `paths` | the files it would write or remove |

```console
$ reldir doctor --explain FIX_REMOVE_REFERENCE
id                   | explanation
---------------------+--------------------------------------------------------------------------------------------------------------------------------
FIX_REMOVE_REFERENCE | removes the dangling reference: the array element that holds it, or -- where no array holds it -- the value, which becomes null
(1 row)
FIX_REMOVE_REFERENCE: removes the dangling reference: the array element that holds it, or -- where no array holds it -- the value, which becomes null
```

### Exit status

`reldir doctor` without `--fix` exits `0`, whatever it lists. With `--fix`:

| Exit | When |
|---|---|
| `0` | the database is valid afterwards, or a `--dry-run` planned the repairs |
| the fault's own, as `check` | the database is still invalid afterwards: a repair was made and faults remain, or no fix repairs what is left |
| `9` (`DECISION_REQUIRED`) | the only repairs rewrite rows and `--allow-data` was not given, or a fix removes data and was not confirmed; nothing is written |

### Alternatives

When several fixes resolve one fault, they are listed least destructive
first, and the first is the default. A reference to a row that is gone is
answered, in this order, by:

1. `FIX_RESTORE_TARGET`: restore the row it names, byte for byte, from the
   last revision that had it.
2. `FIX_REMOVE_REFERENCE`: remove the array element that holds the reference
   or, where no array holds it, set the value to null; offered only where the
   schema admits that.
3. `FIX_ORPHAN_DELETE_ROW`: delete the row that holds the reference, with
   whatever its own referrers' actions require.

### Fixes

| Fix | Class | Resolves |
|---|---|---|
| `FIX_RESTORE_TARGET` | data | `FOREIGN_KEY_VIOLATION`, when recorded history has the missing row. |
| `FIX_REMOVE_REFERENCE` | data | `FOREIGN_KEY_VIOLATION`, inside an array or on a value that admits null. |
| `FIX_ORPHAN_DELETE_ROW` | data | `FOREIGN_KEY_VIOLATION`. |
| `FIX_RENAME_TO_IDENTITY` | layout | `IDENTITY_MISMATCH`: renames the file; its contents do not change. |
| `FIX_RENAME_FIELD` | data | `ROW_UNKNOWN_FIELD` that is clearly a misspelling of a missing column. |
| `FIX_DROP_UNKNOWN_FIELD` | data | `ROW_UNKNOWN_FIELD`. |
| `FIX_COERCE_VALUE` | data | `TYPE_MISMATCH`, when the same value written as the column's type loses nothing. |
| `FIX_FILL_DEFAULT` | data | `ROW_MISSING_FIELD` or `NOT_NULL_VIOLATION`, on a column with a default that is not null. |
| `FIX_WIDEN_CONTRACT` | schema | `CONTRACT_EXCEEDED`: widens an inferred contract just enough to admit what outside programs wrote. |
| `FIX_PIN_SCHEMA` | schema | `LINT_SCHEMA_UNPINNED`. |
| `FIX_TIGHTEN_NULLABLE` | schema | `LINT_NULLABLE_NEVER_NULL`. |
| `FIX_NARROW_TYPE` | schema | `LINT_WIDER_TYPE`. |
| `FIX_ADD_ENUM` | schema | `LINT_ENUM_CANDIDATE`. |
| `FIX_ADD_UNIQUE` | schema | `LINT_UNIQUE_CANDIDATE`. |
| `FIX_ADD_CHECK` | schema | `LINT_CHECK_CANDIDATE`. |
| `FIX_ADD_MIN_ITEMS` | schema | `LINT_MIN_ITEMS_CANDIDATE`. |
| `FIX_ADD_FK` | schema | `LINT_FK_CANDIDATE` and `LINT_DOMAIN_CANDIDATE`, joining tables to an identity domain where needed. |
| `FIX_ADD_GENERATOR` | schema | `LINT_PK_NOT_GENERATED`. |
| `FIX_CANONICALIZE` | data | `LINT_NON_CANONICAL_FORMATTING`: rewrites the files in their table's style; the rows do not change. |
| `FIX_MANUAL` | manual | A fault with no safe repair; doctor says what to look at. |

A fault offers a fix only when that fix would work for that row: a coercion
only when it makes the row valid, a default only when the column declares one,
and a rename only when one missing column is the closest match, within two
edits.

### Rules

- `--fix` applies each fault's default fix. `--only` names a fix or a code;
  naming an alternative chooses it over the default.
- Two fixes that touch one file are never applied together. The second is
  deferred, and doctor says to run it again.
- A fix that changes what the database promises is applied only when `--only`
  names it or the code it resolves: `FIX_WIDEN_CONTRACT` and every fix a lint
  finding proposes. A plain `--fix` repairs faults; it never tightens or
  widens a contract.
- Fixes that rewrite rows need `--allow-data`. Fixes that remove data
  (`FIX_REMOVE_REFERENCE`, `FIX_DROP_UNKNOWN_FIELD` and
  `FIX_ORPHAN_DELETE_ROW`) also need confirmation, or `--yes`.
- Before it rewrites a row or renames a file, doctor takes a snapshot named
  `pre-doctor-<revision>` and prints how to restore it, unless `--no-snapshot`
  is given.
- A repair may leave faults it does not address, and never adds one: if the
  resulting state has a fault the current state lacks, nothing is written.
- Fixes from lint findings are offered only for a valid database.
- A repair that returns the database to a state history already holds is
  reported with that revision; no new revision is recorded.
