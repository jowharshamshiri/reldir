---
title: Schemas
---

# Schemas

A table's schema is one JSON Schema 2020-12 document. Every row of the table is
validated against it, exactly as any 2020-12 validator would, and the
relational facts JSON Schema has no keyword for -- identity, uniqueness,
references, rules over sets of rows -- are stated in the same document under
`x-reldir`.

## Where a schema lives

| File | What it is |
|---|---|
| `schema/<table>.json` | A **pin**: a declaration you keep with your data and in version control. A pinned table's schema *is* its pin; reldir reads it where it lies. |
| `.db/schema/<table>.json` | A **working schema**: one reldir inferred for a table nobody pinned. It is rebuilt from the rows if `.db/` is deleted. |

A change reldir makes to a pinned table's schema -- a migration, an applied
fix -- is written to the pin. `reldir schema pin <table>` moves a working schema
to `schema/`. A working schema left beside a pin for the same table is a stale
copy; it governs nothing, and the next writing command removes it.

Table names match `^[a-z][a-z0-9_]*$`, are never `schema`, and never a Windows
device name (`con`, `nul`, `com1`, ...).

## The dialect

Every table schema says `"$schema": "https://reldir.dev/schema/reldir-2"`. The
URI names the dialect; nothing is fetched from it. The dialect is compiled into
the binary, and `reldir schema dialect` prints it so an editor can validate and
complete schemas as you write them:

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

### Every standard keyword, with its standard meaning

All of JSON Schema 2020-12 is available and enforced on every row:
`type`, `enum`, `const`, the string, number, array and object bounds,
`pattern`, `properties`, `patternProperties`, `additionalProperties`,
`propertyNames`, `required`, `dependentRequired`, `dependentSchemas`, `items`,
`prefixItems`, `contains`, `unevaluatedProperties`, `unevaluatedItems`,
`allOf`, `anyOf`, `oneOf`, `not`, `if`/`then`/`else`, and local `$ref` to
`$defs`.

`format` **asserts**. The dialect enables the format-assertion vocabulary, so
`date`, `date-time`, `uuid`, `email`, `uri`, `ipv4` and the other standard
formats reject a value that does not have the form.

A `$ref` must stay inside the document (`#/$defs/...`). A reference to another
file or a URL is `SCHEMA_REF_EXTERNAL`: a table's validity never depends on a
file the database does not govern.

### A closed dialect

JSON Schema ignores keywords it does not know, so a typo such as `minLenght`
silently constrains nothing. reldir refuses it, at any depth, and names the
keyword you probably meant:

```text
error[SCHEMA_UNKNOWN_KEY]: "minLenght" is not a JSON Schema or reldir keyword (did you mean "minLength"?)
  --> schema/users.json:7:25
```

Your own annotations are welcome under any name beginning `x-` (other than
`x-reldir...`): `"x-ui-widget": "textarea"` is kept and ignored.

### Annotations

`title`, `description`, `$comment` and `examples` describe a schema without
constraining anything. They take no part in the schema's identity, so editing
a description does not change the database's state hash.

## The root

A table schema's root is an object schema with these members:

| Member | Required | Meaning |
|---|---|---|
| `$schema` | yes | the dialect URI |
| `type` | yes | `"object"` |
| `properties` | yes | the columns, **in the order rows are written** |
| `required` | | columns a row may not omit; the primary key must be listed |
| `additionalProperties` | yes | `false` refuses members the schema does not declare (`ROW_UNKNOWN_FIELD`); `true` keeps them |
| `x-reldir` | yes | the relational facts below |

`additionalProperties` is required because there is no safe default: a
database that silently kept unknown members would accept typos, and one that
silently refused them would reject data another tool wrote. Say which you mean.

Any other standard keyword may appear at the root and applies to the whole
row -- `if`/`then`/`else` for conditional requirements, `dependentRequired`,
`allOf` of several rules:

```json
{
  "if": { "properties": { "mode": { "const": "blueprint" } }, "required": ["mode"] },
  "then": { "required": ["blueprint"] }
}
```

## Column types

Each top-level property is a column with one relational type, read from its
subschema. The type decides how the value is compared, sorted, keyed and
exposed to SQL.

