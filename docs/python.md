---
title: Python
---

# Python

The `reldir` package runs the engine inside your Python process and puts a
[PEP 249](https://peps.python.org/pep-0249/) (DB-API 2.0) interface on it. It
has no runtime dependencies. The wheel also installs the `reldir` command,
which is the same command as the standalone binary.

The package is an abi3 wheel for CPython 3.10 and later. To build and install
it, see [How to install reldir]({{ site.baseurl }}/install).

```python
import reldir

with reldir.connect("data") as db:   # commits on success, rolls back on error
    db.execute("UPDATE users SET age = age + 1 WHERE id = ?", ["ada"])
    for row in db.query("SELECT id, name FROM users ORDER BY name"):
        print(row["id"], row["name"])
```

## Connecting

```text
reldir.connect(
    path,
    *,
    readonly=False,     # write nothing at all, not even derived state
    establish=True,     # adopt a folder of JSON files the first time it is opened
    autocommit=False,   # commit each change as soon as it is made
    wait=None,          # seconds to wait for another writer's lock
    timeout=None,       # seconds a query may run
    max_rows=None,      # rows a query may return
) -> reldir.Connection
```

Opening a folder for the first time adopts it, exactly as the command line
does: schemas are inferred and recorded, and no row file is changed (see
[concepts]({{ site.baseurl }}/concepts)). With `establish=False`, a folder that
is not yet a database is refused. A path that does not exist raises
`ProgrammingError` with code `PATH_NOT_FOUND`.

A connection holds no file open between calls. Every call reads the folder as
it is at that moment, so an edit another program made a moment ago is seen.
While [`reldir serve`]({{ site.baseurl }}/cli#serve) runs, each call examines
only what changed.

## PEP 249

| Global | Value | Meaning |
|---|---|---|
| `reldir.apilevel` | `"2.0"` | DB-API 2.0 |
| `reldir.threadsafety` | `1` | threads may share the module, not connections |
| `reldir.paramstyle` | `"qmark"` | `?` placeholders; `:name` with a mapping also works |

`threadsafety` is `1` because an open transaction belongs to the thread that
began it. Opening a connection is cheap, so give each thread its own.

The standard exception classes are module attributes and attributes of every
connection: `Warning`, `Error`, `InterfaceError`, `DatabaseError`,
`DataError`, `OperationalError`, `IntegrityError`, `InternalError`,
`ProgrammingError` and `NotSupportedError`.

## Transactions

Following PEP 249, the first change made through a connection begins a
transaction, and the change stays in it until `commit()` or `rollback()`. The
transaction holds the database's writer lock for that whole time: other
writers wait for it, up to `wait` seconds, and readers are never blocked,
because they answer from the last committed state. Nothing reaches a file
until `commit()`. The state the transaction's statements make together is
validated then, and written as one revision or not at all.

Reads made inside an open transaction see the transaction's own changes:

```python
db = reldir.connect("data")
db.execute("UPDATE users SET age = 37 WHERE id = ?", ["ada"])
assert db.in_transaction
assert db.scalar("SELECT age FROM users WHERE id = 'ada'") == 37   # read your write
done = db.commit()            # what the commit did, or None if none was open
print(done["revision"])       # (number, id) of the revision recorded
```

`rollback()` discards the open transaction. `close()` rolls back whatever is
uncommitted and closes the connection; using it afterwards raises
`ProgrammingError`.

With `autocommit=True`, each change commits as soon as it is made, and no
transaction stays open between calls.

A transaction kept open longer than `transaction_timeout_seconds` (see
[configuration]({{ site.baseurl }}/configuration)) is rolled back, and its
next statement raises `OperationalError` with code `TRANSACTION_EXPIRED`.

### Blocks

Used as a context manager, a connection commits when the block succeeds and
rolls back when it raises, as `sqlite3` does. The connection stays open
afterwards.

```python
with reldir.connect("data") as db:
    db.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'")
    db.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'")
```

`transaction()` opens an explicit block. It raises `ProgrammingError` if a
transaction is already open.

```python
with db.transaction(idempotency_key="transfer-42"):
    db.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'")
    db.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'")
```

`idempotency_key` names the change. If history already holds a revision made
under that key, committing reports that revision and makes nothing again, so
retrying after an uncertain failure is always safe. `widen=True` lets the
change widen a table's contract to admit a value the contract never admitted,
in the same revision (see [schemas]({{ site.baseurl }}/schemas)). Without it,
such a change is refused with `CONTRACT_WIDENING_REQUIRED`.

## Cursors

```python
cursor = db.cursor()
cursor.execute("SELECT id, name FROM users WHERE age > ? ORDER BY id", [30])
print([column[0] for column in cursor.description])   # ['id', 'name']
first = cursor.fetchone()                              # ('ada', 'Ada')
rest = cursor.fetchall()
```

- `execute(sql, parameters=None)` runs one statement. `parameters` is a
  sequence for `?` or a mapping for `:name`. It returns the cursor.
- `description` is a tuple of 7-item sequences, one per column: the name, then
  six `None`s. An answer with no rows still has its columns.
- `rowcount` is `-1` after a read. After a change, it counts the rows the
  statement itself changed; rows changed by referential actions (a cascade, a
  `set_null`) are not counted.
- `fetchone()`, `fetchmany(size=cursor.arraysize)` and `fetchall()` return
  rows as tuples, or whatever the connection's `row_factory` makes of them.
  A cursor is iterable.
- A change with a `RETURNING` clause leaves its returned rows to fetch, with
  their `description`.
- `executemany(sql, seq_of_parameters)` runs one change once per set of
  parameters, all in one transaction: if one fails, none is written. With
  `autocommit=True`, it commits once, at the end.
- `setinputsizes()` and `setoutputsize()` do nothing: values bind as JSON.
- `lastrowid` is always `None`. A table's key is a column of the row; read it
  from `RETURNING`, or from what `insert()` returns.

`connection.execute()` and `connection.executemany()` are shortcuts that open
a cursor, as in `sqlite3`.

### Rows as dicts

```python
db.row_factory = reldir.dict_factory
row = db.execute("SELECT id, name FROM users ORDER BY id").fetchone()
# {'id': 'ada', 'name': 'Ada'}
```

`reldir.Row` is a `dict` that keeps the order the columns were selected in.

## Conveniences

Beside PEP 249, a connection has methods for common work:

| Method | Returns |
|---|---|
| `query(sql, params=None)` | every row of a read, each a `Row` |
| `one(sql, params=None)` | the one row, or `None`; more than one row raises `ProgrammingError` |
| `scalar(sql, params=None)` | the single value of a one-row, one-column answer, or `None` |
| `insert(table, row_or_rows)` | the rows as written, generated keys included |
| `update(table, key, patch)` | the row as written |
| `delete(table, key)` | the number of rows removed or changed, referential actions included |
| `get(table, key)` | the row, or `None` |
| `tables()` | a list of `{"name", "rows", "key", "pinned"}` |
| `check()` | `{"valid", "errors", "warnings"}`, each diagnostic a dict |
| `state()` | a name for the state the database holds now |
| `root` | the database's folder |

A key is the key's value, or a list of values for a composite key.
`insert`, `update` and `delete` join the open transaction, or begin one, or
commit at once under `autocommit=True`. `get` reads committed rows; inside an
open transaction, read with `query` instead.

```python
db = reldir.connect("data", autocommit=True)
[ticket] = db.insert("tickets", {"what": "a"})      # the sequence filled in
print(ticket["n"])
db.update("users", "ada", {"name": "Augusta"})
db.delete("users", "ada")
```

## Errors

Every error reldir raises is an instance of `reldir.Error` and carries:

- `code`: the registered code, such as `"FOREIGN_KEY_VIOLATION"` (see
  [errors]({{ site.baseurl }}/errors));
- `exit`: the exit status the command line reports for it;
- `diagnostics`: a list of dicts, every fault behind the failure, each with its
  `code`, `message`, and where known its `path`, `location`, `pointer`,
  `constraint`, `expected` and `observed`.

The class follows the code:

| Class | Codes |
|---|---|
| `DataError` | `TYPE_MISMATCH`, `INPUT_INVALID`, `NOT_NULL_VIOLATION` |
| `NotSupportedError` | `TRANSACTION_STATEMENT` |
| `OperationalError` | `READ_ONLY`; contention (`LOCK_CONTENDED`, `CONCURRENT_MODIFICATION`, `TRANSACTION_EXPIRED`), recovery, untrusted metadata and I/O, decisions required, unsafe filesystems, commit outcomes |
| `IntegrityError` | every other fault that leaves the database invalid: keys, references, checks, assertions, schema rules, `CONTRACT_WIDENING_REQUIRED` |
| `ProgrammingError` | usage errors and names that do not resolve: unknown tables and columns, statements outside the SQL surface |
| `InternalError` | a defect in reldir |

```python
try:
    db.execute("UPDATE posts SET user_id = 'nobody' WHERE id = 'p2'")
    db.commit()
except reldir.IntegrityError as error:
    print(error.code)                      # FOREIGN_KEY_VIOLATION
    print(error.diagnostics[0]["path"])    # posts/p2.json
```

## Values

Values cross between Python and the database exactly, as JSON does.

| Python | Stored as |
|---|---|
| `None` | `null` |
| `bool` | `true` / `false` |
| `int` | an integer, of any size |
| `float` | a number; `nan` and the infinities raise `ValueError` |
| `decimal.Decimal` | a number, with its digits as written |
| `str` | a string |
| `datetime`, `date`, `time` | its ISO 8601 text |
| `list`, `tuple` | an array |
| `dict` with `str` keys | an object |

Anything else raises `TypeError`. Read back, an integer is an `int` of any
size, any other number is a `float`, and arrays and objects are lists and
dicts. A decimal column holds text in reldir's canonical form (see
[schemas]({{ site.baseurl }}/schemas)), so it reads back as the exact `str`.

## asyncio

`reldir.aio` runs each call on a worker thread, so the event loop never waits
on the filesystem. Changes commit one by one; `run_transaction` makes several
one change. The whole transaction runs on one worker thread, because a
transaction belongs to the thread that began it.

```python
import asyncio
import reldir.aio

async def main():
    db = await reldir.aio.connect("data")
    rows = await db.query("SELECT * FROM users")
    changed = await db.execute("UPDATE users SET age = 1 WHERE id = 'ada'")  # rows changed

    def transfer(tx: reldir.Connection) -> int:
        tx.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'")
        tx.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'")
        return tx.scalar("SELECT balance FROM accounts WHERE id = 'a'")

    balance = await db.run_transaction(transfer, idempotency_key="transfer-42")

asyncio.run(main())
```

`AsyncConnection` has `query`, `one`, `scalar`, `execute`, `insert`, `check`
and `run_transaction`. `reldir.aio.connect` takes the same options as
`reldir.connect`, except `autocommit`.

## pandas

pandas reads through any DB-API connection:

```python
import pandas
import reldir

db = reldir.connect("data")
frame = pandas.read_sql("SELECT id, age FROM users WHERE age > ?", db, params=[30])
```

pandas warns that it tests only SQLAlchemy and `sqlite3` connections; the
warning is pandas' own and can be filtered with `warnings.filterwarnings`.

## The command

The wheel installs `reldir` on the `PATH`. `python -m reldir` runs the same
command, in the same process:

```sh
python -m reldir --db data check
```

See the [CLI reference]({{ site.baseurl }}/cli).
