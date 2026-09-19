"""A Python driver for reldir.

reldir is a command-line binary, not a linkable library, so this package speaks
to it the way a database driver speaks to a server: it marshals a request, runs
it, and turns the response into Python objects and exceptions. The subprocess is
the wire protocol.

    import reldir

    with reldir.connect("./data") as db:
        db.execute("INSERT INTO users (id, name) VALUES (?, ?)", ["u1", "Alice"])
        for row in db.query("SELECT id, name FROM users ORDER BY name"):
            print(row["id"], row["name"])

Concurrency is the reason this package exists in the shape it does. reldir
admits one writer at a time and unlimited readers, and a writer that finds the
lock held waits for it. When the wait expires the binary reports
``LOCK_CONTENDED``, which is always safe to retry because nothing was written.
`Connection.execute` retries that for you, with the same bounded, jittered
backoff the binary uses internally.

What it does *not* silently retry is ``CONCURRENT_MODIFICATION``: nothing was
written there either, but the state the statement was planned against has moved,
so a blind retry would re-plan against different data. For a bare statement that
is usually what you want and `retry_on_conflict` (default true) does it; for a
read-modify-write it is a lost update, so use `Connection.transaction` which
re-reads and hands you the fresh state.
"""

from __future__ import annotations

import json
import os
import random
import shutil
import subprocess
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, TypeVar

_T = TypeVar("_T")

__all__ = [
    "connect",
    "Connection",
    "Row",
    "ReldirError",
    "DatabaseInvalid",
    "LockContended",
    "ConcurrentModification",
    "PathInterference",
    "QueryError",
    "UnknownName",
    "ResourceLimit",
    "TransactionIncomplete",
    "RecoveryRequired",
    "FormatUnsupported",
    "MetadataCorrupt",
    "Uninitialized",
    "ConfirmationRequired",
    "UsageError",
    "Stale",
    "Result",
    "RetryPolicy",
]

BINARY = "reldir"


class ReldirError(Exception):
    """A diagnostic reported by the binary.

    Carries the machine-readable diagnostic verbatim: `code` is the stable
    error code, and the remaining fields are whatever that diagnostic supplied.
    Scripts should branch on `code` or on the exception subclass, never on the
    message text, which is prose and may be reworded.
    """

    def __init__(
        self,
        code: str,
        message: str,
        *,
        exit_code: int,
        diagnostic: Mapping[str, Any] | None = None,
        stderr: str = "",
    ) -> None:
        super().__init__(f"{code}: {message}" if code else message)
        self.code = code
        self.message = message
        self.exit_code = exit_code
        self.diagnostic: Mapping[str, Any] = diagnostic or {}
        self.stderr = stderr

    @property
    def path(self) -> str | None:
        value = self.diagnostic.get("path")
        return str(value) if value is not None else None

    @property
    def table(self) -> str | None:
        value = self.diagnostic.get("table")
        return str(value) if value is not None else None

    @property
    def field(self) -> str | None:
        value = self.diagnostic.get("field")
        return str(value) if value is not None else None


class DatabaseInvalid(ReldirError):
    """The database does not satisfy its own schemas or constraints."""


class LockContended(ReldirError):
    """Another writer held the writer lock until the wait expired.

    Nothing was written. Retrying is always safe.
    """


class ConcurrentModification(ReldirError):
    """Authoritative files changed underneath the statement.

    Nothing was written, but the state the statement was planned against has
    moved. A retry re-plans against the new data, which is correct for a bare
    statement and wrong for a read-modify-write.
    """


class PathInterference(ReldirError):
    """A directory the transaction needed is no longer a directory.

    Retrying meets the same path, so this needs a look rather than another
    attempt.
    """


class QueryError(ReldirError):
    """SQL outside the supported subset, or a type error inside a query."""


class UnknownName(QueryError):
    """A table, column, or row key that does not resolve."""


class ResourceLimit(ReldirError):
    """A configured limit was exceeded. Nothing was truncated silently."""


class TransactionIncomplete(ReldirError):
    """An interrupted transaction materialised a state needing recovery."""


class RecoveryRequired(TransactionIncomplete):
    """Kept as a distinct name for callers that catch recovery specifically."""


class FormatUnsupported(ReldirError):
    """The on-disk format is not one this binary understands."""


class MetadataCorrupt(ReldirError):
    """Internal metadata could not be read and could not be rebuilt."""


