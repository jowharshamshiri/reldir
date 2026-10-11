---
title: Schemas
---

# Schemas

Each table has one schema: a JSON Schema 2020-12 document. Every row of the
table is validated against it, as any 2020-12 validator would validate it.
Facts that JSON Schema has no keyword for (identity, uniqueness, references,
rules over many rows) are stated in the same document, under `x-reldir`.

## Where a schema lives

| File | What it is |
|---|---|
| `schema/<table>.json` | A **pin**: a schema a person declared and keeps with the data. |
| `.db/schema/<table>.json` | An **inferred contract**: the schema reldir inferred when it adopted the folder, for a table nobody has pinned. |

Both are versioned in git (see [configuration]({{ site.baseurl }}/configuration#version-control)).
When reldir changes a table's schema, through a migration, a repair or a
widening, it writes the change where that schema lives.

```console
$ reldir schema show users
{
  "$schema": "https://reldir.dev/schema/reldir-3",
  "type": "object",
  "properties": {
    "id": {
      "type": "string"
    },
    "name": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "name"
  ],
  "additionalProperties": false,
  "x-reldir": {
    "table": "users",
    "identity": {
      "source": "body",
      "columns": [
        "id"
      ],
      "naming": {
        "rule": "reldir"
      }
    }
  }
}
```

A table name matches `^[a-z][a-z0-9_]*$` and equals the schema's file name.
`x-reldir.table` must say the same name (`SCHEMA_TABLE_NAME_MISMATCH`). A
name that breaks the rule is `SCHEMA_INVALID_TABLE_NAME`.

## The dialect

Every table schema says `"$schema": "https://reldir.dev/schema/reldir-3"`. The
URI names the dialect; nothing is fetched from it. The dialect is built into
the binary. `reldir schema dialect` prints it, so an editor can validate and
complete schemas while you write them:

```sh
reldir schema dialect > .reldir-dialect.json
```

```json
{
  "json.schemas": [
    { "fileMatch": ["schema/*.json", ".db/schema/*.json"], "url": "./.reldir-dialect.json" }
  ]
}
```

### Standard keywords

Every keyword of JSON Schema 2020-12 is available and keeps its standard
meaning: `type`, `enum`, `const`, the bounds on strings, numbers, arrays and
objects, `pattern`, `properties`, `patternProperties`,
`additionalProperties`, `propertyNames`, `required`, `dependentRequired`,
`dependentSchemas`, `items`, `prefixItems`, `contains`,
`unevaluatedProperties`, `unevaluatedItems`, `allOf`, `anyOf`, `oneOf`, `not`,
`if`/`then`/`else`, `$defs` and `$ref`.

`format` asserts. The dialect turns on the format-assertion vocabulary, so a
value that does not have the form of `date`, `date-time`, `uuid`, `email`,
`uri`, `ipv4` or another standard format is refused. An unknown format is
`SCHEMA_UNKNOWN_FORMAT`.

A `$ref` must point inside the same document, such as `#/$defs/address`. A
reference to another file or a URL is `SCHEMA_REF_EXTERNAL`.

### A closed dialect

An unknown keyword, at any depth, is `SCHEMA_UNKNOWN_KEY`, and the message
names the closest known keyword:

```console
$ echo '{"$schema": "https://reldir.dev/schema/reldir-3", "type": "object", "properties": {"id": {"type": "string"}, "text": {"type": "string", "minLenght": 1}}, "required": ["id"], "additionalProperties": false, "x-reldir": {"table": "notes", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}}' > schema/notes.json
$ reldir schema validate
error[SCHEMA_UNKNOWN_KEY]: "minLenght" is not a JSON Schema or reldir keyword (did you mean "minLength"?); an unknown keyword would constrain nothing
  --> schema/notes.json:1:110
[..]
   = at: /properties/text
4 schema(s), 1 fault(s)
$ rm schema/notes.json
```

`reldir schema validate` judges the schemas alone, without reading rows.

Your own annotations may use any name that begins with `x-`, except names that
begin with `x-reldir`. `"x-ui-widget": "textarea"` is kept and has no effect.
`x-reldir` itself is allowed only at the root of a table schema.

### Annotations

`title`, `description`, `$comment`, `examples`, `deprecated`, `readOnly`,
`writeOnly` and third-party `x-` members describe a schema without
constraining a row.

## The root

The root of a table schema is an object schema with these members:

| Member | Required | Meaning |
|---|---|---|
| `$schema` | yes | The dialect URI. |
| `type` | yes | `"object"`. |
| `properties` | yes | The columns; at least one. |
| `required` | no | The columns a row may not leave out. |
| `additionalProperties` | yes | `false` refuses members the schema does not declare (`ROW_UNKNOWN_FIELD`); `true` keeps them. |
| `x-reldir` | yes | The relational facts described below. |

A missing member is `SCHEMA_MISSING_REQUIRED`. `additionalProperties` has no
default.

Any other standard keyword may appear at the root and applies to the whole
row, such as `if`/`then`/`else` for a conditional requirement:

```json
{
  "if": { "properties": { "mode": { "const": "blueprint" } }, "required": ["mode"] },
  "then": { "required": ["blueprint"] }
}
```

## Column types

Each member of `properties` is a column. Its subschema gives it one relational
type, which decides how the value is compared, sorted, keyed and shown to SQL.

| Type | Subschema |
|---|---|
| `bool` | `{"type": "boolean"}` |
| `int` | `{"type": "integer", "x-reldir-type": "int"}` |
| `integer` | `{"type": "integer"}` |
| `float` | `{"type": "number", "x-reldir-type": "float"}` |
| `number` | `{"type": "number"}` |
| `decimal` | `{"type": "string", "x-reldir-type": "decimal"}` |
| `ulid` | `{"type": "string", "x-reldir-type": "ulid"}` |
| `enum` | `{"type": "string", "enum": [...]}`, or `const` |
| `bytes` | `{"type": "string", "contentEncoding": "base64"}` |
| `date` | `{"type": "string", "format": "date"}` |
| `timestamp` | `{"type": "string", "format": "date-time"}` |
| `uuid` | `{"type": "string", "format": "uuid"}` |
| `string` | `{"type": "string"}` |
| `array` | `{"type": "array", ...}` |
| `object` | `{"type": "object", ...}` |
| `json` | anything else: `{}`, several types, a `$ref` that recurses |

A type is read through a chain of local `$ref`s, so a column defined in
`$defs` has the type of its definition. A `$ref` that recurses, as in a tree
whose nodes hold nodes, makes the column `json`.

A column is nullable when `"null"` is among its types:
`{"type": ["string", "null"]}`. An `enum` judges every value, null included, so
a nullable `enum` also lists `null` among its members. A `json` column is
always nullable.

### Written forms

`x-reldir-type` marks the types that a standard keyword cannot tell apart.
Each enforces how a value is written:

- `int` is an integer written without a fraction or an exponent that fits in
  64 signed bits: `1`, never `1.0` or `1e0`.
- `float` is a number written with a fraction or an exponent: `1.0`, never `1`.
- `decimal` is a string in canonical form, matching
  `^-?(0|[1-9][0-9]*)(\.[0-9]*[1-9])?$`: no leading zeros, no trailing
  fractional zeros, no `+` and no `-0`. Decimals compare by exact value.
- `ulid` is 26 uppercase Crockford base32 characters.

The standard types have rules of their own. A `uuid` is lowercase and
hyphenated. `bytes` must be standard base64 that decodes. A `timestamp`
carries an offset and compares as the instant it denotes, so
`2024-01-01T10:00:00Z` and `2024-01-01T12:00:00+02:00` are the same key.

A tag refines exactly one JSON type: `int` refines `integer`, `float` refines
`number`, and `decimal` and `ulid` refine `string`. A tag that contradicts the
subschema's `type` is `SCHEMA_TYPE_TAG_CONFLICT`.

### Presence, null and defaults

These are separate questions:

- `required` decides whether a row may leave a member out.
- `"null"` in `type` decides whether the value may be null.
- `default` says what a member that was left out reads as.

A member that was left out stays out of the file. It reads as its default in
SQL, in keys and in references.

## `x-reldir`

| Key | Meaning |
|---|---|
| `table` | The table's name. Required. |
| `identity` | Which values identify a row, and how they name its file. Required. |
| `style` | How a new row's file is written. |
| `unique` | Lists of columns that no two rows may share. |
| `indexes` | Lists of columns to index, for faster queries. |
| `generated` | A column mapped to `uuid`, `ulid`, `now` or `sequence`, to fill it when an insert gives no value. |
| `identityDomain` | A namespace that this table's keys share with other tables. |
| `foreignKeys` | References from this table's rows to rows anywhere. |
| `acyclic` | Reference graphs that must never contain a cycle. |
| `checks` | SQL conditions every row must meet. |
| `assertions` | SQL queries that find rows breaking a rule over many rows. |
| `schemaVersion` | A whole number from 1 that you manage; reldir only keeps it. |

Every name in `x-reldir` (a table, a domain or a constraint) matches
`^[a-z][a-z0-9_]*$`. A key that does not belong is `SCHEMA_UNKNOWN_KEY`. A
value of the wrong shape is `SCHEMA_GRAMMAR`.

### Unique constraints and indexes

```json
{ "unique": [["email"], ["team_id", "handle"]], "indexes": [["created_at"]] }
```

Each list in `unique` is a key no two rows may share (`UNIQUE_VIOLATION`). A
unique key is made of scalars (`SCHEMA_KEY_NOT_SCALAR`). A column that does not
exist is `SCHEMA_COLUMN_UNKNOWN`.

Every key and every reference is indexed already. `indexes` is only for
queries.

### Generated values

```json
{ "generated": { "id": "ulid", "created_at": "now", "number": "sequence" } }
```

| Generator | Column type | Value |
|---|---|---|
| `uuid` | `uuid` | a new random UUID |
| `ulid` | `ulid` | a new ULID |
| `now` | `timestamp` | the time of the insert |
| `sequence` | `int` | one more than the largest value in the table |

A generator whose value does not fit its column, or a column that has both a
generator and a default, is `SCHEMA_DEFAULT_TYPE_MISMATCH`. A generated column
may be left out of `required`, even when it is the key.

## Identity

`x-reldir.identity` says which values identify a row and how they name its
file. It has one of two sources.

### The file name

```json
{ "identity": { "source": "filename", "column": "slug", "type": "string" } }
```

The file name, without `.json`, is the key. The body does not hold it. It is
shown as a column, here `slug`, which is the table's first column in SQL.
`type` is `string`, or `int` when every name is an integer written in
canonical form (no sign other than `-`, no leading zeros, no `-0`). The column
must not also be a member of the body (`SCHEMA_IDENTITY_INVALID`).

### The body

```json
{ "identity": { "source": "body", "columns": ["id"], "naming": { "rule": "reldir" } } }
```

One or more columns of the body are the key, and `naming` says how their values
spell the file name. A key column must exist, be in `required` or have a
generator, not admit null, and be a scalar that spells a name in exactly one
way. `float`, `number`, `bool` and `bytes` columns cannot be keys. Any other
key is `SCHEMA_IDENTITY_INVALID`.

There are three naming rules.

**`{"rule": "reldir"}`** is reldir's percent-encoding. ASCII letters, digits,
`_`, `-` and `.` are written as they are, except a leading or trailing `.`.
Every other byte is written as `%HH` in uppercase. A key that is a Windows
device name (`con`, `nul`, `com1` and the like) has its first byte encoded.
The values of a composite key are joined with `,`. A key spells its name as it
is written: a string exactly as written, an integer by its exact digits.

**`{"rule": "verbatim", "alphabet": "0-9_a-z"}`** uses the key's text as the
name. It takes one key column. The alphabet is a character class: single
characters and `lo-hi` ranges, with `\\` for a backslash, `\-` for a hyphen and
`\u{HEX}` for any character. A key that needs a character outside the
alphabet is `FILENAME_RULE_UNDETERMINED`.

**`{"rule": "template", "prefix": "user-", "suffix": "", "inner": {...}}`**
wraps a `reldir` or `verbatim` rule in fixed text, as in `user-17.json`. At
least one of `prefix` and `suffix` is not empty.

Whatever the rule, a name may not be empty, may not begin with `.`, may not
hold a path separator or NUL, and may not exceed 255 bytes
(`FILENAME_TOO_LONG`). A file whose name is not the one its row's key gives is
`IDENTITY_MISMATCH`; doctor renames the file and leaves its contents as they
are. Two names equal under case or Unicode folding are
`PORTABILITY_COLLISION`: a warning, or an error when `portable_names` is set
(see [configuration]({{ site.baseurl }}/configuration#settings)).

### Inferred identity

When reldir adopts a folder, it records the naming convention the files
already use, and renames none of them. It tries each column that every row holds as a
string or an integer, under `verbatim` (with the alphabet the names use), then
`reldir`, then a template around either. A column matches when the rule
spells every row's value as exactly that row's file name. Among the columns
that match, it prefers `id`, then `<singular table name>_id`, then the first
in column order.

When no column matches, the file name itself becomes the key, as a column
named `id` (or `_file`, `_file_2` and so on, when the body already uses
`id`). It is typed `int` when every name is a canonical integer. Composite keys
are only ever declared. `reldir infer --pk COLUMNS` chooses the key
explicitly.

## Style

`x-reldir.style` records how a table's files are written, so a row that
reldir writes looks like the rows around it:

```json
{
  "style": {
    "indent": "  ", "colon": ": ", "comma": ", ", "newline": "\n",
    "final_newline": true, "bom": false, "empty": "{}", "padding": false,
    "inline_scalar_arrays": false
  }
}
```

| Member | Values | Meaning |
|---|---|---|
| `indent` | 1 to 16 spaces, 1 to 4 tabs, or `""` | One level of indentation; `""` writes a document on one line. |
| `colon` | `":"`, `": "`, `" : "` | The text between a member's name and its value. |
| `comma` | `","`, `", "` | The separator in a container written on one line. |
| `newline` | `"\n"`, `"\r\n"` | The line break. |
| `final_newline` | boolean | Whether the file ends with a line break. |
| `bom` | boolean | Whether the file begins with a UTF-8 byte order mark. |
| `empty` | `"{}"`, `"{ }"` | How an empty object or array is written. |
| `padding` | boolean | Whether a container on one line has spaces inside its brackets. |
| `inline_scalar_arrays` | boolean | Whether an indented file writes an array of plain values on one line. |

Every member is required. Inference records the style most of a table's files
share. A table without `style` is written with the values above. An edit to an
existing file keeps that file's own style.

A row file that is not in its table's recorded style is reported by `reldir
lint` as `LINT_NON_CANONICAL_FORMATTING`, and `FIX_CANONICALIZE` rewrites it.
The row's values do not change.

## References

A foreign key says that values found in a row name rows that exist:

```json
{
  "foreignKeys": [
    { "from": ["team_id"], "to": { "table": "teams" }, "onDelete": "set_null" },
    { "from": ["reviewer_ids[]"], "to": { "table": "users" }, "onDelete": "remove" },
    { "from": ["modules[].lessons[].lesson_ref"], "to": { "table": "lessons" } },
    { "name": "prerequisites",
      "from": ["relations[?relation='requires'].target"],
      "to": { "domain": "content" },
      "onDelete": "restrict" }
  ]
}
```

| Member | Meaning |
|---|---|
| `from` | One path per key column: where in the row the referencing values are. |
| `to` | `{"table": t}`, `{"tables": [t, ...]}` or `{"domain": d}`. |
| `columns` | The target columns to compare with. The default is the target's key. |
| `onDelete` | What happens to the reference when its target row is deleted. |
| `onUpdate` | What happens when its target row's key changes. |
| `name` | The constraint's name. The default is `fk_` and the paths, as in `fk_team_id` or `fk_reviewer_ids`. |

A reference to a row that does not exist is `FOREIGN_KEY_VIOLATION`. Names of
foreign keys, checks, acyclic graphs and assertions share one namespace per
table (`SCHEMA_CONSTRAINT_NAME_DUPLICATE`).

### Paths

A path names places inside a row. It always ends at a scalar. A null or absent
value along the way makes no reference, just as a null column makes none.

```text
path       = segment *( "." segment )
segment    = name *( "[]" / filter )
filter     = "[?" name "=" literal "]"
name       = identifier / quoted
identifier = ( ALPHA / "_" ) *( ALPHA / DIGIT / "_" )
quoted     = DQUOTE *json-char DQUOTE          ; a JSON string
literal    = "'" *( %x00-26 / %x28-10FFFF / "''" ) "'"
           / number / "true" / "false" / "null"
```

- `team_id` is a column.
- `reviewer_ids[]` is every element of an array column.
- `modules[].lessons[].lesson_ref` is a member of objects nested in arrays.
- `relations[?relation='requires'].target` takes only the elements whose
  `relation` is `requires`, so one list of edges can hold references of
  several kinds.

A path is checked when the schema loads. It must parse and reach a scalar
through arrays and objects the schema declares
(`SCHEMA_REFERENCE_PATH_INVALID`), and that scalar must be able to equal the
target's key (`SCHEMA_FK_TYPE_MISMATCH`). A composite reference has one path
per key column, and none of its paths may cross an array.

### Targets

- `{"table": "teams"}` names a row of one table, by its key or by the unique
  constraint that `columns` names. Columns that are no key of the target are
  `SCHEMA_FK_TARGET_NOT_UNIQUE`.
- `{"tables": ["objectives", "knowledge"]}` names a row of any of two or more
  tables. Their keys must be of one comparable type, and no key may be held in
  two of them (`SCHEMA_TARGET_KEYS_OVERLAP`), so a reference names exactly one
  row.
- `{"domain": "content"}` names a row of any table in an identity domain.

A target table or domain without a schema is `SCHEMA_FK_TARGET_MISSING`.

### Identity domains

Tables that share `"identityDomain": "content"` share one key namespace. No key
may be held by rows of two member tables (`DOMAIN_KEY_VIOLATION`), so a
reference into the domain names exactly one row, whatever table it is in. The
member tables key on one column, all of one comparable type
(`SCHEMA_DOMAIN_KEY_INVALID`).

For example, a corpus whose keys are unique across kinds, such as
`subject.calculus`, `objective.limits` and `item.q17`, puts every table in one
domain, and a reference whose target may be of any kind targets the domain.

### Referential actions

When a row is deleted or its key changes, every reference that names it is
resolved by its foreign key's action, in the same transaction:

| Action | On delete | On key change |
|---|---|---|
| `restrict` | Refuse at once, naming every reference in the way. | Refuse. |
| `no_action` | Refuse only if the reference still names nothing when the whole change is done. | The same. |
| `cascade` | Delete the referencing row. | Write the new key into the reference. |
| `remove` | Remove the array element that holds the reference; with no array, remove the member. | The same. |
| `set_null` | Set the referencing value to null. | The same. |
| `set_default` | Set it to its column's default. | The same. |

An action that is left out is `restrict`. `reldir lint` reports a foreign key that leaves an action out
(`LINT_FK_ACTION_DEFAULTED`).

An action may only write what the schema admits. This is checked when the
schema loads (`SCHEMA_FK_ACTION_INVALID`):

- `set_null` needs a reference that admits null.
- `set_default` needs a reference that declares a default.
- `remove` on a path that crosses no array needs a member that rows may leave
  out.
- `remove` never takes an element out of an array typed by position
  (`prefixItems`), where every later element would move into a place that
  means something else.

Actions apply to every change reldir makes: SQL, row commands, imports and
repairs. They are computed completely before anything is written. A cascade
deletes a row, which may vacate keys of its own, and those are resolved in
turn. Every row an action touches is reported:

```console
$ reldir delete tags history --dry-run
remove posts/p1.json (posts.tags[] -> tags of ["history"] in tags/history.json)
action | path              | planned
-------+-------------------+--------
delete | tags/history.json | true
write  | posts/p1.json     | true
(2 rows)
dry run: 2 file(s) would change; nothing was written
$ reldir delete users ada --dry-run
error[FOREIGN_KEY_VIOLATION]: deleting users/ada.json refused: posts/p1.json still references it at /user_id (posts.user_id -> users, onDelete restrict)
  --> posts/p1.json:3:14
   |
 3 |   "user_id": "ada",
   |              ^
   = at: /user_id
   = constraint: posts.user_id -> users
   = observed: ["ada"]
   = help: change or remove the references first, or declare an action that resolves them
```

Actions apply to the changes reldir makes. A file deleted outside reldir is
not cascaded: a reference to the missing row is a `FOREIGN_KEY_VIOLATION`,
which `reldir doctor` repairs (see
[validation]({{ site.baseurl }}/validation#doctor)).

## Acyclic graphs

```json
{ "acyclic": [ { "name": "prerequisites", "edges": ["prerequisite_refs[]"] } ] }
```

Each edge is a path from a row to keys of the same table. The edges must never
form a cycle. A cycle is `CYCLE_VIOLATION`, and the message spells it out:
`prerequisites must be acyclic, but ["a"] -> ["b"] -> ["a"] forms a cycle`. A graph is drawn
between single-column keys, and an edge must reach a value of the table's key
type (`SCHEMA_ACYCLIC_INVALID`).

## Checks

A check is one SQL condition over one row's columns, in SQLite's dialect:

```json
{ "checks": [
  { "name": "dates_ordered", "expr": "start_date <= end_date" },
  { "name": "has_tags", "expr": "json_array_length(tags) > 0" }
] }
```

A row for which the condition is false is `CHECK_VIOLATION`. A null result
passes, as with SQL's `CHECK`. reldir parses the expression and renders it
itself, so no text in it can close the expression and extend the statement
around it.

A check may read only its own table, and may call only functions whose result
depends on the rows alone:

- core scalar functions, such as `length`, `lower`, `substr`, `coalesce`,
  `iif` and `printf`;
- mathematical functions, such as `abs`, `round`, `floor` and `pow`;
- the JSON functions, `json_each` and `json_tree` included;
- the date and time functions, at a stated instant;
- reldir's own `dec_add`, `dec_sub`, `dec_mul`, `dec_cmp`, `dec_sum` and
  `nfc`.

`random()`, `CURRENT_TIMESTAMP`, `date('now')`, a date function given no
instant, and an instant computed from constants (`'n' || 'ow'`) are refused,
so validity never depends on the time or on chance. A check that does
not parse, is more than one expression, reads another table or calls another
function is `SCHEMA_CHECK_INVALID`.

## Assertions

An assertion is a rule over many rows. Its `query` is one `SELECT` that returns
the key of every row that breaks the rule. It may read any table.

```json
{ "assertions": [
  { "name": "assessed",
    "query": "SELECT id FROM objectives o WHERE NOT EXISTS (SELECT 1 FROM items i, json_each(i.objective_refs) r WHERE r.value = o.id)",
    "severity": "warning",
    "message": "no item assesses this objective" }
] }
```

Each row returned is `ASSERTION_VIOLATION`, located at that row's file, with
`message` when one is given. With `"severity": "error"`, the default, it makes
the database invalid and refuses any change that would introduce it. With
`"warning"` it is reported and nothing more. Assertions call the same
functions as checks; one that breaks these rules is `SCHEMA_ASSERTION_INVALID`.

## Contracts

A table's schema is its contract with the programs that read its files. Why
reldir keeps it, and asks before widening it, is explained in
[Concepts]({{ site.baseurl }}/concepts#the-observed-contract).

An **inferred contract**, in `.db/schema/`, states what every file held when
reldir inferred it:

- a member is in `required` when every file holds it;
- `"null"` is among a member's types only where a file held null;
- an object has `"additionalProperties": false` and declares exactly the
  members its files held;
- a member every file wrote as an integer is `int`, one every file wrote with a
  fraction or an exponent is `float`, and one written both ways is `number`;
- a string every file held as a timestamp, date, UUID or ULID has that type;
- `identity` and `style` record the files' naming and formatting;
- a reference is declared where a member's name matches `reference_naming`
  (see [configuration]({{ site.baseurl }}/configuration#reference-naming)) and
  every value is a key of the named table, with `restrict` actions.

It states no `enum`, `unique`, bounds, `minItems` or checks; `reldir lint`
proposes those, and `reldir infer --tighten` states them.

| Who writes a value outside the contract | Inferred contract | Pinned schema |
|---|---|---|
| reldir: a statement, a row command, an import, a referential action, a repair | refused: `CONTRACT_WIDENING_REQUIRED` | refused: the rule's own code |
| another program, editing a file | `CONTRACT_EXCEEDED`, a warning; the database stays valid | the rule's own code, an error |

`CONTRACT_WIDENING_REQUIRED` names the value, the rule, and the smallest
widening that admits it. A file that holds a value beyond its contract is left
as it is.

A widening edits the schema where it lives and is recorded in history:

| Widening | Changes |
|---|---|
| `--widen` on a change | whatever the change's values need, in the change's revision |
| `reldir schema widen T` | declares what the table's files hold beyond the contract |
| `reldir schema widen T.member --null` | adds `"null"` to the member's `type` |
| `reldir schema widen T.member --optional` | removes the member from `required` |
| `reldir schema widen T.member --type T` | adds the JSON type to `type`, and drops an `x-reldir-type` tag that does not refine it |
| `reldir schema widen T.member --any` | replaces the member's subschema with `{}` |
| `reldir doctor --fix --only FIX_WIDEN_CONTRACT` | declares what files written outside reldir hold |

A member is named with the [path syntax](#paths) of references, such as
`users.nick` or `posts.tags[]`. A member whose subschema states no `type`
widens only with `--any`. A widening the contract already admits writes
nothing. `--widen` adds no column: a member the table does not declare is
`UNKNOWN_COLUMN` (see [migrations]({{ site.baseurl }}/migrations)).

If an inferred contract's file is missing, reldir restores it byte for byte
from recorded history and reports `CONTRACT_REDERIVED`.

`reldir schema pin T` moves an inferred contract to `schema/T.json`, where it
is a pin. `reldir lint` reports each unpinned table (`LINT_SCHEMA_UNPINNED`),
and `FIX_PIN_SCHEMA` pins it. Removing a pin and running
`reldir infer T --write` returns the table to an inferred contract.

How to widen, case by case:
[How to write a value a table's contract does not admit]({{ site.baseurl }}/widen-a-contract).

## Managing schemas

| Command | What it does |
|---|---|
| `reldir schema show T` | Print a table's schema. |
| `reldir schema new T` | Declare a new table as a pin, with a string key `id`. |
| `reldir schema pin T` | Make an inferred contract a pin. |
| `reldir schema widen T[.member]` | Widen a contract. |
| `reldir schema validate` | Report every fault in the schemas, without judging rows. |
| `reldir schema dialect` | Print the dialect, for editors. |
| `reldir infer [T...] --write` | Infer schemas from rows and keep them. |
| `reldir infer T --on-schema-conflict compare` | Show how inference differs from an existing schema. |
| `reldir infer T --pk COLUMNS` | Infer with the key named, from columns whose values already name the table's files. |

`reldir infer --tighten` also states facts the rows only happen to share, as
constraints: `enums`, `unique`, `checks`, `min-items`, or `all`. When a table
already has a schema, `--on-schema-conflict` decides what happens: `ask`,
`fail` (`SCHEMA_CONFLICT`), `compare` or `reinfer`.

Structural changes are migrations: adding, renaming and dropping tables and
columns, changing a type or a key, and adding and dropping constraints,
indexes and domains. See [migrations]({{ site.baseurl }}/migrations).

The schema errors, with their exit statuses, are listed in the
[error reference]({{ site.baseurl }}/errors).
