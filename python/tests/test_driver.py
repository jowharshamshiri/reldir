"""The driver's contract with the binary.

These tests run real processes against real directories. Where a test asserts
something about concurrency it creates actual contention rather than simulating
it, because the failures worth catching here -- a lost update, a retry that
should not have happened, a refusal reported as the wrong kind -- only appear
when two writers really race.
"""

from __future__ import annotations

import json
import subprocess
import threading
import time
from pathlib import Path

import pytest

import reldir


# --------------------------------------------------------------------- reading


def test_query_returns_rows_in_column_order(db: reldir.Connection) -> None:
    rows = db.query("SELECT id, name FROM users ORDER BY name")
    assert [row["name"] for row in rows] == ["Alice", "Bob"]
    # A Row preserves the order the columns were selected in, which is what
    # makes `list(row)` and `row.values()` meaningful.
    assert list(rows[0]) == ["id", "name"]


def test_scalar_and_one_narrow_a_result(db: reldir.Connection) -> None:
    assert db.scalar("SELECT count(*) FROM users") == 2
    assert db.one("SELECT name FROM users WHERE id = 'u1'")["name"] == "Alice"
    assert db.one("SELECT name FROM users WHERE id = 'nobody'") is None


def test_one_refuses_to_silently_pick_a_row(db: reldir.Connection) -> None:
    # Returning the first of several would let a wrong assumption survive
    # unnoticed, which is the bug this guard exists to expose.
    with pytest.raises(reldir.QueryError) as caught:
        db.one("SELECT name FROM users")
    assert "2 rows" in caught.value.message


def test_scalar_refuses_a_multi_column_row(db: reldir.Connection) -> None:
    with pytest.raises(reldir.QueryError):
        db.scalar("SELECT id, name FROM users WHERE id = 'u1'")


def test_a_read_does_not_advance_the_revision(db: reldir.Connection) -> None:
    before = db.status()["revision"]
    db.query("SELECT * FROM users")
    assert db.status()["revision"] == before


# --------------------------------------------------------------------- writing


def test_execute_inserts_and_is_visible_immediately(db: reldir.Connection) -> None:
    db.execute("INSERT INTO users (id, name) VALUES (?, ?)", ["u3", "Carol"])
    assert db.scalar("SELECT count(*) FROM users") == 3
    assert db.one("SELECT name FROM users WHERE id = 'u3'")["name"] == "Carol"


def test_write_helpers_round_trip(db: reldir.Connection) -> None:
    db.insert("users", {"id": "u4", "name": "Dave"})
    assert db.one("SELECT name FROM users WHERE id = 'u4'")["name"] == "Dave"

    db.update("users", "u4", {"name": "David"})
    assert db.one("SELECT name FROM users WHERE id = 'u4'")["name"] == "David"

    db.delete("users", "u4")
    assert db.one("SELECT name FROM users WHERE id = 'u4'") is None


def test_a_write_lands_on_disk_as_readable_json(
    db: reldir.Connection, database: Path
) -> None:
    # The whole premise of reldir is that rows stay readable files. A driver
    # that wrote through some side channel would defeat the point.
    db.execute("INSERT INTO users (id, name) VALUES ('u9', 'Eve')")
    written = json.loads((database / "users" / "u9.json").read_text())
    assert written == {"id": "u9", "name": "Eve"}


# -------------------------------------------------------------------- binding


def test_parameters_are_quoted_not_interpolated(db: reldir.Connection) -> None:
    # The injection this prevents: a value containing a quote must land as data.
    db.execute("INSERT INTO users (id, name) VALUES (?, ?)", ["u5", "O'Brien"])
    assert db.one("SELECT name FROM users WHERE id = 'u5'")["name"] == "O'Brien"


def test_a_placeholder_inside_a_literal_is_not_a_placeholder() -> None:
    from reldir import _bind

    # Counting `?` characters would bind the one inside the string and shift
    # every later parameter, which is why the scan tracks quoting.
    assert _bind("SELECT '?' , ?", [1]) == "SELECT '?' , 1"
    assert _bind("SELECT 'it''s ?', ?", ["x"]) == "SELECT 'it''s ?', 'x'"


def test_placeholder_and_parameter_counts_must_agree() -> None:
    from reldir import _bind

    with pytest.raises(ValueError, match="more placeholders"):
        _bind("SELECT ?, ?", [1])
    with pytest.raises(ValueError, match="2 parameters"):
        _bind("SELECT ?", [1, 2])
    with pytest.raises(ValueError, match="unterminated"):
        _bind("SELECT 'open", [])