class Uninitialized(ReldirError):
    """The directory is not a database and was not permitted to become one."""


class ConfirmationRequired(ReldirError):
    """The operation needs confirmation, which the driver always supplies."""


class UsageError(ReldirError):
    """A malformed invocation. A bug in the caller or in this driver."""


class Stale(Exception):
    """Raised by a `transaction` body when its compare-and-set found no match.

    Not a diagnostic from the binary: reldir cannot detect that a value moved
    between your read and your write, because each statement is valid on its
    own. This is how a caller reports that it noticed, and `transaction` treats
    it like a conflict -- the block runs again and re-reads.
    """


# Exit codes are a documented contract (docs/errors.md). They classify a failure
# even when no structured diagnostic was emitted -- a binary that dies before it
# can serialise one still exits meaningfully.
_EXIT_CLASSES: dict[int, type[ReldirError]] = {
    1: UsageError,
    2: DatabaseInvalid,
    3: ConcurrentModification,
    4: QueryError,
    5: TransactionIncomplete,
    6: MetadataCorrupt,
    8: DatabaseInvalid,
    9: ConfirmationRequired,
    10: Uninitialized,
}

# The code is more specific than the exit status, so it wins where both apply.
_CODE_CLASSES: dict[str, type[ReldirError]] = {
    "LOCK_CONTENDED": LockContended,
    "CONCURRENT_MODIFICATION": ConcurrentModification,
    "PATH_INTERFERENCE": PathInterference,
    "TRANSACTION_INCOMPLETE": RecoveryRequired,
    "QUERY_UNSUPPORTED": QueryError,
    "QUERY_TYPE_ERROR": QueryError,
    "UNKNOWN_TABLE": UnknownName,
    "UNKNOWN_COLUMN": UnknownName,
    "UNKNOWN_ROW": UnknownName,
    "RESOURCE_LIMIT": ResourceLimit,
    "FORMAT_UNSUPPORTED": FormatUnsupported,
    "INTERNAL_METADATA_CORRUPT": MetadataCorrupt,
    "UNINITIALIZED": Uninitialized,
    "CONFIRMATION_REQUIRED": ConfirmationRequired,
    "USAGE": UsageError,
}


class Row(dict):
    """One result row.

    A plain `dict` preserving the column order the binary emitted, so
    `list(row)` gives the columns in the order they were selected and
    `row["name"]` reads a value.
    """

    __slots__ = ()


class Result(list):
    """What a statement produced.

    A `list` of `Row`, so it iterates and indexes like a query result, plus
    `changed`: how many files the statement rewrote. The count is what makes a
    compare-and-set usable -- an `UPDATE` whose `WHERE` no longer matches
    changes nothing and succeeds, and only the count distinguishes that from a
    write that landed.
    """

    __slots__ = ("changed",)

    def __init__(self, rows: Sequence[Row] = (), changed: int = 0) -> None:
        super().__init__(rows)
        self.changed = changed


@dataclass(frozen=True)
class RetryPolicy:
    """How the driver retries a statement it is safe to retry.

    Mirrors the binary's own waiting policy: bounded, exponential, and jittered
    so that several processes refused at the same instant do not retry in
    lockstep and collide again.

    `attempts` counts the total tries, so `attempts=1` disables retrying. The
    delay before retry *n* is drawn uniformly from ``[0, min(cap, base * 2**n)]``.
    """

    attempts: int = 6
    base_delay: float = 0.01
    max_delay: float = 0.5

    def __post_init__(self) -> None:
        if self.attempts < 1:
            raise ValueError("attempts must be at least 1")
        if self.base_delay < 0 or self.max_delay < 0:
            raise ValueError("delays must not be negative")
        if not all(map(_finite, (self.base_delay, self.max_delay))):
            raise ValueError("delays must be finite")

    def delay_before(self, attempt: int) -> float:
        """Full jitter: a uniform draw from the interval, not its endpoint."""
        ceiling = min(self.max_delay, self.base_delay * (2**attempt))
        return random.uniform(0.0, ceiling)


def _finite(value: float) -> bool:
    return value == value and value not in (float("inf"), float("-inf"))


@dataclass
class _Invocation:
    args: Sequence[str]
    stdout: str
    stderr: str
    returncode: int
    records: list[dict] = field(default_factory=list)


