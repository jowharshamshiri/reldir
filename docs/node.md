---
title: Node.js
---

# Node.js

`@reldir/reldir` runs the engine inside your Node.js process, behind a promise
API. Every call runs on the libuv thread pool, so the event loop keeps running
while reldir reads the folder or waits for another writer's lock.

The package needs Node.js 22 or later and a native module built for its
platform: Linux, macOS or Windows, on x86-64 or arm64. To build and install
it, see [How to install reldir]({{ site.baseurl }}/install).

```js
const reldir = require('@reldir/reldir');

const db = await reldir.open('data');
const adults = await db.query('SELECT id, name FROM users WHERE age >= ?', [18]);
await db.execute("UPDATE users SET age = age + 1 WHERE id = 'ada'");
```

## Opening

```text
const db = await reldir.open(path, {
  readonly: false,   // write nothing at all, not even derived state
  establish: true,   // adopt a folder of JSON files the first time it is opened
  wait: undefined,   // seconds to wait for another writer's lock
  timeout: undefined,// seconds a query may run
  maxRows: undefined,// rows a query may return
});
```

Opening a folder for the first time adopts it, as the command line does:
schemas are inferred and recorded, and no row file is changed (see
[concepts]({{ site.baseurl }}/concepts)). A `Database` holds no file open
between calls; every call answers from the folder as it is then. While
[`reldir serve`]({{ site.baseurl }}/cli#serve) runs, each call examines only
what changed.

## Reading

| Method | Resolves to |
|---|---|
| `query(sql, params)` | every row, each an object |
| `select(sql, params)` | `{ columns, rows }`: the columns in order, even for an empty answer |
| `one(sql, params)` | the one row, or `undefined`; more than one row is an error |
| `scalar(sql, params)` | the single value of a one-row, one-column answer |
| `get(table, key)` | the row a key names, or `undefined` |
| `tables()` | `[{ name, rows, key, pinned }]` |
| `check()` | `{ valid, errors, warnings }` |
| `state()` | a name for the state the database holds now |

`params` is an array for `?` placeholders, or an object for `:name` ones. A
key is the key's value, or an array of values for a composite key.

```js
const ada = await db.one('SELECT * FROM users WHERE id = :id', { id: 'ada' });
const { columns, rows } = await db.select('SELECT id, name FROM users WHERE age > 99');
```

## Changing

Each of these commits one change, validated before any file is written.

| Method | Changes |
|---|---|
| `execute(sql, params, options)` | one `INSERT`, `UPDATE` or `DELETE` |
| `insert(table, rowOrRows)` | one row, or several, generated columns filled in |
| `update(table, key, patch)` | the columns `patch` names, of the row `key` names |
| `delete(table, key)` | the row `key` names, with its referrers' actions |

Each resolves to what the change did:

```ts
interface Changed {
  rows: number;        // rows written or removed, referential actions included
  induced: number;     // of those, the rows referential actions changed
  files: number;       // files written or removed
  returning: Row[];    // RETURNING's rows, or the rows an insert or update wrote
  revision: { number: number; id: string } | null;
  replayed: boolean;   // made before under the same idempotency key
  warnings: Diagnostic[];
}
```

`execute` takes `{ idempotencyKey, widen }`. An idempotency key names the
change: if history already holds a revision made under it, that revision is
reported (`replayed: true`) and nothing is made again, so a retry after an
uncertain failure is always safe. `widen: true` lets the change widen a
table's contract to admit a value it never admitted, in the same revision
(see [schemas]({{ site.baseurl }}/schemas)).

```js
const done = await db.execute(
  'UPDATE accounts SET balance = balance - 5 WHERE id = ? RETURNING balance',
  ['a'],
  { idempotencyKey: 'withdraw-17' },
);
console.log(done.returning[0].balance, done.revision.number);
```

## Transactions

```js
const balance = await db.transaction(async (tx) => {
  await tx.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'");
  await tx.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'");
  return (await tx.query("SELECT balance FROM accounts WHERE id = 'a'"))[0].balance;
});
```

`transaction(work, options)` begins a transaction, runs `work`, and commits
when its promise resolves, or rolls back when it rejects. `options` takes the
same `{ idempotencyKey, widen }` as `execute`.

`begin(options)` returns the `Transaction` to drive by hand. It has `query`,
`execute`, `insert`, `update`, `delete`, `commit` and `rollback`, and an
`active` property that is `false` once it has ended; a call on an ended
transaction is refused.

A transaction holds the database's writer lock from `begin` until it ends:
other writers wait for it, and readers answer from the last committed state.
Its statements see what the statements before them did. Nothing reaches a file
until `commit()`, which judges the state they make together and writes it as
one revision, or refuses it and writes nothing. A transaction runs on a thread
of its own; one that is garbage-collected before it ends is rolled back.

## Errors

A failure rejects with a `ReldirError`:

```js
try {
  await db.execute("UPDATE posts SET user_id = 'ghost'");
} catch (error) {
  if (!(error instanceof reldir.ReldirError)) throw error;
  console.log(error.code);                  // 'FOREIGN_KEY_VIOLATION'
  console.log(error.exit);                  // the command line's exit status for it
  console.log(error.diagnostics[0].path);   // 'posts/p1.json'
}
```

`code` is the registered code (see [errors]({{ site.baseurl }}/errors)), and
`diagnostics` lists every fault behind the failure, each with its `code`,
`message`, and where known its `path`, `location`, `pointer` and `help`.

## Values

Values cross exactly. An integer beyond `Number.MAX_SAFE_INTEGER` comes back
as a `BigInt`, and a `BigInt` may be written:

```js
await db.update('users', 'ada', { big: 12345678901234567890123n });
const row = await db.get('users', 'ada');
row.big === 12345678901234567890123n;   // true
```

Other numbers are `number`s, and arrays and objects are arrays and objects. A
decimal column holds text in reldir's canonical form, so it reads back as the
exact string.

## TypeScript

The package ships its types (`index.d.ts`): `open`, `Database`,
`Transaction`, `ReldirError`, `Row`, `Params`, `OpenOptions`,
`ChangeOptions`, `Changed` and `Diagnostic`.

```ts
import { open, type Row } from '@reldir/reldir';

const db = await open('data', { wait: 5 });
const rows: Row[] = await db.query('SELECT * FROM users');
```

`require('@reldir/reldir').version` is the engine's version.
