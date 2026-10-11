"""reldir: a folder of JSON files as a relational database.

The engine runs in this process. This module is its DB-API 2.0 (PEP 249)
interface, with a few conveniences beside it::

    import reldir

    with reldir.connect("data") as db:
        db.execute("UPDATE users SET age = age + 1 WHERE id = ?", ["ada"])
        print(db.query("SELECT id, name FROM users ORDER BY name"))

A connection reads the folder as it is at each call, so an edit another
program made a moment ago is seen. A change opens a transaction, as PEP 249
requires: it holds the writer lock until ``commit()`` or ``rollback()``, and
nothing reaches a file until it commits. With ``autocommit=True`` each change
commits at once.
"""

from __future__ import annotations

import contextlib
from collections.abc import Iterator, Mapping, Sequence
from typing import Any, Callable

from reldir._reldir import (
    DatabaseError,
    DataError,
    Error,
    IntegrityError,
    InterfaceError,
    InternalError,
    NotSupportedError,
    OperationalError,
    ProgrammingError,
    Warning,
    __version__,
)
from reldir._reldir import Database as _Database  # noqa: F401  (also used by reldir.aio)
from reldir._reldir import Transaction as _Transaction

__all__ = [
    "apilevel",
    "threadsafety",
    "paramstyle",
    "connect",
    "Connection",
    "Cursor",
    "Row",
    "dict_factory",
    "Warning",
    "Error",
    "InterfaceError",
    "DatabaseError",
    "DataError",
    "OperationalError",
    "IntegrityError",
    "InternalError",
    "ProgrammingError",
    "NotSupportedError",
    "__version__",
]

apilevel = "2.0"
# Threads may share the module. A connection holds its open transaction for
# the thread that began it, so a connection is not shared between threads;
# open one per thread -- opening is cheap.
threadsafety = 1
# `?` for positional parameters; `:name` with a mapping works as well.
paramstyle = "qmark"

Parameters = Sequence[Any] | Mapping[str, Any] | None


class Row(dict):
    """A row as a dict, in the order its columns were selected."""

    __slots__ = ()


def dict_factory(cursor: Cursor, row: tuple) -> Row:
    """A ``row_factory`` that makes each row a :class:`Row`."""
    return Row(zip((column[0] for column in cursor.description or ()), row))


def connect(
    path: str | Any,
    *,
    readonly: bool = False,
    establish: bool = True,
    autocommit: bool = False,
    wait: float | None = None,
    timeout: float | None = None,
    max_rows: int | None = None,
) -> Connection:
    """Open the database in ``path``, adopting a folder of JSON files the
    first time unless ``establish`` is false.

    ``readonly`` writes nothing at all, not even derived state; ``wait`` is
    how long to wait for another writer; ``timeout`` how long a query may
    run; ``max_rows`` how many rows a query may return.
    """
    database = _Database(
        path,
        readonly=readonly,
        establish=establish,
        wait=wait,
        timeout=timeout,
        max_rows=max_rows,
    )
    return Connection(database, autocommit=autocommit)