class Connection:
    """An open reldir database.

    Not a persistent connection -- reldir has no server -- but the same object
    a caller expects from a driver: it holds the database location and the
    settings every statement inherits, and it is safe to share between threads.

    Thread safety: writes are serialised on an internal lock. reldir would
    serialise them anyway through its own file lock, but two threads of one
    process contending for that lock burn their retry budgets against each other
    for no reason. Reads are not serialised and run concurrently.
    """

    def __init__(
        self,
        path: str | os.PathLike[str],
        *,
        binary: str = BINARY,
        wait: float = 5.0,
        retry: RetryPolicy | None = None,
        timeout: float | None = None,
        env: Mapping[str, str] | None = None,
    ) -> None:
        self.path = Path(path)
        self.binary = shutil.which(binary) or binary
        if shutil.which(binary) is None and not Path(binary).exists():
            raise FileNotFoundError(
                f"the reldir binary {binary!r} is not on PATH. "
                "Install it with `cargo install reldir`, or pass binary=... ."
            )
        if wait < 0 or not _finite(wait):
            raise ValueError("wait must be a finite number of seconds, zero or greater")
        self.wait = wait
        self.retry = retry or RetryPolicy()
        self.timeout = timeout
        self._env = dict(env) if env is not None else None
        self._write_lock = threading.Lock()
        self._closed = False

    # ---------------------------------------------------------------- plumbing

    def _run(self, args: Sequence[str], *, readonly: bool) -> _Invocation:
        if self._closed:
            raise ValueError("this connection is closed")
        argv = [self.binary, "--db", str(self.path), "--format", "jsonl"]
        if readonly:
            # A read that takes no lock cannot contend and cannot be refused by
            # a concurrent writer, which is the whole reason to distinguish it.
            argv.append("--readonly")
        else:
            # `--yes` because a driver has no terminal to prompt at: a statement
            # that needs confirmation would otherwise hang or fail, and the
            # caller already expressed intent by calling execute.
            argv += ["--wait", repr(self.wait), "--yes"]
        argv += list(args)

        completed = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=self.timeout,
            env=self._env,
            check=False,
        )
        invocation = _Invocation(
            args=argv,
            stdout=completed.stdout,
            stderr=completed.stderr,
            returncode=completed.returncode,
        )
        for line in completed.stdout.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError as cause:
                # The binary promises JSONL on stdout under `--format jsonl`.
                # Anything else means a version mismatch or a crash mid-write,
                # and guessing at half a record would invent data.
                raise ReldirError(
                    "DRIVER_PROTOCOL",
                    f"reldir emitted a line that is not JSON: {line[:200]!r}",
                    exit_code=completed.returncode,
                    stderr=completed.stderr,
                ) from cause
            if isinstance(parsed, dict):
                invocation.records.append(parsed)
        return invocation

    def _raise(self, invocation: _Invocation) -> None:
        diagnostic = _last_diagnostic(invocation)
        code = str(diagnostic.get("code", "")) if diagnostic else ""
        message = str(diagnostic.get("message", "")) if diagnostic else ""
        if not message:
            # No structured diagnostic: the binary failed before it could emit
            # one. stderr is then the only account of what happened, and
            # discarding it would leave the caller with a bare exit code.
            message = invocation.stderr.strip() or (
                f"reldir exited {invocation.returncode} without a diagnostic"
            )
        cls = _CODE_CLASSES.get(code) or _EXIT_CLASSES.get(
            invocation.returncode, ReldirError
        )
        raise cls(
            code or f"EXIT_{invocation.returncode}",
            message,
            exit_code=invocation.returncode,
            diagnostic=diagnostic,
            stderr=invocation.stderr,
        )

    def _attempt(
        self,
        args: Sequence[str],
        *,
        readonly: bool,
        retry_on_conflict: bool,
    ) -> _Invocation:
        last: ReldirError | None = None
        for attempt in range(self.retry.attempts):
            invocation = self._run(args, readonly=readonly)
            if invocation.returncode == 0:
                return invocation
            try:
                self._raise(invocation)
            except LockContended as error:
                # Nothing was written, so another attempt is always sound.
                last = error
            except ConcurrentModification as error:
                if not retry_on_conflict:
                    raise
                last = error
            except RecoveryRequired as error:
                # A transaction was materialising -- renames half-applied -- at
                # the instant this ran. Answering from those rows would report a
                # state that never existed, so the binary refuses, and it is
                # right to. The condition is momentary and self-clearing: the
                # writer holding the lock finishes its renames and the marker
                # comes off, so the next attempt succeeds.
                #
                # It is retried rather than raised because it is not a fact
                # about the database, only about the instant. A transaction that
                # is genuinely interrupted -- the writer died mid-rename --
                # outlives every attempt here and surfaces once the budget is
                # spent, which is when it needs `reldir recover` rather than
                # another try.
                last = error
            # Anything else propagates: it will not become true on a retry.
            if attempt + 1 < self.retry.attempts:
                time.sleep(self.retry.delay_before(attempt))
        assert last is not None
        raise last

    # ------------------------------------------------------------------- reads

    def query(self, sql: str, params: Sequence[Any] | None = None) -> list[Row]:
        """Run a read and return its rows.

        Runs with `--readonly`, so it takes no lock, never contends, and cannot
        be refused by a concurrent writer. It also never advances the recorded
        revision: a read is an observation, not a transition.
        """
        invocation = self._attempt(
            ["sql", _bind(sql, params)], readonly=True, retry_on_conflict=False
        )
        if invocation.returncode != 0:
            self._raise(invocation)
        return [
            Row((k, v) for k, v in record.items() if k != "kind")
            for record in invocation.records
            if record.get("kind") == "row"
        ]

    def one(self, sql: str, params: Sequence[Any] | None = None) -> Row | None:
        """Run a read expected to match at most one row.

        Raises if it matches more, because a caller asking for one row and
        silently receiving the first of several is how a bug hides.
        """
        rows = self.query(sql, params)
        if len(rows) > 1:
            raise QueryError(
                "DRIVER_TOO_MANY_ROWS",
                f"one() matched {len(rows)} rows",
                exit_code=0,
            )
        return rows[0] if rows else None

    def scalar(self, sql: str, params: Sequence[Any] | None = None) -> Any:
        """Return the single value of a single-column, single-row read."""
        row = self.one(sql, params)
        if row is None:
            return None
        if len(row) != 1:
            raise QueryError(
                "DRIVER_NOT_SCALAR",
                f"scalar() selected {len(row)} columns: {list(row)}",
                exit_code=0,
            )
        return next(iter(row.values()))

    # ------------------------------------------------------------------ writes

    def execute(
        self,
        sql: str,
        params: Sequence[Any] | None = None,
        *,
        retry_on_conflict: bool = True,
    ) -> Result:
        """Run a statement that may write.

        Returns a `Result` carrying any rows and `changed`, the number of files
        the statement rewrote. `changed == 0` is how a compare-and-set reports
        that its predicate did not match -- see `Connection.transaction`.

        Retries `LOCK_CONTENDED` always, and `CONCURRENT_MODIFICATION` when
        `retry_on_conflict` is set. Pass `retry_on_conflict=False` when the
        statement was computed from data you read earlier: a retry re-plans
        against state that has since moved, which for a read-modify-write is a
        lost update. `Connection.transaction` handles that case properly.
        """
        with self._write_lock:
            invocation = self._attempt(
                ["sql", _bind(sql, params)],
                readonly=False,
                retry_on_conflict=retry_on_conflict,
            )
        if invocation.returncode != 0:
            self._raise(invocation)
        return _result(invocation)

    def executemany(
        self, sql: str, seq_of_params: Sequence[Sequence[Any]]
    ) -> None:
        """Run one statement repeatedly.

        Each invocation is its own transaction: reldir has no multi-statement
        transaction, so this is a loop, not an atomic batch. Where the shape
        allows it, a single multi-row `INSERT` is both atomic and far faster,
        because it pays the process cost once.
        """
        for params in seq_of_params:
            self.execute(sql, params)

    def insert(self, table: str, row: Mapping[str, Any]) -> None:
        """Insert one row given as a mapping."""
        with self._write_lock:
            invocation = self._attempt(
                ["insert", table, json.dumps(row)],
                readonly=False,
                retry_on_conflict=True,
            )
        if invocation.returncode != 0:
            self._raise(invocation)

    def update(self, table: str, key: str, patch: Mapping[str, Any]) -> None:
        """Patch one row by primary key."""
        with self._write_lock:
            invocation = self._attempt(
                ["update", table, key, json.dumps(patch)],
                readonly=False,
                retry_on_conflict=True,
            )
        if invocation.returncode != 0:
            self._raise(invocation)

    def delete(self, table: str, key: str) -> None:
        """Delete one row by primary key."""
        with self._write_lock:
            invocation = self._attempt(
                ["delete", table, key], readonly=False, retry_on_conflict=True
            )
        if invocation.returncode != 0:
            self._raise(invocation)

    def transaction(
        self,
        body: Callable[[Connection], _T],
        *,
        attempts: int | None = None,
    ) -> _T:
        """Run a read-modify-write, re-running it if the state moves.

        reldir commits one statement at a time, so this is not a multi-statement
        transaction and it does not roll back statements that already
        committed. What it does is the thing a bare retry gets wrong: when a
        statement fails because the files moved, the *whole function* runs
        again, so the reads that informed the writes are taken again against the
        new state. Retrying only the failing statement would re-plan against
        data the caller never saw, which is a lost update.

        `body` takes a callable rather than being a `with` block because a block
        cannot be re-executed: a context manager yields once, so a conflict
        inside it can only be re-raised, never retried. Taking the work as a
        function is what makes the retry real.

        Because the function may run more than once it must be safe to repeat:
        no external side effects that cannot be replayed.

        **Carry what you read into the write.** reldir raises
        `CONCURRENT_MODIFICATION` when the files move *while a statement is in
        flight*, not when they moved between your read and your write -- each
        statement is complete and valid on its own, so nothing detects a stale
        read for you. Make the predicate carry the value you read, and retry
        when nothing changed:

            def increment(tx):
                row = tx.one("SELECT n FROM counters WHERE id = 'c1'")
                result = tx.execute(
                    "UPDATE counters SET n = ? WHERE id = 'c1' AND n = ?",
                    [row["n"] + 1, row["n"]],
                    retry_on_conflict=False,
                )
                if result.changed == 0:
                    raise Stale("c1 moved between the read and the write")

            db.transaction(increment)

        `Stale` is retried like a conflict, so the block re-reads and tries
        again. Without that predicate two concurrent increments both read the
        same value and both succeed, and one increment is silently lost.

        Returns whatever `body` returns.
        """
        budget = attempts if attempts is not None else self.retry.attempts
        if budget < 1:
            raise ValueError("attempts must be at least 1")
        last: Exception | None = None
        for attempt in range(budget):
            try:
                return body(self)
            except (LockContended, ConcurrentModification, Stale) as error:
                last = error
                if attempt + 1 < budget:
                    time.sleep(self.retry.delay_before(attempt))
        assert last is not None
        raise last

    # ------------------------------------------------------------------- state

    def status(self) -> dict:
        """Report validity, revision, and any external changes."""
        invocation = self._attempt(["status"], readonly=True, retry_on_conflict=False)
        if invocation.returncode not in (0, 2):
            self._raise(invocation)
        for record in invocation.records:
            if record.get("kind") in ("status", "check_summary"):
                return dict(record)
        return {"valid": invocation.returncode == 0}

    def check(self) -> None:
        """Validate the whole database, raising `DatabaseInvalid` if it is not."""
        invocation = self._attempt(["check"], readonly=True, retry_on_conflict=False)
        if invocation.returncode != 0:
            self._raise(invocation)

    def tables(self) -> list[str]:
        """List governed table names.

        The binary exposes these through the shell's `.tables`, which prints
        them space-separated on stdout. There is no JSONL listing to parse, so
        this reads that line rather than inventing a subcommand: a driver that
        called one the binary does not have would fail for every caller.
        """
        if self._closed:
            raise ValueError("this connection is closed")
        completed = subprocess.run(
            [self.binary, "--db", str(self.path), "--readonly", "shell"],
            input=".tables\n",
            capture_output=True,
            text=True,
            timeout=self.timeout,
            env=self._env,
            check=False,
        )
        if completed.returncode != 0:
            self._raise(
                _Invocation(
                    args=["shell", ".tables"],
                    stdout=completed.stdout,
                    stderr=completed.stderr,
                    returncode=completed.returncode,
                )
            )
        return completed.stdout.split()

    def close(self) -> None:
        """Release the connection.

        There is no socket to close: this marks the object unusable so that a
        use-after-close is an error here rather than a surprise later.
        """
        self._closed = True

    def __enter__(self) -> Connection:
        return self

    def __exit__(self, *exception: object) -> None:
        self.close()

    def __repr__(self) -> str:
        return f"<reldir.Connection path={str(self.path)!r} wait={self.wait}>"