def test_literals_cover_reldir_types_and_refuse_others() -> None:
    from reldir import _literal

    assert _literal(None) == "NULL"
    assert _literal(True) == "true"
    assert _literal(False) == "false"
    assert _literal(3) == "3"
    assert _literal("a'b") == "'a''b'"
    assert json.loads(_literal({"k": 1})[1:-1]) == {"k": 1}

    import datetime

    # Coercing this through str() is how a datetime becomes an unparseable row.
    with pytest.raises(TypeError):
        _literal(datetime.datetime(2026, 1, 1))
    with pytest.raises(ValueError):
        _literal(float("nan"))


# ------------------------------------------------------------------ diagnostics


def test_an_unknown_table_raises_a_typed_error(db: reldir.Connection) -> None:
    with pytest.raises(reldir.UnknownName) as caught:
        db.query("SELECT * FROM nosuch")
    assert caught.value.code == "UNKNOWN_TABLE"
    assert caught.value.exit_code == 4


def test_a_constraint_violation_carries_its_diagnostic(db: reldir.Connection) -> None:
    with pytest.raises(reldir.ReldirError) as caught:
        # u1 already exists, so this collides with the primary key.
        db.execute("INSERT INTO users (id, name) VALUES ('u1', 'Duplicate')")
    error = caught.value
    assert error.code
    assert error.message
    # The diagnostic is carried verbatim so a caller can branch on structure
    # rather than parsing prose.
    assert isinstance(error.diagnostic, dict)


def test_unsupported_sql_is_a_query_error(db: reldir.Connection) -> None:
    with pytest.raises(reldir.QueryError):
        db.execute("CREATE TRIGGER t AFTER INSERT ON users BEGIN SELECT 1; END")


# ----------------------------------------------------------------- concurrency


