"""PEP 249, as an application uses it."""

from __future__ import annotations

import json
import threading
from decimal import Decimal
from pathlib import Path

import pytest

import reldir


def test_module_globals_are_pep_249() -> None:
    assert reldir.apilevel == "2.0"
    assert reldir.threadsafety == 1
    assert reldir.paramstyle == "qmark"
    for name in ["Warning", "Error", "InterfaceError", "DatabaseError", "DataError",
                 "OperationalError", "IntegrityError", "InternalError", "ProgrammingError",
                 "NotSupportedError"]:
        assert issubclass(getattr(reldir, name), Exception)
    assert issubclass(reldir.IntegrityError, reldir.DatabaseError)
    assert issubclass(reldir.DatabaseError, reldir.Error)
    assert issubclass(reldir.InterfaceError, reldir.Error)


def test_a_cursor_reads_with_a_description_and_fetches_in_steps(db: reldir.Connection) -> None:
    cursor = db.cursor()
    assert cursor.execute("SELECT id, name FROM users ORDER BY id") is cursor
    assert [column[0] for column in cursor.description] == ["id", "name"]
    assert all(len(column) == 7 for column in cursor.description)
    assert cursor.rowcount == -1
    assert cursor.fetchone() == ("ada", "Ada")
    assert cursor.fetchmany(5) == [("bob", "Bob")]
    assert cursor.fetchone() is None
    assert cursor.fetchall() == []
    cursor.execute("SELECT id FROM users WHERE age > ?", [99])
    assert cursor.description == (("id", None, None, None, None, None, None),)
    assert list(cursor) == []


def test_a_change_opens_a_transaction_that_commit_ends(db: reldir.Connection, folder: Path) -> None:
    cursor = db.execute("UPDATE users SET age = age + 1 WHERE id = ?", ["ada"])
    assert cursor.rowcount == 1
    assert db.in_transaction
    # The transaction reads its own write; the file is not written yet.
    assert db.scalar("SELECT age FROM users WHERE id = 'ada'") == 37
    assert json.loads((folder / "users/ada.json").read_text())["age"] == 36
    done = db.commit()
    assert done["revision"] is not None
    assert not db.in_transaction
    assert json.loads((folder / "users/ada.json").read_text())["age"] == 37


def test_rollback_and_close_write_nothing(db: reldir.Connection, folder: Path) -> None:
    before = (folder / "users/ada.json").read_bytes()
    db.execute("DELETE FROM posts")
    db.execute("UPDATE users SET name = 'X'")
    db.rollback()
    db.execute("UPDATE users SET name = 'Y'")
    db.close()
    assert (folder / "users/ada.json").read_bytes() == before
    assert (folder / "posts/p1.json").exists()
    with pytest.raises(reldir.ProgrammingError):
        db.cursor().execute("SELECT 1")


def test_the_context_manager_commits_or_rolls_back(folder: Path) -> None:
    with reldir.connect(folder) as db:
        db.execute("UPDATE users SET name = 'Augusta' WHERE id = 'ada'")
    with pytest.raises(ZeroDivisionError), reldir.connect(folder) as db:
        db.execute("UPDATE users SET name = 'Lost' WHERE id = 'ada'")
        1 / 0
    assert json.loads((folder / "users/ada.json").read_text())["name"] == "Augusta"


def test_a_transaction_is_judged_whole_when_it_commits(db: reldir.Connection, folder: Path) -> None:
    # The post names a user inserted after it: valid when it commits.
    with db.transaction():
        db.execute("INSERT INTO posts (id, user_id) VALUES ('p2', 'cy')")
        db.execute("INSERT INTO users (id, name) VALUES ('cy', 'Cy')")
    assert (folder / "posts/p2.json").exists()
    db.execute("INSERT INTO posts (id, user_id) VALUES ('p3', 'nobody')")
    with pytest.raises(reldir.IntegrityError) as raised:
        db.commit()
    assert raised.value.code == "FOREIGN_KEY_VIOLATION"
    assert raised.value.diagnostics and raised.value.diagnostics[0]["code"] == "FOREIGN_KEY_VIOLATION"
    assert not (folder / "posts/p3.json").exists()


def test_executemany_is_one_change(folder: Path) -> None:
    db = reldir.connect(folder, autocommit=True)
    with pytest.raises(reldir.Error):
        db.executemany(
            "INSERT INTO users (id, name, age) VALUES (?, ?, ?)",
            [("dee", "Dee", 30), ("eve", "Eve", "old")],
        )
    assert not (folder / "users/dee.json").exists()
    cursor = db.executemany(
        "INSERT INTO users (id, name, age) VALUES (:id, :name, :age)",
        [{"id": "dee", "name": "Dee", "age": 30}, {"id": "eve", "name": "Eve", "age": 31}],
    )
    assert cursor.rowcount == 2
    assert (folder / "users/eve.json").exists()


def test_autocommit_commits_each_change(folder: Path) -> None:
    db = reldir.connect(folder, autocommit=True)
    db.execute("UPDATE users SET age = 50 WHERE id = 'bob'")
    assert not db.in_transaction
    assert json.loads((folder / "users/bob.json").read_text())["age"] == 50


