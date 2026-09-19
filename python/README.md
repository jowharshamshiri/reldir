# reldir — Python driver

A driver for [reldir](https://github.com/jowharshamshiri/reldir), a relational
database that lives in a directory of JSON files.

```sh
pip install reldir
cargo install reldir   # the driver drives the binary; both are needed
```

```python
import reldir

with reldir.connect("./data") as db:
    db.execute("INSERT INTO users (id, name) VALUES (?, ?)", ["u1", "Alice"])

    for row in db.query("SELECT id, name FROM users ORDER BY name"):
        print(row["id"], row["name"])
```

## Why this exists

reldir is a command-line binary, not a linkable library. This package speaks to
it the way a database driver speaks to a server: it marshals a request, runs it,
and turns the response into Python objects and exceptions. The subprocess is the
wire protocol.

## Concurrency

This is the part worth reading.

reldir admits **one writer at a time and unlimited readers**. A writer that finds
the lock held waits for it — bounded, with jittered backoff — and reports
`LOCK_CONTENDED` if the wait expires. The driver then retries.

Two layers, composing deliberately: the binary absorbs short contention without
paying for a process restart, and the driver covers longer outages.

```python
db = reldir.connect("./data", wait=5.0, retry=reldir.RetryPolicy(attempts=6))
```

Reads take no lock at all, so they never contend. They can still be refused for
one reason: while a transaction is *materialising* — renames half-applied —
answering would report a state that never existed. The driver retries that for
you, because the condition is momentary and self-clearing: measured against a
continuous writer it cleared on the first retry every time.

A transaction that is merely *staged*, which every ordinary write passes
through, does not refuse readers at all.

What is **not** absorbed is a transaction whose writer actually died mid-rename.
That marker never clears on its own, so it outlives the retry budget and reaches
you as `RecoveryRequired` — which is the signal to run `reldir recover`.

### Three ways a write is refused

All exit `3`, and in all three **nothing was written**:

| Exception | Meaning | Retry? |
|---|---|---|
| `LockContended` | someone else held the lock longer than the wait | always safe |
| `ConcurrentModification` | the files moved underneath the statement | safe, but it re-plans against new data |
| `PathInterference` | a directory the transaction needed is not a directory | no — look at it |

`execute()` retries the first two by default. That is right for a bare
statement and **wrong for a read-modify-write**, where re-planning against moved
state is a lost update. For that, use `transaction()`, which re-runs the whole
function so the reads are taken again:

```python
def increment(tx):
    row = tx.one("SELECT n FROM counters WHERE id = 'c1'")
    result = tx.execute(
        "UPDATE counters SET n = ? WHERE id = 'c1' AND n = ?",
        [row["n"] + 1, row["n"]],
        retry_on_conflict=False,   # let the block retry, not the statement
    )
    if result.changed == 0:
        raise reldir.Stale("c1 moved between the read and the write")

db.transaction(increment)
```

**Carry what you read into the write.** reldir raises
`ConcurrentModification` when files move *while a statement is in flight*, not
when they moved between your read and your write — each statement is complete
and valid on its own, so nothing detects a stale read for you. The `AND n = ?`
predicate is what makes the conflict visible: the update matches nothing,
`result.changed` is `0`, and raising `Stale` sends the block round again.
Without it, two concurrent increments both read the same value, both succeed,
and one is silently lost.

It takes a function rather than being a `with` block for a reason that matters:
a context manager yields once, so a conflict inside a `with` can only be
re-raised, never retried. Taking the work as a function is what makes the retry
real — and a `with`-shaped version silently loses exactly half the updates under
contention.

Because the function may run more than once, it must be safe to repeat.

## Threads

`Connection` is safe to share. Writes are serialised on an internal lock —
reldir would serialise them anyway, but two threads of one process contending
for the file lock would burn their retry budgets against each other for nothing.
Reads run concurrently.

## Parameters

`?` placeholders are bound by the driver, which quotes each value as a SQL
literal. The scan tracks string quoting, so a `?` inside a literal is left
alone, and `'it''s'` is understood as one string.

Only types reldir has are accepted: `str`, `int`, `float`, `bool`, `None`,
`list`, `dict`. Anything else raises `TypeError` rather than being coerced
through `str()`, because a silent stringification is how a `datetime` becomes an
unparseable row.

## Cost

Every call is a process spawn plus a full-directory validation — roughly 100ms.
Fine for tens of writes per second, not thousands. Where the shape allows it, a
single multi-row `INSERT` is both atomic and far faster than `executemany`,
which is a loop and not an atomic batch.

## Errors

Every exception carries the binary's machine-readable diagnostic verbatim:

```python
try:
    db.execute("INSERT INTO users (id) VALUES ('u1')")
except reldir.DatabaseInvalid as error:
    print(error.code, error.message, error.path, error.field)
```

Branch on `error.code` or the exception class, never on message text.

## License

MIT.
