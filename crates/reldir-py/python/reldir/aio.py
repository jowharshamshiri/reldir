"""asyncio access: each call runs on a worker thread, so the event loop
never waits on the filesystem.

A transaction runs whole on one worker thread -- it holds the writer lock,
and belongs to the thread that began it::

    db = await reldir.aio.connect("data")
    rows = await db.query("SELECT * FROM users")

    def transfer(tx):
        tx.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'")
        tx.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'")

    await db.run_transaction(transfer)
"""

from __future__ import annotations

import asyncio
from collections.abc import Mapping, Sequence
from typing import Any, Callable, TypeVar

import reldir

T = TypeVar("T")


async def connect(path: str | Any, **options: Any) -> AsyncConnection:
    options.pop("autocommit", None)
    database = await asyncio.to_thread(reldir._Database, path, **options)
    return AsyncConnection(database)


class AsyncConnection:
    """Changes made through it commit one by one; :meth:`run_transaction`
    makes several one change."""

    def __init__(self, database: Any) -> None:
        self._database = database

    def _connection(self) -> reldir.Connection:
        return reldir.Connection(self._database, autocommit=True)

    async def query(self, sql: str, parameters: Any = None) -> list[reldir.Row]:
        return await asyncio.to_thread(self._connection().query, sql, parameters)

    async def one(self, sql: str, parameters: Any = None) -> reldir.Row | None:
        return await asyncio.to_thread(self._connection().one, sql, parameters)

    async def scalar(self, sql: str, parameters: Any = None) -> Any:
        return await asyncio.to_thread(self._connection().scalar, sql, parameters)

    async def execute(self, sql: str, parameters: Any = None) -> int:
        """Run a change and commit it; how many rows it changed."""

        def run() -> int:
            return self._connection().execute(sql, parameters).rowcount

        return await asyncio.to_thread(run)

    async def insert(
        self, table: str, rows: Mapping[str, Any] | Sequence[Mapping[str, Any]]
    ) -> list[reldir.Row]:
        return await asyncio.to_thread(self._connection().insert, table, rows)

    async def check(self) -> dict:
        return await asyncio.to_thread(self._database.check)

    async def run_transaction(
        self,
        work: Callable[[reldir.Connection], T],
        *,
        idempotency_key: str | None = None,
    ) -> T:
        """Run ``work`` with a connection whose statements are one change,
        committed when ``work`` returns and rolled back when it raises."""

        def run() -> T:
            connection = reldir.Connection(self._database)
            with connection.transaction(idempotency_key=idempotency_key):
                return work(connection)

        return await asyncio.to_thread(run)