def test_concurrent_writers_all_succeed_under_the_default_wait(
    database: Path, binary: str
) -> None:
    """The reason `--wait` exists.

    Eight threads write different rows at once. With a wait, they queue behind
    one another and all of them land. Without one they would race for a lock
    that only one can hold, and seven would fail.
    """
    connection = reldir.connect(database, binary=binary, wait=10.0)
    failures: list[Exception] = []

    def writer(index: int) -> None:
        try:
            connection.execute(
                "UPDATE counters SET n = ? WHERE id = ?", [index + 1, f"c{index % 4}"]
            )
        except Exception as error:  # noqa: BLE001 - recorded and asserted below
            failures.append(error)

    threads = [threading.Thread(target=writer, args=(i,)) for i in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert failures == [], f"every writer should have been served: {failures}"
    # Every value on disk must be one that was actually written, never a torn
    # or partially applied one.
    for index in range(4):
        n = json.loads((database / "counters" / f"c{index}.json").read_text())["n"]
        assert n in range(1, 9), f"c{index} holds an impossible value {n}"
    connection.close()


def test_a_zero_wait_reports_contention_rather_than_queueing(
    database: Path, binary: str
) -> None:
    """`wait=0` is a deliberate posture, not an absent setting.

    A held lock must be refused at once and named `LOCK_CONTENDED`, so a caller
    who asked for a deterministic failure gets one instead of a queue.
    """
    holder_started = threading.Event()
    release = threading.Event()

    def hold() -> None:
        # `import` takes the writer lock for the duration of a real commit.
        # Holding it from another process is the only honest way to contend.
        rows = "\n".join(
            json.dumps({"id": f"bulk{i}", "name": f"n{i}"}) for i in range(400)
        )
        source = database / "bulk.jsonl"
        source.write_text(rows + "\n")
        holder_started.set()
        subprocess.run(
            [binary, "--db", str(database), "import", "users", "--from", str(source)],
            capture_output=True,
            text=True,
            check=False,
        )
        release.set()

    thread = threading.Thread(target=hold)
    thread.start()
    holder_started.wait(timeout=10)

    impatient = reldir.connect(
        database, binary=binary, wait=0.0, retry=reldir.RetryPolicy(attempts=1)
    )
    contended = None
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and not release.is_set():
        try:
            impatient.execute("UPDATE counters SET n = 99 WHERE id = 'c0'")
        except reldir.LockContended as error:
            contended = error
            break
        except reldir.ConcurrentModification:
            # The state moved rather than the lock being held; not what this
            # test is about, so keep trying while the holder still runs.
            continue
    thread.join()
    impatient.close()

    if contended is None:
        pytest.skip("the holding import finished before contention could be observed")
    assert contended.code == "LOCK_CONTENDED"
    assert contended.exit_code == 3
    # Nothing was written, which is what makes a retry safe.
    assert "LOCK_CONTENDED" in contended.stderr or contended.message


def test_a_read_is_never_refused_for_lock_contention(
    database: Path, binary: str
) -> None:
    """Readers take no lock, so a busy writer cannot starve them.

    A reader *may* be refused while a transaction is materialising -- renames
    are half-applied then, and answering from those rows would report a state
    that never existed. What must never happen is a refusal for contention, or
    because a writer merely staged a transaction it has not begun applying.
    """
    done = threading.Event()

    def churn() -> None:
        writer = reldir.connect(database, binary=binary, wait=10.0)
        for index in range(4):
            writer.execute("UPDATE counters SET n = ? WHERE id = 'c1'", [index])
        writer.close()
        done.set()

    thread = threading.Thread(target=churn)
    thread.start()

    reader = reldir.connect(database, binary=binary)
    answers = 0
    while not done.is_set():
        # No except clause: the driver absorbs both transients itself. A
        # failure escaping here is a real defect, because a reader competing
        # with an ordinary writer must always eventually be served.
        rows = reader.query("SELECT id, name FROM users ORDER BY id")
        assert [row["id"] for row in rows][:2] == ["u1", "u2"]
        answers += 1
    thread.join()
    reader.close()
    assert answers > 0, "the reader was never served while a writer worked"


def test_a_genuinely_interrupted_transaction_still_surfaces(
    database: Path, binary: str
) -> None:
    """Retrying the transient must not bury the permanent.

    A writer that died mid-rename leaves the marker behind for good. That needs
    `reldir recover`, so it has to outlive the retry budget and reach the
    caller rather than being absorbed as though it were momentary.
    """
    interrupted = database / ".db" / "transactions" / "55555555-5555-4555-8555-555555555555"
    (interrupted / "staged").mkdir(parents=True)
    (interrupted / "journal.json").write_text(
        json.dumps(
            {
                "id": "55555555-5555-4555-8555-555555555555",
                "start_root": "x",
                "origin": "internal",
                "changes": [{"path": "users/u1.json", "stage": "00000000"}],
            }
        )
        + "\n"
    )
    (interrupted / "COMMITTING").write_text("commit\n")

    reader = reldir.connect(
        database, binary=binary, retry=reldir.RetryPolicy(attempts=3, base_delay=0.001)
    )
    with pytest.raises(reldir.RecoveryRequired):
        reader.query("SELECT id FROM users")
    reader.close()


def test_a_staged_transaction_does_not_refuse_a_reader(
    database: Path, binary: str
) -> None:
    """The common case: a write stages bytes before it renames anything.

    Every ordinary write passes through that state, so refusing readers there
    would mean any writer preparing a commit broke every concurrent query.
    """
    staged = database / ".db" / "transactions" / "11111111-1111-4111-8111-111111111111"
    (staged / "staged").mkdir(parents=True)
    (staged / "journal.json").write_text(
        json.dumps(
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "start_root": "x",
                "origin": "internal",
                "changes": [{"path": "users/u1.json", "stage": "00000000"}],
            }
        )
        + "\n"
    )

    reader = reldir.connect(database, binary=binary)
    assert len(reader.query("SELECT id FROM users")) == 2

    # Once it begins materialising, the same read must refuse rather than
    # answer from rows that are a partial application of a change.
    (staged / "COMMITTING").write_text("commit\n")
    with pytest.raises(reldir.RecoveryRequired):
        reader.query("SELECT id FROM users")
    reader.close()