def connect(
    path: str | os.PathLike[str],
    *,
    binary: str = BINARY,
    wait: float = 5.0,
    retry: RetryPolicy | None = None,
    timeout: float | None = None,
    env: Mapping[str, str] | None = None,
) -> Connection:
    """Open a reldir database.

    `wait` is how long the binary waits for the writer lock before reporting
    contention; the driver then retries under `retry`. The two compose: the
    binary absorbs short contention without a process restart, and the driver
    covers longer outages.
    """
    return Connection(
        path, binary=binary, wait=wait, retry=retry, timeout=timeout, env=env
    )


def _result(invocation: _Invocation) -> Result:
    """Turn an invocation's records into rows plus a change count.

    The binary emits one `change` record per file it rewrote, and a single
    `no_change` record when a statement matched nothing. Counting the former is
    what lets a caller tell a compare-and-set that missed from one that landed.
    """
    rows = [
        Row((k, v) for k, v in record.items() if k != "kind")
        for record in invocation.records
        if record.get("kind") == "row"
    ]
    changed = sum(1 for record in invocation.records if record.get("kind") == "change")
    return Result(rows, changed)


def _last_diagnostic(invocation: _Invocation) -> dict:
    """The diagnostic that explains a failure.

    Errors are emitted on stderr as a single JSON object when a machine format
    is selected, and diagnostics also appear inline on stdout. The stderr one
    describes the failure that ended the run, so it wins; stdout is searched
    only when stderr carried nothing parseable.
    """
    text = invocation.stderr.strip()
    if text:
        for line in reversed(text.splitlines()):
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(parsed, dict) and "code" in parsed:
                return parsed
    for record in reversed(invocation.records):
        if record.get("kind") == "diagnostic" and record.get("severity") == "error":
            return record
    return {}


