---
title: Validation
---

# Validation

Three commands, three questions:

| Command | Question | Writes |
|---|---|---|
| `check` | Is the database valid? | nothing but history and derived state |
| `lint` | Could the schemas say more than they do? | nothing |
| `doctor` | What can be repaired, and how? | only with `--fix` |

## check

```console
$ reldir check
VALID: 3 table(s), 7 row(s), 0 violation(s), 0 warning(s), 1 lint finding(s), [..] ms
```

A full check judges the directory's structure, every schema, every row against
its schema, every file name against its row's identity, every key and unique
constraint, identity domains, every reference, every acyclic graph, every check
and every assertion, recovery state, and history. Each fault names its file,
the line and column, and the JSON Pointer of the value at fault:

```console
$ echo '{"id": "p9", "user_id": "ada", "title": ""}' > posts/p9.json
$ reldir check
error[SCHEMA_VIOLATION]: the value at /title: "" is shorter than 1 character
  --> posts/p9.json:1:41
[..]
INVALID: [..]
$ rm posts/p9.json
```

`check` exits `2` when the database is invalid; `--strict` also exits `7` on
warnings and lint findings. `reldir --readonly check --format sarif` is the
CI entry point: it writes nothing at all and produces a SARIF 2.1.0 log that
code-scanning tools annotate pull requests with.

Checking is incremental. The mirror remembers each file's size, times and
inode, and a file whose stat is unchanged is not read again; an unchanged
database is judged in the time it takes to stat it.

## lint

Lint reports how a valid database's schemas could be stronger. It never changes
anything; each finding carries the evidence and, where one exists, the fix that
applies it.

| Code | Meaning |
|---|---|
| `LINT_SCHEMA_UNPINNED` | the table is governed by an inferred working schema, which deleting `.db/` would lose |
| `LINT_NULLABLE_NEVER_NULL` | a column admits null, and no row is null |
| `LINT_WIDER_TYPE` | a `string` holding only uuids, ulids, timestamps or dates, or a `float` holding only integers |
| `LINT_ENUM_CANDIDATE` | a string column with few distinct values |
| `LINT_UNIQUE_CANDIDATE` | every row holds a different value, over at least `unique_min_rows` rows |
| `LINT_CHECK_CANDIDATE` | an int that is never negative, a string that is never empty |
| `LINT_FK_CANDIDATE` | values at some path, at any depth, all name rows of one table, and no foreign key says so |
| `LINT_DOMAIN_CANDIDATE` | values at some path each name a row of one of several tables whose keys never collide: an identity domain |
| `LINT_FK_ACTION_DEFAULTED` | a foreign key does not say what happens when its target is deleted or re-keyed |
| `LINT_PK_NOT_GENERATED` | a uuid or ulid key every insert must supply |
| `LINT_COLUMN_NEVER_POPULATED` | no row holds a value in a column |
| `LINT_INCONSISTENT_PRESENCE` | some rows have a column and others do not |
| `LINT_ADDITIONAL_FIELDS_ALLOWED` | the schema accepts members it does not declare |
| `LINT_NON_CANONICAL_FORMATTING` | rows not in canonical formatting (informational) |
| `LINT_NO_DESCRIPTION` | a table or column without a description (with `--descriptions`) |

References are found by one analysis shared with inference: every scalar leaf
of the rows -- `modules[].lessons[].lesson_ref`, `relations[].target` -- is a
candidate, and it is proposed when every value it holds is a key. Inference
declares a proposal on its own only when the leaf is *named* as a reference to
its target (the `reference_naming` patterns in
[configuration]({{ site.baseurl }}/configuration)); lint reports the rest.

## doctor

`doctor` turns every fault `check` finds, and every lint finding that has a
remedy, into concrete fixes -- the exact documents, rows or renames each would
write -- and applies them on request.

```sh
reldir doctor                        # the plan; changes nothing
reldir doctor --fix                  # apply schema and layout fixes
reldir doctor --fix --allow-data     # also fixes that rewrite rows
reldir doctor --fix --only FIX_ID    # one fix, or one alternative
reldir doctor --explain FIX_ID       # what a fix does
```

