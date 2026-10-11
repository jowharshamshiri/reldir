from __future__ import annotations

import asyncio
import json
import subprocess
import sys
from pathlib import Path

import reldir
import reldir.aio


def test_asyncio_runs_on_worker_threads(folder: Path) -> None:
    async def scenario() -> tuple[int, int]:
        db = await reldir.aio.connect(folder)
        changed = await db.execute("UPDATE users SET age = 1 WHERE id = 'ada'")

        def transfer(tx: reldir.Connection) -> int:
            tx.execute("UPDATE users SET age = age + 10 WHERE id = 'ada'")
            return tx.scalar("SELECT age FROM users WHERE id = 'ada'")

        inside = await db.run_transaction(transfer)
        rows = await asyncio.gather(*(db.scalar("SELECT age FROM users WHERE id = 'ada'") for _ in range(4)))
        assert set(rows) == {11}
        return changed, inside

    assert asyncio.run(scenario()) == (1, 11)


def test_the_wheel_carries_the_command(folder: Path) -> None:
    done = subprocess.run(
        [sys.executable, "-m", "reldir", "--db", str(folder), "--format", "json", "sql", "SELECT count(*) AS n FROM users"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert done.returncode == 0, done.stderr
    envelope = json.loads(done.stdout)
    assert envelope["records"][0]["row"]["n"] == 2
    assert envelope["reldir"] == reldir.__version__


def test_pandas_reads_through_the_driver(folder: Path) -> None:
    import warnings

    import pandas

    db = reldir.connect(folder)
    with warnings.catch_warnings():
        # pandas names SQLAlchemy and sqlite3 as the connections it tests,
        # and warns about any other DB-API connection it is handed.
        warnings.filterwarnings("ignore", message="pandas only supports SQLAlchemy", category=UserWarning)
        frame = pandas.read_sql("SELECT id, age FROM users WHERE age > ? ORDER BY id", db, params=[30])
    assert list(frame.columns) == ["id", "age"]
    assert frame["age"].tolist() == [36, 41]