def _bind(sql: str, params: Sequence[Any] | None) -> str:
    """Substitute `?` placeholders, quoting each value as a SQL literal.

    reldir's CLI takes one SQL string, so parameters are bound here. They are
    bound by this driver rather than by string formatting in the caller, which
    is the difference between a quoted literal and an injection.

    A placeholder inside a string literal is not a placeholder, so the scan
    tracks quoting rather than counting `?` characters.
    """
    if params is None:
        return sql
    params = list(params)
    out = []
    index = 0
    in_string = False
    i = 0
    while i < len(sql):
        char = sql[i]
        if in_string:
            out.append(char)
            if char == "'":
                # A doubled quote is an escaped quote, not the end of the
                # literal: 'it''s' is one string.
                if i + 1 < len(sql) and sql[i + 1] == "'":
                    out.append(sql[i + 1])
                    i += 2
                    continue
                in_string = False
            i += 1
            continue
        if char == "'":
            in_string = True
            out.append(char)
            i += 1
            continue
        if char == "?":
            if index >= len(params):
                raise ValueError(
                    f"SQL has more placeholders than the {len(params)} parameters given"
                )
            out.append(_literal(params[index]))
            index += 1
            i += 1
            continue
        out.append(char)
        i += 1
    if in_string:
        raise ValueError("SQL ends inside an unterminated string literal")
    if index != len(params):
        raise ValueError(
            f"SQL has {index} placeholders but {len(params)} parameters were given"
        )
    return "".join(out)


def _literal(value: Any) -> str:
    """Render one Python value as a SQL literal.

    Only the types reldir's type system has. Anything else is refused rather
    than coerced through `str()`, because a silent stringification is how a
    datetime becomes an unparseable row.
    """
    if value is None:
        return "NULL"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        if value != value or value in (float("inf"), float("-inf")):
            raise ValueError(f"{value!r} has no SQL literal")
        return repr(value)
    if isinstance(value, str):
        return "'" + value.replace("'", "''") + "'"
    if isinstance(value, (Mapping, list)):
        # A JSON column takes a JSON document, quoted as a string literal.
        return "'" + json.dumps(value).replace("'", "''") + "'"
    raise TypeError(
        f"{type(value).__name__} has no reldir literal; "
        "pass a str, int, float, bool, None, list, or dict"
    )
