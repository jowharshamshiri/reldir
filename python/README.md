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
you as `RecoveryRequired`. The next write — from any process — rolls it
forward; `reldir recover` does so explicitly.

### Three ways a write is refused

All exit `3`, and in all three **nothing was written**:

| Exception | Meaning | Retry? |
|---|---|---|
| `LockContended` | someone else held the lock longer than the wait | always safe |
| `ConcurrentModification` | the files moved while the statement was committing | safe, but it plans again against new data |
| `PathInterference` | a directory the transaction needed is not a directory | no — look at it |

`execute()` retries the first two by default. reldir plans each statement under
its writer lock, so `UPDATE counters SET n = n + 1` never loses an increment and
a bare statement is always safe to retry. What reldir cannot see is a value
**you** read in Python and wrote back later — a read-modify-write. For that, use
`transaction()`, which re-runs the whole function so the reads are taken again:

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

**Carry what you read into the write.** Each statement is planned against the
current state and is valid on its own, so nothing detects that a value moved
between your read and your write for you. The `AND n = ?`
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

`?` placeholders are bound by the binary, never spliced into the SQL: each value
travels as JSON in its own `--param` argument, so a quote in a value is data and
`7` and `"7"` stay different values. A `?` inside a string literal is not a
placeholder, and a count that disagrees with the placeholders is refused as a
`QueryError`.

Only types reldir has are accepted: `str`, `int`, `float`, `bool`, `None`,
`list`, `dict`. Anything else raises `TypeError` before anything runs, rather
than being coerced through `str()`, because a silent stringification is how a
`datetime` becomes an unparseable row.

## Cost

Every call is a process spawn. Observation is incremental — unchanged files are
recognised by their metadata and not re-read — and a commit validates what the
change touches, so the cost is dominated by the spawn and the durable commit,
not by the size of the database. Fine for tens of writes per second, not
thousands. Where the shape allows it, a
single multi-row `INSERT` is both atomic and far faster than `executemany`,
which is a loop and not an atomic batch.

## Errors

Every call reads the binary's `command_result` envelope (`--format json`), and
every exception carries its diagnostic verbatim — `error.diagnostic` is the one
that stopped the command, `error.diagnostics` everything it reported:

```python
try:
    db.check()
except reldir.DatabaseInvalid as error:
    print(error.code, error.message, error.path)
    for violation in error.diagnostics:
        print(violation["code"], violation.get("path"), violation.get("pointer"))
```

Branch on `error.code` or the exception class, never on message text.

| Exception | Code | Exit |
|---|---|---|
| `UsageError` | `USAGE` | 1 |
| `DatabaseInvalid` | a violation code | 2 |
| `LockContended`, `ConcurrentModification`, `PathInterference` | as named | 3 |
| `QueryError`, `UnknownName` | `QUERY_*`, `UNKNOWN_*` | 4 |
| `TransactionIncomplete`, `RecoveryRequired` | as named | 5 |
| `FormatUnsupported`, `MetadataCorrupt` | `FORMAT_UNSUPPORTED`, `INTERNAL_METADATA_CORRUPT` | 6 |
| `DecisionRequired` | `DECISION_REQUIRED` | 9 |
| `InferenceFailed` | `INFER_*` | 8 |
| `Uninitialized` | `UNINITIALIZED` | 10 |
| `UnsafeFilesystem` | `UNSAFE_FILESYSTEM` | 11 |

`status()` never raises for an invalid database — it is an observation, and
returns `valid`, `state`, `violations` and the recorded `revision`.

## License

MIT.