def test_returning_and_inserted_rows_come_back(db: reldir.Connection) -> None:
    cursor = db.execute("UPDATE users SET age = age + 1 WHERE id = 'bob' RETURNING id, age")
    assert [column[0] for column in cursor.description] == ["id", "age"]
    assert cursor.fetchall() == [("bob", 42)]
    rows = db.insert("tickets", [{"what": "a"}, {"what": "b"}])
    assert [row["n"] for row in rows] == [1, 2]
    db.commit()
    assert db.get("tickets", 2)["what"] == "b"
    assert db.update("users", "ada", {"name": "Augusta"})["name"] == "Augusta"
    assert db.delete("users", "ada") == 2
    db.commit()
    assert db.get("users", "ada") is None


def test_values_cross_exactly(db: reldir.Connection, folder: Path) -> None:
    big = 12345678901234567890123
    db.insert("amounts", {"id": "x", "big": big, "price": "19.99"})
    db.commit()
    text = (folder / "amounts/x.json").read_text()
    assert "12345678901234567890123" in text and '"19.99"' in text
    # A decimal is written in its canonical form, or refused.
    db.insert("amounts", {"id": "y", "price": "19.990"})
    with pytest.raises(reldir.DataError) as raised:
        db.commit()
    assert raised.value.code == "TYPE_MISMATCH"
    row = db.one("SELECT big, price FROM amounts WHERE id = 'x'")
    assert row["big"] == big and isinstance(row["big"], int)
    assert db.scalar("SELECT count(*) FROM amounts WHERE big = ?", [big]) == 1
    assert db.scalar("SELECT count(*) FROM users WHERE age > ?", [Decimal("35.5")]) == 2
    with pytest.raises(ValueError):
        db.query("SELECT ?", [float("nan")])
    with pytest.raises(TypeError):
        db.query("SELECT ?", [object()])


def test_errors_are_the_pep_249_kind_with_their_code(db: reldir.Connection) -> None:
    with pytest.raises(reldir.ProgrammingError) as raised:
        db.query("SELECT * FROM nowhere")
    assert raised.value.code == "UNKNOWN_TABLE"
    with pytest.raises(reldir.ProgrammingError):
        db.query("SELEC 1")
    with pytest.raises(reldir.NotSupportedError):
        db.execute("BEGIN")
    with pytest.raises(reldir.ProgrammingError):
        db.one("SELECT id FROM users")


def test_a_readonly_connection_changes_nothing(folder: Path) -> None:
    db = reldir.connect(folder, readonly=True)
    assert db.scalar("SELECT count(*) FROM users") == 2
    with pytest.raises(reldir.OperationalError) as raised:
        db.execute("DELETE FROM posts")
    assert raised.value.code == "READ_ONLY"
    assert not (folder / ".db").exists()


def test_another_writer_waits_for_the_transaction(folder: Path) -> None:
    first = reldir.connect(folder)
    first.execute("UPDATE users SET age = 1 WHERE id = 'ada'")
    second = reldir.connect(folder, wait=0.2, autocommit=True)
    with pytest.raises(reldir.OperationalError) as raised:
        second.execute("UPDATE users SET age = 2 WHERE id = 'bob'")
    assert raised.value.code == "LOCK_CONTENDED"
    # A read never waits: it answers from the last commit.
    assert second.scalar("SELECT age FROM users WHERE id = 'ada'") == 36
    first.commit()
    second.execute("UPDATE users SET age = 2 WHERE id = 'bob'")


def test_threads_share_the_module_with_a_connection_each(folder: Path) -> None:
    reldir.connect(folder).close()
    results: list[int] = []
    errors: list[BaseException] = []

    def work() -> None:
        try:
            db = reldir.connect(folder, wait=30, autocommit=True)
            db.execute("UPDATE users SET age = age + 1 WHERE id = 'ada'")
            results.append(1)
        except BaseException as error:  # noqa: BLE001 -- reported below
            errors.append(error)

    threads = [threading.Thread(target=work) for _ in range(6)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert not errors, errors
    assert reldir.connect(folder).scalar("SELECT age FROM users WHERE id = 'ada'") == 42


def test_an_idempotent_transaction_is_made_once(folder: Path) -> None:
    db = reldir.connect(folder)
    for _ in range(2):
        with db.transaction(idempotency_key="birthday-ada"):
            db.execute("UPDATE users SET age = age + 1 WHERE id = 'ada'")
    assert db.scalar("SELECT age FROM users WHERE id = 'ada'") == 37


def test_row_factory_and_check(db: reldir.Connection) -> None:
    db.row_factory = reldir.dict_factory
    row = db.execute("SELECT id, name FROM users ORDER BY id").fetchone()
    assert row == {"id": "ada", "name": "Ada"}
    report = db.check()
    assert report["valid"] is True and report["errors"] == []
    assert [table["name"] for table in db.tables()] == ["amounts", "posts", "tickets", "users"]