class Connection:
    """A connection to one database. See :func:`connect`."""

    Warning = Warning
    Error = Error
    InterfaceError = InterfaceError
    DatabaseError = DatabaseError
    DataError = DataError
    OperationalError = OperationalError
    IntegrityError = IntegrityError
    InternalError = InternalError
    ProgrammingError = ProgrammingError
    NotSupportedError = NotSupportedError

    def __init__(self, database: _Database, *, autocommit: bool = False) -> None:
        self._database: _Database | None = database
        self._transaction: _Transaction | None = None
        self.autocommit = autocommit
        self.row_factory: Callable[[Cursor, tuple], Any] | None = None

    # ------------------------------------------------------------- PEP 249

    def cursor(self) -> Cursor:
        return Cursor(self)

    def commit(self) -> dict | None:
        """Commit the open transaction as one change; what it did, or None
        when no transaction was open."""
        self._live()
        if self._transaction is None:
            return None
        transaction, self._transaction = self._transaction, None
        return transaction.commit()

    def rollback(self) -> None:
        """Discard the open transaction."""
        self._live()
        if self._transaction is not None:
            transaction, self._transaction = self._transaction, None
            transaction.rollback()

    def close(self) -> None:
        """Roll back whatever is uncommitted, and close."""
        if self._database is None:
            return
        try:
            if self._transaction is not None:
                self.rollback()
        finally:
            self._database = None

    # ------------------------------------------------- sqlite3-style helpers

    def execute(self, sql: str, parameters: Parameters = None) -> Cursor:
        return self.cursor().execute(sql, parameters)

    def executemany(self, sql: str, seq_of_parameters: Sequence[Parameters]) -> Cursor:
        return self.cursor().executemany(sql, seq_of_parameters)

    def __enter__(self) -> Connection:
        return self

    def __exit__(self, kind: type | None, value: BaseException | None, trace: Any) -> None:
        """Commit when the block succeeds, roll back when it raises. The
        connection stays open, as sqlite3's does."""
        if self._database is None:
            return
        if kind is None:
            self.commit()
        else:
            self.rollback()

    @property
    def in_transaction(self) -> bool:
        return self._transaction is not None

    @property
    def root(self) -> str:
        return str(self._live().root)

    @contextlib.contextmanager
    def transaction(
        self, *, idempotency_key: str | None = None, widen: bool = False
    ) -> Iterator[Connection]:
        """A block whose statements are one change: committed when it ends,
        rolled back when it raises. ``idempotency_key`` names the change, so
        retrying it after an uncertain failure never makes it twice."""
        if self._transaction is not None:
            raise ProgrammingError("a transaction is already open; commit or roll it back first")
        self._transaction = self._live().begin(idempotency_key=idempotency_key, widen=widen)
        try:
            yield self
        except BaseException:
            self.rollback()
            raise
        else:
            self.commit()

    # --------------------------------------------------------- conveniences

    def query(self, sql: str, parameters: Parameters = None) -> list[Row]:
        """Every row a read statement answers, each a :class:`Row`."""
        columns, rows = self._read(sql, parameters)
        return [Row(zip(columns, row)) for row in rows]

    def one(self, sql: str, parameters: Parameters = None) -> Row | None:
        """The one row a read answers, None when there is none; more than one
        is an error rather than an arbitrary pick."""
        rows = self.query(sql, parameters)
        if len(rows) > 1:
            raise ProgrammingError(f"the query answered {len(rows)} rows, not one")
        return rows[0] if rows else None

    def scalar(self, sql: str, parameters: Parameters = None) -> Any:
        """The single value of a one-row, one-column answer."""
        row = self.one(sql, parameters)
        if row is None:
            return None
        if len(row) != 1:
            raise ProgrammingError(f"the query answered {len(row)} columns, not one")
        return next(iter(row.values()))

    def insert(self, table: str, rows: Mapping[str, Any] | Sequence[Mapping[str, Any]]) -> list[Row]:
        """Insert a row or rows; the rows as written, generated keys and all."""
        done = self._change(lambda target: target.insert(table, rows))
        return [Row(row) for row in done["returning"]]

    def update(self, table: str, key: Any, patch: Mapping[str, Any]) -> Row:
        """Set columns of the row ``key`` names; the row as written."""
        done = self._change(lambda target: target.update(table, key, patch))
        return Row(done["returning"][0])

    def delete(self, table: str, key: Any) -> int:
        """Delete the row ``key`` names, with its referrers' actions; how many
        rows were removed or changed in all."""
        return self._change(lambda target: target.delete(table, key))["rows"]

    def get(self, table: str, key: Any) -> Row | None:
        """The row ``key`` names, or None. A composite key is a list."""
        if self._transaction is not None:
            raise ProgrammingError("get reads committed rows; query within the transaction instead")
        row = self._live().get(table, key)
        return None if row is None else Row(row)

    def tables(self) -> list[dict]:
        return self._live().tables()

    def check(self) -> dict:
        """Validate everything: ``{"valid", "errors", "warnings"}``."""
        return self._live().check()

    def state(self) -> str:
        """A name for the state the database holds now."""
        return self._live().state()

    # ------------------------------------------------------------- internals

    def _live(self) -> _Database:
        if self._database is None:
            raise ProgrammingError("the connection is closed")
        return self._database

    def _read(self, sql: str, parameters: Parameters) -> tuple[list[str], list[tuple]]:
        if self._transaction is not None:
            return self._transaction.query(sql, parameters)
        return self._live().query(sql, parameters)

    def _target(self) -> _Transaction:
        """The transaction a change goes into, begun if none is open."""
        if self._transaction is None:
            self._transaction = self._live().begin()
        return self._transaction

    def _change(self, apply: Callable[[Any], dict]) -> dict:
        if self._transaction is None and self.autocommit:
            return apply(self._live())
        return apply(self._target())