def test_transaction_reruns_the_block_so_reads_are_retaken(
    database: Path, binary: str
) -> None:
    """A read-modify-write must not lose an update.

    Two processes increment the same counter. Each reads, adds one, and writes.
    If the block did not re-read on conflict, one increment would be lost and
    the total would be 1 rather than 2.
    """
    connection = reldir.connect(database, binary=binary, wait=10.0)
    barrier = threading.Barrier(2)
    errors: list[Exception] = []

    def step(tx: reldir.Connection) -> None:
        current = tx.scalar("SELECT n FROM counters WHERE id = 'c2'")
        # The predicate carries the value that was read. Without it both
        # threads read the same n, both writes succeed, and one increment is
        # silently lost -- reldir cannot detect a stale read on its own,
        # because each UPDATE is complete and valid by itself.
        result = tx.execute(
            "UPDATE counters SET n = ? WHERE id = 'c2' AND n = ?",
            [current + 1, current],
            retry_on_conflict=False,
        )
        if result.changed == 0:
            raise reldir.Stale("c2 moved between the read and the write")

    def increment() -> None:
        try:
            barrier.wait(timeout=10)
            for _ in range(3):
                connection.transaction(step, attempts=10)
        except Exception as error:  # noqa: BLE001 - asserted below
            errors.append(error)

    threads = [threading.Thread(target=increment) for _ in range(2)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert errors == [], f"increments should have completed: {errors}"
    final = json.loads((database / "counters" / "c2.json").read_text())["n"]
    assert final == 6, f"every increment must survive, got {final}"
    connection.close()


# -------------------------------------------------------------- retry policy


def test_transaction_returns_what_the_body_returns(db: reldir.Connection) -> None:
    assert db.transaction(lambda tx: tx.scalar("SELECT count(*) FROM users")) == 2


def test_transaction_refuses_a_zero_budget(db: reldir.Connection) -> None:
    with pytest.raises(ValueError, match="at least 1"):
        db.transaction(lambda tx: None, attempts=0)


def test_transaction_does_not_swallow_an_unrelated_failure(
    db: reldir.Connection,
) -> None:
    # Only contention and moved state are retryable. A genuine mistake inside
    # the body must surface on the first attempt rather than being retried into
    # a confusing delay.
    calls = []

    def body(tx: reldir.Connection) -> None:
        calls.append(1)
        tx.query("SELECT * FROM nosuch")

    with pytest.raises(reldir.UnknownName):
        db.transaction(body)
    assert calls == [1], "a non-retryable failure must not re-run the body"


def test_retry_policy_rejects_nonsense() -> None:
    with pytest.raises(ValueError):
        reldir.RetryPolicy(attempts=0)
    with pytest.raises(ValueError):
        reldir.RetryPolicy(base_delay=-1)
    with pytest.raises(ValueError):
        reldir.RetryPolicy(max_delay=float("inf"))


def test_retry_delay_is_jittered_within_its_ceiling() -> None:
    policy = reldir.RetryPolicy(base_delay=0.01, max_delay=0.1)
    draws = {policy.delay_before(3) for _ in range(200)}
    assert all(0.0 <= draw <= 0.1 for draw in draws)
    # Lockstep retries are the failure mode jitter exists to prevent, so a
    # constant delay would defeat it.
    assert len(draws) > 1


def test_a_nonretryable_failure_is_not_retried(db: reldir.Connection) -> None:
    # An unknown table will never become known by trying again. Retrying it
    # would multiply the latency of every genuine mistake.
    started = time.monotonic()
    with pytest.raises(reldir.UnknownName):
        db.execute("INSERT INTO nosuch (id) VALUES ('x')")
    assert time.monotonic() - started < 5


# ------------------------------------------------------------------ lifecycle


def test_connect_reports_a_missing_binary_clearly(database: Path) -> None:
    with pytest.raises(FileNotFoundError, match="not on PATH"):
        reldir.connect(database, binary="reldir-does-not-exist")


def test_a_negative_wait_is_refused_by_the_driver(database: Path, binary: str) -> None:
    with pytest.raises(ValueError, match="zero or greater"):
        reldir.connect(database, binary=binary, wait=-1)


def test_using_a_closed_connection_is_an_error(db: reldir.Connection) -> None:
    db.close()
    with pytest.raises(ValueError, match="closed"):
        db.query("SELECT 1")


def test_status_and_check_agree_on_a_valid_database(db: reldir.Connection) -> None:
    assert db.status()["valid"] is True
    db.check()


def test_check_raises_on_an_invalid_database(
    db: reldir.Connection, database: Path
) -> None:
    # An externally written row that violates the schema must surface as a
    # typed failure, not as a silently ignored file.
    (database / "users" / "bad.json").write_text(json.dumps({"id": 42}) + "\n")
    with pytest.raises(reldir.ReldirError):
        db.check()