| Type | Subschema |
|---|---|
| `bool` | `{"type": "boolean"}` |
| `int` | `{"type": "integer", "x-reldir-type": "int"}` |
| `float` | `{"type": "number"}` |
| `decimal` | `{"type": "string", "x-reldir-type": "decimal", "pattern": ...}` |
| `string` | `{"type": "string"}` |
| `enum` | `{"type": "string", "enum": [...]}` |
| `bytes` | `{"type": "string", "contentEncoding": "base64"}` |
| `date` | `{"type": "string", "format": "date"}` |
| `timestamp` | `{"type": "string", "format": "date-time"}` |
| `uuid` | `{"type": "string", "format": "uuid"}` |
| `ulid` | `{"type": "string", "x-reldir-type": "ulid", "pattern": ...}` |
| `array` | `{"type": "array", ...}` |
| `object` | `{"type": "object", ...}` |
| `json` | anything else, including `{}` |

A nullable column adds `"null"` to its `type`: `{"type": ["string", "null"]}`.

`x-reldir-type` marks the three types a standard keyword cannot tell apart, and
reldir enforces their lexical form on top of the schema:

- `int` is written without a fraction or exponent and fits in 64 bits: `1` but
  never `1.0` or `1e0`;
- `decimal` is a string in canonical form -- no leading zeros, no trailing
  fractional zeros, no `+`, no `-0` -- compared numerically and exactly;
- `ulid` is 26 uppercase Crockford base32 characters.

A `uuid` must be lowercase. A `timestamp` must carry an offset; it is compared
as an instant, so `10:00:00Z` and `12:00:00+02:00` are the same key.

### Presence, null and defaults

These are separate questions. `required` decides whether a row may omit a
member; a `null` in `type` decides whether the value may be null; `default`
says what an omitted member reads as. An omitted member stays omitted in the
file -- reldir never writes a value you did not give -- and reads as its
default in SQL, in keys and in the state hash.

## `x-reldir`

| Key | Meaning |
|---|---|
| `table` | the table; must equal the file name |
| `primaryKey` | columns identifying a row; scalar, required, not nullable |
| `unique` | lists of columns no two rows may share |
| `indexes` | lists of columns to index for faster queries |
| `filename` | columns a row's file name is built from; the primary key when absent |
| `generated` | column → `uuid`, `ulid`, `now` or `sequence`, filled when an insert gives none |
| `identityDomain` | a namespace this table's keys share with other tables |
| `foreignKeys` | references from this table's rows to rows anywhere |
| `acyclic` | reference graphs that must never contain a cycle |
| `checks` | boolean SQL each row must satisfy |
| `assertions` | SQL naming rows that break a rule over many rows |
| `schemaVersion` | a number you manage; reldir only records it |

### Identity and file names

A row's identity is its primary key, and its file name is derived from its
`filename` columns (the primary key by default): each value rendered
canonically, percent-encoded outside `[A-Za-z0-9._-]`, joined with `,`, with
`.json` appended. A leading `.` is encoded, so a row is never a hidden file,
and `/` is always encoded, so a row never leaves its table.

A file whose name is not the one its key gives is `IDENTITY_MISMATCH`; doctor
renames it. A key whose file name would exceed 255 bytes -- the limit every
common filesystem shares -- is `FILENAME_TOO_LONG`, refused before anything is
written.

A declared `filename` must be the primary key or a unique constraint over
columns that are never null, or two rows could claim one file
(`SCHEMA_FILENAME_NOT_UNIQUE`).

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
| `from` | one path per key column: where in the row the referencing values are |
| `to` | `{"table": t}`, `{"tables": [t, ...]}`, or `{"domain": d}` |
| `columns` | the target columns compared with; the target's primary key when absent |
| `onDelete` | what happens to the reference when its target row is deleted |
| `onUpdate` | what happens when its target row's key changes |
| `name` | the constraint's name; derived from the paths when absent |

### Paths

A path names places inside a row. It always ends at a scalar, and null or
absent values along the way make no reference, exactly as a null column does
not.

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

- `team_id` -- a column;
- `reviewer_ids[]` -- every element of an array column;
- `modules[].lessons[].lesson_ref` -- a member of objects nested in arrays;
- `relations[?relation='requires'].target` -- only the elements whose
  `relation` is `requires`, so one edge list can hold references of several
  kinds.

A path is checked when the schema loads: it must reach a scalar whose type can
equal the target key's, through arrays and objects the schema declares
(`SCHEMA_REFERENCE_PATH_INVALID`, `SCHEMA_FK_TYPE_MISMATCH`). A composite key
uses one path per column, none of which may cross an array.

### Targets

- `{"table": "teams"}` -- rows of one table, compared with its primary key or
  the unique constraint `columns` names.
- `{"tables": ["objectives", "knowledge"]}` -- a row of any of these tables.
- `{"domain": "content"}` -- a row of any table in an identity domain.

### Identity domains

