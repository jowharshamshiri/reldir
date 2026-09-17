---
title: Schemas
---

# Schemas

Every table has a working schema at `.db/schema/<table>.json`, which the binary
maintains and every command reads. The runtime never operates on an implicit
schema: query, mutation, and validation always run against a file that physically
exists.

Pinning copies that schema to `schema/<table>.json`, making it a declaration you
own: kept in version control, surviving `rm -rf .db`, and never inferred over.
Where both exist they agree. If you edit the pin, it wins and the working copy is
rebuilt from it.

Table names must match `^[a-z][a-z0-9_]*$`, must not be `schema`, and must avoid
Windows reserved device names.

## The schema language

A schema is a [JSON Schema 2020-12](https://json-schema.org/) document in reldir's
own dialect, identified by `"$schema": "https://reldir.dev/schema/reldir-1"`. Ordinary
JSON Schema tooling can read it.

That URI names the dialect; it is not fetched, and nothing is served there yet.
The dialect itself is compiled into the binary, so to get editor completion,
write it out and point your editor at the local copy:

```sh
reldir schema dialect > .reldir-dialect.json
```

In VS Code, for example, associate it with your schema files:

```json
{
  "json.schemas": [
    { "fileMatch": ["schema/*.json", ".db/schema/*.json"],
      "url": "./.reldir-dialect.json" }
  ]
}
```

reldir does not accept arbitrary JSON Schema. A document that says `oneOf`
requests a semantics reldir has no relational meaning for, and ignoring it would let
the file and the database disagree about which rows are valid. The dialect
therefore declares a `$vocabulary` of its own, marked required, so a conforming
reader learns that it cannot fully process the document without understanding
reldir's keywords.

Valid JSON Schema content that does not alter reldir's semantics is preserved.
Content that would alter semantics reldir cannot represent is rejected by name with
`SCHEMA_UNSUPPORTED_KEYWORD`.

### What a document says

Standard keywords describe the columns: `type`, `properties`, `items`, `enum`,
`const`, `default`, `required`, `additionalProperties`, `format`, `pattern`,
`contentEncoding`, and `description`; the bounds `minLength`, `maxLength`,
`minItems`, `maxItems`, `minProperties`, `maxProperties`, `minimum`, `maximum`,
`exclusiveMinimum`, `exclusiveMaximum`, `multipleOf`, and `uniqueItems`; and the
composition keywords `oneOf`, `anyOf`, `allOf`, and `not`.

Everything relational that JSON Schema has no keyword for lives under one
extension key, `x-reldir`:

| Key | Meaning |
|---|---|
| `table` | the relation's identity; equals the file stem |
| `primaryKey` | row identity, file naming, foreign-key targets |
| `columnOrder` | the order columns were declared in |
| `schemaVersion` | user-managed; default 1 |
| `schemaFormat` | pins the format version explicitly |
| `unique` | arrays of column-name arrays |
| `indexes` | arrays of column-name arrays |
| `foreignKeys` | `{ columns, references: { table, columns }, onDelete, onUpdate }` |
| `checks` | `{ name, expr }`, where `expr` is a boolean SQL expression |
| `generated` | column name to `uuid`, `ulid`, `now`, or `sequence` |
| `filename` | the columns a row's filename is built from |

`columnOrder` is required because rows are written in schema column order, so
two schemas differing only in that order write different bytes for the same
logical row. JSON object members are unordered, so the order has to be stated
explicitly.

A foreign key is deliberately **not** a `$ref`. In JSON Schema, `$ref` means
"apply this schema to the value here", which is composition and reuse. It cannot
say that a value names a row that must exist in another table, and it has nowhere
to put a composite column list or a referential action. Spelling foreign keys
with `$ref` would make the document look standard while making its meaning
false.

### References from inside an array

A column holding an array of ids is many edges from one row, and reldir enforces
it. Suffix the column name with `[]` and each element must name a row that
exists:

```json
{
  "foreignKeys": [
    {
      "columns": ["objective_refs[]"],
      "references": { "table": "objectives", "columns": ["id"] },
      "onDelete": "restrict"
    }
  ]
}
```

Each element is looked up on its own, so one row with four references performs
four lookups and a missing target is reported per element. A null element makes
no reference and is not an orphan, exactly as a null column is not.

An element key names exactly one column: a tuple drawn from two arrays has no
defined pairing. Its element type must match the target's, not the array's —
`objective_refs` is an array of `string`, and the target `id` is a `string`.
`set_null` and `set_default` are refused for an element key, because removing an
element and nulling one are different operations and neither action says which
was meant.

Where a relationship carries its own attributes — a weight, an order, a
rationale — give the edges their own table with a composite primary key and a
foreign key on each side. That form holds data about the edge; an array holds
only the fact of it.

The alternative is still a query, if you would rather inspect than enforce:

The recommended form is to keep the array and check it as a query, which
`json_each` expands one element per row:

```sql
SELECT b.id, e.value
FROM blocks b, json_each(b.objective_refs) e
WHERE e.value NOT IN (SELECT id FROM objectives);
```

An empty result means every element resolves. Run it in `reldir check`'s company
rather than in place of it: the schema still governs the array's element type,
and this governs what the elements point at.

Where the relationship deserves enforcement rather than inspection, give the
edges their own table with a composite primary key of the two sides, and declare
a foreign key on each. That is the form reldir enforces transactionally, including
referential actions.

### Nullability and presence

These are different questions, and JSON Schema already distinguishes them.

A **nullable** column admits null as a value, written as a type union:
`"type": ["string", "null"]`.

A **required** column is one a row cannot omit. A row may leave out a column that
has a default or a generator, because the value can still be supplied; it may
leave out a nullable column, because absence reads as null. Everything else is
listed in `required`, and omitting it is `ROW_MISSING_FIELD`.

A `NOT NULL` column with a default is therefore not required: it admits no
null, but a row need not carry it.

### Types

Standard keywords carry the type where they can. Where they cannot distinguish
two reldir types, the subschema is tagged with `x-reldir-type`: reldir's `int` is lexical
where JSON Schema's `integer` is mathematical, and `decimal` and `ulid` are
strings with application semantics. The tag sits on the subschema it describes,
so it works at any depth. An `array` of `decimal` has no column name by which a
document-level map could key it.

| reldir type | JSON Schema |
|---|---|
| `bool` | `{"type": "boolean"}` |
| `int` | `{"type": "integer", "x-reldir-type": "int"}` |
| `float` | `{"type": "number"}` |
| `decimal` | `{"type": "string", "pattern": …, "x-reldir-type": "decimal"}` |
| `string` | `{"type": "string"}` |
| `bytes` | `{"type": "string", "contentEncoding": "base64"}` |
| `date` | `{"type": "string", "format": "date"}` |
| `timestamp` | `{"type": "string", "format": "date-time"}` |
| `uuid` | `{"type": "string", "format": "uuid"}` |
| `ulid` | `{"type": "string", "pattern": …, "x-reldir-type": "ulid"}` |
| `enum` | `{"type": "string", "enum": [...]}`, or `const` for a single value |
| `array` | `{"type": "array", "items": {...}}` |
| `object` | `{"type": "object", "properties": {...}}` |
| `json` | `{}` |

In 2020-12 `format` is an annotation unless the format-assertion vocabulary is
enabled, which this dialect does not enable. A generic validator therefore
understands the structure of a reldir schema and checks its shape; reldir remains the
authority on what a `uuid`, `ulid`, `decimal`, or `timestamp` actually admits.

### Patterns

`pattern` constrains a string value, and reldir enforces it. It applies wherever a
string appears — a column, an array's elements, a nested property:

```json
{
  "slug":     { "type": "string", "pattern": "^[a-z][a-z0-9-]{2,63}$" },
  "refs":     { "type": "array", "items": { "type": "string", "pattern": "^obj-[0-9]+$" } }
}
```

A value that satisfies the type but not the pattern is `TYPE_MISMATCH`, and the
message names the pattern it missed rather than the type it already has.

Patterns are matched with Rust's `regex`, which is ECMA-262 syntax without
backreferences or lookaround, and which matches in time linear in the subject.
A pattern reldir cannot compile is refused when the schema is validated, with
`SCHEMA_CHECK_INVALID` naming the column — never accepted and then quietly
unenforced.

An unanchored pattern matches anywhere in the value, as it does in JSON Schema.
Anchor with `^` and `$` to constrain the whole string.

A pattern decides which rows a schema admits, so it is part of what that schema
*is*: adding or changing one changes the schema's hash, as changing a type does.

`decimal` and `ulid` are the exception, and only because they are written with a
`pattern` to begin with: that is how JSON Schema spells what those types admit.
reldir reads that one back as the type restating itself rather than as a further
constraint, so a `decimal` column has the same identity before and after its
schema makes a round trip through disk.

### Bounds

A bound says how large a value may be, or where a number may fall. reldir
enforces them, and each is written under the name JSON Schema uses for that
shape:

```json
{
  "title":  { "type": "string", "minLength": 1, "maxLength": 200 },
  "tags":   { "type": "array", "items": { "type": "string" }, "minItems": 1, "uniqueItems": true },
  "meta":   { "type": "object", "properties": {}, "minProperties": 1 },
  "year":   { "type": "integer", "x-reldir-type": "int", "minimum": 1000, "maximum": 3000 },
  "ratio":  { "type": "number", "exclusiveMinimum": 0, "exclusiveMaximum": 1 },
  "amount": { "type": "number", "multipleOf": 0.01 }
}
```

A string's length counts characters, not bytes, so a bound means the same thing
in every script.

`uniqueItems` compares elements by reldir's canonical rendering — the same
rendering that decides a row's hash. It normalizes strings to NFC and collapses
the sign on zero, but it keeps a number's spelling, so `1` and `1.0` are two
elements here where JSON Schema's own equality counts them as one. The
divergence is deliberate: adopting JSON Schema's numeric equality would mean
either a second notion of sameness used by this keyword alone, or changing what
canonical form says two values are — and canonical form decides row identity for
every database reldir governs.

A bound stated for a type it cannot describe is refused with
`SCHEMA_UNKNOWN_KEY`: `minLength` on a boolean constrains nothing. A bound that
contradicts itself — a minimum above its maximum, a `multipleOf` of zero — is
`SCHEMA_BOUND_INVALID`.

### Composition

`oneOf`, `anyOf`, `allOf`, and `not` narrow which values of a column's declared
type are legal. They do not decide the type: a column has exactly one, because
row ordering, SQL binding, and coercion all depend on knowing it.

```json
{
  "code": {
    "type": "string",
    "oneOf": [
      { "type": "string", "pattern": "^[A-Z]{3}$" },
      { "type": "string", "pattern": "^[0-9]{6}$" }
    ]
  }
}
```

A column declares at most one composition keyword; two would be two constraints
wearing one name, and the second would be invisible. `not` takes a single
subschema rather than a list. Anything else is `SCHEMA_COMPOSITION_INVALID`.

An alternative often says nothing but which member a value must carry:

```json
{
  "trigger": {
    "type": "object",
    "properties": { "on_choice": { "type": ["integer", "null"], "x-reldir-type": "int" },
                    "on_error":  { "type": ["boolean", "null"] } },
    "oneOf": [ { "type": "object", "required": ["on_choice"] },
               { "type": "object", "required": ["on_error"] } ]
  }
}
```

`required` inside an alternative is a question about the *value* — which members
it must present — and it may name a member the alternative itself does not
declare. The alternative need not state a type at all: JSON Schema writes these
as bare `{ "required": ["on_choice"] }`, and reldir reads that as "an object
carrying this member" rather than as the empty schema. That is different from the `required` list at the document root, which
decides whether a row may omit a column, and different again from a column's
nullability, which decides whether a declared member may be absent. All three
coexist because they answer three different questions.

An alternative is itself a column, so it may carry its own pattern, bounds, or
composition, and is held to every rule a column is held to.

### Closed objects

`additionalProperties: false` on an `object` column rejects members the column
does not declare:

```json
{ "meta": { "type": "object",
            "properties": { "source": { "type": "string" } },
            "additionalProperties": false } }
```

The default is JSON Schema's own: absent means open. At the document root the
same keyword decides whether a *row* may carry undeclared fields, reported per
key as `ROW_UNKNOWN_FIELD`; on a column it decides whether the value matches the
column at all.

### Annotations

`title`, `$comment`, `examples`, `readOnly`, and `deprecated` describe a schema
without constraining a row. They are kept verbatim through a load and a save, and
take no part in validation or in a schema's identity, so a comment cannot change
a database's hash.

## Example

```json
{
  "$schema": "https://reldir.dev/schema/reldir-1",
  "type": "object",
  "properties": {
    "id":      { "type": "string", "format": "uuid" },
    "email":   { "type": "string" },
    "role":    { "type": "string", "enum": ["admin", "member"], "default": "member" },
    "team_id": { "type": ["string", "null"], "format": "uuid" },
    "created": { "type": "string", "format": "date-time" }
  },
  "required": ["id", "email"],
  "additionalProperties": false,
  "x-reldir": {
    "table": "users",
    "schemaVersion": 1,
    "primaryKey": ["id"],
    "columnOrder": ["id", "email", "role", "team_id", "created"],
    "unique": [["email"]],
    "indexes": [["team_id"]],
    "foreignKeys": [
      {
        "columns": ["team_id"],
        "references": { "table": "teams", "columns": ["id"] },
        "onDelete": "set_null",
        "onUpdate": "restrict"
      }
    ],
    "checks": [{ "name": "email_has_at", "expr": "email LIKE '%@%'" }],
    "generated": { "id": "uuid", "created": "now" }
  }
}
```

## Type system

| Type | JSON representation |
|---|---|
| `bool` | boolean |
| `int` | number without fraction or exponent, 64-bit signed |
| `float` | number, IEEE 754 double |
| `decimal` | string in canonical decimal form, arbitrary precision |
| `string` | string, valid Unicode |
| `bytes` | string, standard base64 |
| `date` | string, `YYYY-MM-DD` |
| `timestamp` | string, RFC 3339 with offset; compared and stored canonically in UTC |
| `uuid` | string, lowercase 8-4-4-4-12 |
| `ulid` | string, 26 Crockford base32 uppercase |
| `enum` | string, one of `values` |
| `array` | array, elements validated against `items` |
| `object` | object, validated against `properties` when given |
| `json` | any JSON value; opaque to SQL beyond equality and text extraction |

Validation is strict and reads no coercions: an `int` is accepted in a `float`
column, and nothing else is converted. Everything else is `TYPE_MISMATCH`.
`doctor` may offer lossless coercions as data fixes: `"42"` to `42` is
lossless, while `"01"` to `1` is not, because it does not round-trip.

`int` overflow is an error, never wraparound. Silent lossy coercion never occurs.

## Missing and unknown fields

- A key absent for a nullable column reads as `NULL`, or as the column default if
  one is declared.
- A key absent for a `NOT NULL` column with no default is `ROW_MISSING_FIELD`.
- A key present in the row but absent from the schema is `ROW_UNKNOWN_FIELD`,
  unless the schema sets `"additionalProperties": true`.

Defaults are logical values: two rows that both omit a defaulted column share that
column's value for uniqueness and identity purposes.

## Cross-schema rules

- Every column named in `primaryKey`, `unique`, `indexes`, `foreignKeys.columns`,
  or `filename` must exist.
- A foreign key's target table must have a schema.
- A foreign key's target columns must be that table's primary key or a declared
  unique constraint, in order.
- Referencing and referenced column types must be identical.
- `onDelete: set_null` requires nullable referencing columns; `set_default`
  requires declared defaults.
- Foreign keys must not form a cycle in which every edge is `cascade`.
- `checks.expr` must parse and type-check as a boolean expression over the table's
  columns.

## Referential actions

`restrict`, `cascade`, `set_null`, `set_default`, and `no_action` are supported on
both `onDelete` and `onUpdate`. Binary-performed mutations execute them
transactionally and list every row a cascade touched:

```console
$ reldir delete users u1
changed 2 path(s); revision 2
  posts/p1.json
  users/u1.json
```

A blocked `restrict` is reported as `FOREIGN_KEY_VIOLATION` and changes nothing.

## Custom file naming

`x-reldir.filename` may name non-primary-key columns, a `slug` for example, as
long as those columns are covered by a unique constraint and are `NOT NULL`:

```json
{ "storage": { "filename": ["slug"] } }
```

## Managing schemas

```sh
reldir schema show users        # print a schema
reldir schema new users         # scaffold a minimal valid schema
reldir schema pin users         # declare the working schema in schema/
reldir schema restore users     # rebuild the working schema from its pin
reldir schema validate          # validate every schema
```

Schema changes are first-class database changes. See [migrations]({{ site.baseurl }}/cli#migrate) for
transactional schema evolution, and [validation]({{ site.baseurl }}/validation) for tightening what
inference guessed.
