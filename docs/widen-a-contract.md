---
title: How to write a value a table's contract does not admit
---

# How to write a value a table's contract does not admit

This guide shows how to make a table's contract admit a value it has never
held: when a change is refused with `CONTRACT_WIDENING_REQUIRED`, or when
`reldir check` warns `CONTRACT_EXCEEDED` about a file another program wrote.
Why reldir asks first is explained in
[Concepts]({{ site.baseurl }}/concepts#the-observed-contract).

Before you widen, decide whether the programs that read the table's files can
take the new shape. If they cannot, the value is the mistake: change the value,
not the contract.

## Widen as part of a change

A refusal names the value, the rule it breaks and the widening that would
admit it:

```console
$ reldir sql "UPDATE users SET born = NULL WHERE id = 'grace'"
[..]
error[CONTRACT_WIDENING_REQUIRED]: the change would write what the contract of users has never admitted: "born" cannot be null; nothing was written
  --> users/grace.json:4:11
   |
 4 |   "born": null
   |           ^
   = at: /born
   = constraint: /properties/born/type
   = help: widening would: /properties/born: adds "null" to `type`. Programs reading these files rely on that contract; pass --widen to widen it as part of this change, or widen it first with `reldir schema widen`
```

To accept that widening, run the same change with `--widen`. The schema and
the rows are written in one revision:

```console
$ reldir --widen sql "UPDATE users SET born = NULL WHERE id = 'grace'"
widened the contract of users: /properties/born: adds "null" to `type`
action | path                  | planned
-------+-----------------------+--------
write  | users/grace.json      | false
write  | .db/schema/users.json | false
(2 rows)
committed: 2 file(s), revision #2 ([..])
```

To see the widening and the change without writing either, add `--dry-run`.

## Widen ahead of a change

If you know the shape the table needs before any row holds it, widen the
member with `reldir schema widen TABLE.MEMBER` and say how:

| To let a member | Pass |
|---|---|
| be null | `--null` |
| be left out of a row | `--optional` |
| hold values of another JSON type | `--type T` |
| hold any value | `--any` |

```console
$ reldir schema widen users.born --optional
widened the contract of users: /: makes "born" optional
action | path                  | planned
-------+-----------------------+--------
write  | .db/schema/users.json | false
(1 row)
widened: 1 file(s), revision #3 ([..])
$ reldir insert users '{"id": "alan", "name": "Alan Turing"}'
id   | name
-----+------------
alan | Alan Turing
(1 row)

action | path            | planned
-------+-----------------+--------
write  | users/alan.json | false
(1 row)
inserted: 1 file(s), revision #4 ([..])
```

For a member inside an array, name the elements: `posts.tags[]`. If the
contract already admits the value, `schema widen` says so and writes nothing.

## Admit what another program wrote

A program outside reldir may write a value beyond a table's inferred
contract. reldir records the state and warns:

```console
$ echo '{"id": "kurt", "name": "Kurt Gödel", "born": 1906, "field": "logic"}' > users/kurt.json
$ reldir check
recorded revision #5 ([..]) (external): 1 change(s)
warning[CONTRACT_EXCEEDED]: unknown field "field": the schema declares no such column -- outside the contract inferred for users (ROW_UNKNOWN_FIELD)
  --> users/kurt.json:1:52
   |
 1 | {"id": "kurt", "name": "Kurt Gödel", "born": 1906, "field": "logic"}
   |                                                    ^
   = at: /field
   = constraint: /additionalProperties
   = fixes: FIX_WIDEN_CONTRACT
   = help: the file was written outside reldir; reldir leaves the value as it is and never writes one like it. `reldir doctor --only FIX_WIDEN_CONTRACT` or `reldir schema widen` makes the contract admit it
VALID: 1 table(s), 4 row(s), 0 violation(s), 1 warning(s), [..] lint finding(s), [..] ms
```

Until the contract admits it, reldir leaves the value as it is and writes none
like it. If the value belongs in the table, widen the contract just enough to
admit what the files hold:

```console
$ reldir doctor --fix --only FIX_WIDEN_CONTRACT
[..]
action | path                  | planned
-------+-----------------------+--------
write  | .db/schema/users.json | false
(1 row)
repaired: 1 file(s), revision #6 ([..]) (1 fix(es): FIX_WIDEN_CONTRACT)
$ reldir check
VALID: 1 table(s), 4 row(s), 0 violation(s), 0 warning(s), [..] lint finding(s), [..] ms
```

`reldir schema widen users`, with no member, does the same for one table.

If the value does not belong, correct or remove the file instead; see
[How to repair an invalid database]({{ site.baseurl }}/repair-a-database).

## Add a member the table has no column for

A change that names a column the table does not have is refused with
`UNKNOWN_COLUMN`, and `--widen` does not add one. Add the column with a
migration, then make the change:

```console
$ reldir migrate add-column users email --type string --nullable
[..]
migrated: 1 file(s), revision #7 ([..])
$ reldir update users ada '{"email": "ada@example.org"}'
[..]
updated: 1 file(s), revision #8 ([..])
```

See [How to change the shape of a table that holds rows]({{ site.baseurl }}/change-a-schema).

## Widen a pinned table

In a table whose schema is pinned in `schema/`, a value outside the schema is
an error, whoever wrote it. `--widen` and `reldir schema widen` work there too,
and write the pin:

```console
$ reldir schema pin users
[..]
pinned: 2 file(s), revision #9 ([..])
$ reldir schema widen users.name --null
widened the contract of users: /properties/name: adds "null" to `type`
action | path              | planned
-------+-------------------+--------
write  | schema/users.json | false
(1 row)
widened: 1 file(s), revision #10 ([..])
```

To widen in a way these commands do not express -- a looser `pattern`, a
larger `maxLength`, another `enum` member -- edit `schema/<table>.json`
yourself, then run `reldir schema validate`. The next command records the
edited schema. The keywords are in the [schema reference]({{ site.baseurl }}/schemas).