Tables that share `"identityDomain": "content"` share one key namespace: no
key may be held by rows of two member tables (`DOMAIN_KEY_VIOLATION`), so a
reference into the domain names exactly one row, whatever table it is in.
Member tables key on one column, all of one type (`SCHEMA_DOMAIN_KEY_INVALID`).

This is how a corpus whose ids are globally unique -- `subject.calculus`,
`objective.limits`, `item.q17` -- is modelled: every table joins the domain, and
references whose target may be of any kind point at the domain.

### Referential actions

When a row is deleted, or its key changes, every reference naming it is
resolved by its foreign key's action, in the same transaction:

| Action | On delete | On key change |
|---|---|---|
| `restrict` | refuse, naming every reference in the way | refuse |
| `no_action` | the same as `restrict` | the same as `restrict` |
| `cascade` | delete the referencing row | write the new key into the reference |
| `remove` | remove the array element holding the reference; with no array, set the value to null | the same |
| `set_null` | set the referencing value to null | the same |
| `set_default` | set it to its column's default | the same |

Actions left out are `restrict`: nothing is lost unless the schema says so.
`remove` on a path that crosses no array needs a nullable column, and
`set_null` needs a nullable one (`SCHEMA_FK_ACTION_INVALID`).

Actions apply to every change reldir makes -- SQL, row commands, imports,
repairs -- and are computed completely before anything is written: a cascade
deletes a row, which may vacate keys of its own, which are resolved in turn.
Every row an action touched is reported:

```console
$ reldir delete subjects subject.calculus --dry-run
referential_action remove courses/course.intro.json (fk_subject_refs of ["subject.calculus"] in subjects/subject.calculus.json)
dry run: 2 file(s) would change; nothing was written
```

Actions describe what reldir does when *it* makes a change. A file you delete
by hand is not cascaded after the fact: the directory is judged as it stands,
and a reference to the missing row is a `FOREIGN_KEY_VIOLATION` that `reldir
doctor` repairs -- first by restoring the row from recorded history.

### Indexes

Every reference is indexed automatically; `indexes` is only for queries.

## Acyclic graphs

```json
{ "acyclic": [ { "name": "prerequisites", "edges": ["prerequisite_refs[]"] } ] }
```

The edges -- paths from a row to keys of the same table -- must never form a
cycle. A cycle is `CYCLE_VIOLATION`, and the message spells it:
`prerequisites must be acyclic, but ["a"] -> ["b"] -> ["a"] forms a cycle`.

## Checks

A check is a boolean SQL expression over one row's columns, in SQLite's
dialect with its functions:

```json
{ "checks": [
  { "name": "dates_ordered", "expr": "start_date <= end_date" },
  { "name": "has_tags", "expr": "json_array_length(tags) > 0" }
] }
```

A row for which the expression is false is `CHECK_VIOLATION`; null passes, as
in SQL. A check must be deterministic -- validity cannot depend on the clock or
on chance -- so `random()`, `CURRENT_TIMESTAMP`, `date('now')` and their kin are
refused (`SCHEMA_CHECK_INVALID`), as is anything that reads another table.

## Assertions

An assertion is a rule over many rows. Its `query` is a `SELECT` returning the
primary key of every row that breaks the rule; it may read any table.

```json
{ "assertions": [
  { "name": "reachable",
    "query": "SELECT id FROM objectives o WHERE NOT EXISTS (SELECT 1 FROM items i, json_each(i.objective_refs) r WHERE r.value = o.id)",
    "severity": "warning",
    "message": "no item assesses this objective" }
] }
```

Each row returned is `ASSERTION_VIOLATION`, located at that row's file. With
`"severity": "error"` (the default) it makes the database invalid and refuses
any change that would introduce it; with `"warning"` it is reported and
nothing more. Assertions are deterministic for the same reason checks are.

## Identity of a schema

A schema's identity is the SHA-256 of its RFC 8785 canonical form with
`title`, `description`, `$comment` and `examples` removed. Two documents that
state the same rules have the same identity however they are formatted or
ordered.

Column order is not part of identity -- it decides how a row is written, not
whether it is valid -- but it is part of the state the mirror is built from,
so reordering `properties` rewrites nothing and changes no hash.

## Managing schemas

```sh
reldir schema show users          # the document
reldir schema new tags            # declare a new table as a pin
reldir schema pin users           # make an inferred schema a pin
reldir schema validate            # every schema fault, without judging rows
reldir infer users --on-schema-conflict compare   # how inference now differs
reldir migrate add-column users bio --type string --nullable
```

Structural changes -- adding, renaming and dropping tables and columns, changing
a type, adding and dropping constraints and domains -- are migrations. See
[migrate]({{ site.baseurl }}/cli#migrate).