Where several fixes could resolve one fault they are listed least destructive
first, and the first is the default. A reference to a row that is gone is
answered, in order, by:

1. `FIX_RESTORE_TARGET` -- restore the row it names, byte for byte, from the
   last revision that had it;
2. `FIX_REMOVE_REFERENCE` -- remove the array element holding the reference,
   or, where no array holds it, set it to null;
3. `FIX_ORPHAN_DELETE_ROW` -- delete the row holding the reference, with
   whatever its own referrers' actions require.

```console
$ rm users/ada.json
$ reldir doctor
[..]
FIX_RESTORE_TARGET    | data  | restore users/ada.json from revision 1, which posts/p1.json still names [..]
[..]
$ reldir doctor --fix --allow-data --yes
[..]
repaired: 1 file(s), revision 1 (1 fix(es): FIX_RESTORE_TARGET)
```

Restoring what history already recorded leaves the state history has, so it
records no new revision.

### Fixes

| Fix | Class | From |
|---|---|---|
| `FIX_RESTORE_TARGET` | data | `FOREIGN_KEY_VIOLATION`, when history has the missing row |
| `FIX_REMOVE_REFERENCE` | data | `FOREIGN_KEY_VIOLATION` inside an array, or on a nullable value |
| `FIX_ORPHAN_DELETE_ROW` | data | `FOREIGN_KEY_VIOLATION` |
| `FIX_RENAME_TO_IDENTITY` | layout | `IDENTITY_MISMATCH` |
| `FIX_RENAME_FIELD` | data | `ROW_UNKNOWN_FIELD` that is unambiguously a misspelt missing column |
| `FIX_DROP_UNKNOWN_FIELD` | data | `ROW_UNKNOWN_FIELD` |
| `FIX_COERCE_VALUE` | data | `TYPE_MISMATCH` with a lossless conversion that makes the row valid |
| `FIX_FILL_DEFAULT` | data | `ROW_MISSING_FIELD` or `NOT_NULL_VIOLATION` on a column with a default |
| `FIX_PIN_SCHEMA` | schema | `LINT_SCHEMA_UNPINNED` |
| `FIX_TIGHTEN_NULLABLE` | schema | `LINT_NULLABLE_NEVER_NULL` |
| `FIX_NARROW_TYPE` | schema | `LINT_WIDER_TYPE` |
| `FIX_ADD_ENUM` | schema | `LINT_ENUM_CANDIDATE` |
| `FIX_ADD_UNIQUE` | schema | `LINT_UNIQUE_CANDIDATE` |
| `FIX_ADD_CHECK` | schema | `LINT_CHECK_CANDIDATE` |
| `FIX_ADD_FK` | schema | `LINT_FK_CANDIDATE`, and `LINT_DOMAIN_CANDIDATE` |
| `FIX_ADD_GENERATOR` | schema | `LINT_PK_NOT_GENERATED` |
| `FIX_CANONICALIZE` | data | `LINT_NON_CANONICAL_FORMATTING` |
| `FIX_MANUAL` | manual | any fault with no safe repair: doctor explains what to look at |

A fault offers a fix only when that fix would work for that row: a coercion
only when one makes the row valid, a default only when the column declares one,
a rename only when exactly one missing column is within two edits.

### Safety

- `--fix` applies each problem's default fix; `--only` chooses an alternative.
  Two fixes that touch one file are never applied together: the second is
  deferred to the next run.
- Fixes that rewrite rows need `--allow-data`, and fixes that remove data
  (`FIX_REMOVE_REFERENCE`, `FIX_DROP_UNKNOWN_FIELD`, `FIX_ORPHAN_DELETE_ROW`)
  ask for confirmation, or `--yes`.
- Before rewriting any row or renaming any file, doctor takes a snapshot named
  `pre-doctor-<revision>` (unless `--no-snapshot`) and says how to restore it.
- A repair may leave faults it does not address, but can never add one: the
  prospective state must have no fault the current one lacks, or nothing is
  written.
- Schema refinements are offered only for a valid database.