class Cursor:
    """A PEP 249 cursor. Rows are tuples, or what the connection's
    ``row_factory`` makes of them."""

    arraysize = 1

    def __init__(self, connection: Connection) -> None:
        self.connection: Connection | None = connection
        self.description: tuple | None = None
        self.rowcount = -1
        self.lastrowid = None
        self._rows: list[tuple] = []
        self._at = 0

    def _live(self) -> Connection:
        if self.connection is None:
            raise ProgrammingError("the cursor is closed")
        self.connection._live()
        return self.connection

    def _answer(self, columns: Sequence[str], rows: list[tuple]) -> None:
        self.description = tuple((name, None, None, None, None, None, None) for name in columns)
        self._rows = rows
        self._at = 0

    def execute(self, sql: str, parameters: Parameters = None) -> Cursor:
        connection = self._live()
        if connection._live().kind(sql) == "read":
            columns, rows = connection._read(sql, parameters)
            self._answer(columns, rows)
            self.rowcount = -1
            return self
        if connection._transaction is None and connection.autocommit:
            done = connection._live().execute(sql, parameters)
        else:
            done = connection._target().execute(sql, parameters)
        self.rowcount = done["rows"] - done["induced"]
        returning = done["returning"]
        if returning:
            columns = list(returning[0])
            self._answer(columns, [tuple(row.get(column) for column in columns) for row in returning])
        else:
            self.description = None
            self._rows, self._at = [], 0
        return self

    def executemany(self, sql: str, seq_of_parameters: Sequence[Parameters]) -> Cursor:
        """Run a change once per set of parameters, all in one transaction."""
        connection = self._live()
        if connection._live().kind(sql) == "read":
            raise ProgrammingError("executemany runs a change; a read is executed once")
        own = connection._transaction is None and connection.autocommit
        target = connection._live().begin() if own else connection._target()
        total = 0
        try:
            for parameters in seq_of_parameters:
                done = target.execute(sql, parameters)
                total += done["rows"] - done["induced"]
        except BaseException:
            if own:
                target.rollback()
            raise
        if own:
            target.commit()
        self.rowcount = total
        self.description = None
        self._rows, self._at = [], 0
        return self

    def _shape(self, row: tuple) -> Any:
        factory = self.connection.row_factory if self.connection is not None else None
        return row if factory is None else factory(self, row)

    def fetchone(self) -> Any:
        self._live()
        if self._at >= len(self._rows):
            return None
        row = self._rows[self._at]
        self._at += 1
        return self._shape(row)

    def fetchmany(self, size: int | None = None) -> list[Any]:
        self._live()
        size = self.arraysize if size is None else size
        rows = self._rows[self._at : self._at + size]
        self._at += len(rows)
        return [self._shape(row) for row in rows]

    def fetchall(self) -> list[Any]:
        self._live()
        rows = self._rows[self._at :]
        self._at = len(self._rows)
        return [self._shape(row) for row in rows]

    def __iter__(self) -> Iterator[Any]:
        return self

    def __next__(self) -> Any:
        row = self.fetchone()
        if row is None:
            raise StopIteration
        return row

    def close(self) -> None:
        self.connection = None
        self._rows = []

    def setinputsizes(self, sizes: Any) -> None:
        """Nothing to prepare: values bind as JSON."""

    def setoutputsize(self, size: Any, column: Any = None) -> None:
        """Nothing to prepare: values come back as JSON."""
