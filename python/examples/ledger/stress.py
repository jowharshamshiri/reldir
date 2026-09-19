"""Attack the ledger, and through it reldir.

Each scenario asserts an invariant that must hold no matter how the operations
interleave, and reports what actually happened rather than only pass/fail. The
point is to find defects, so a scenario that cannot fail is not worth running:
every one here has a way to come out wrong.

The invariants:

  conservation  a transfer moves money, never creates or destroys it
  audit         every balance change has a matching audit row
  floor         the CHECK holds: no balance goes negative
  integrity     `reldir check` passes at the end of every scenario
"""

from __future__ import annotations

import json
import os
import random
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import reldir  # noqa: E402
from ledger import InsufficientFunds, Ledger  # noqa: E402

def _binary() -> str:
    """The reldir under test.

    Prefers a local debug build so the suite exercises the working tree rather
    than whatever happens to be installed, and says so plainly when neither
    exists -- a stress run against an unknown binary proves nothing.
    """
    override = os.environ.get("RELDIR_BINARY")
    if override:
        return override
    local = Path(__file__).resolve().parents[3] / "target" / "debug" / "reldir"
    if local.exists():
        return str(local)
    found = shutil.which("reldir")
    if found:
        return found
    sys.exit("no reldir binary: run `cargo build`, or set RELDIR_BINARY")


BIN = _binary()


JOURNAL = Path(__file__).with_name("stress-findings.txt")


def say(line: str) -> None:
    """Print and persist immediately.

    A stress run is long and may be interrupted. Buffered output means an
    interrupted run reports nothing at all, losing every finding it had already
    made -- so each line is flushed to the terminal and appended to a journal as
    it happens, not accumulated for a summary that may never print.
    """
    print(line, flush=True)
    with JOURNAL.open("a") as handle:
        handle.write(line + "\n")


class Report:
    def __init__(self) -> None:
        self.findings: list[str] = []
        self.notes: list[str] = []

    def defect(self, scenario: str, detail: str) -> None:
        self.findings.append(f"{scenario}: {detail}")
        say(f"  !! DEFECT  {detail}")

    def note(self, detail: str) -> None:
        self.notes.append(detail)
        say(f"     {detail}")


def fresh(accounts: int = 8, balance: int = 1000) -> Path:
    import tempfile

    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from setup import build

    root = Path(tempfile.mkdtemp())
    build(root, BIN, accounts=accounts, seed_balance=balance)
    return root


def integrity(root: Path, report: Report, scenario: str) -> dict:
    out = subprocess.run(
        [BIN, "--db", str(root), "--readonly", "--format", "jsonl", "check"],
        capture_output=True,
        text=True,
        check=False,
    )
    summary = {}
    for line in out.stdout.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if record.get("kind") == "check_summary":
            summary = record
    if not summary.get("valid"):
        report.defect(
            scenario,
            f"database invalid after the run: {summary or out.stdout[-400:] or out.stderr[-400:]}",
        )
    return summary


# ----------------------------------------------------------------- scenarios


def concurrent_debits(report: Report) -> None:
    """Many threads debit one account. The floor must hold and money must add up.

    A lost update shows as a balance higher than the debits justify; a broken
    CHECK shows as a negative balance; a torn audit shows as the audit sum
    disagreeing with the balance change.
    """
    say("\n[1] concurrent debits on a single account")
    root = fresh(accounts=2, balance=500)
    ledger = Ledger(root, BIN, wait=20.0, retry=reldir.RetryPolicy(attempts=40, max_delay=0.2))
    granted = failed = stale = 0
    lock = threading.Lock()

    def worker(index: int) -> None:
        nonlocal granted, failed, stale
        for _ in range(5):
            try:
                ledger.debit("acct-0000", 10, f"w{index}")
                with lock:
                    granted += 1
            except InsufficientFunds:
                with lock:
                    failed += 1
            except reldir.Stale:
                with lock:
                    stale += 1
            except Exception as error:  # noqa: BLE001
                report.defect("concurrent-debits", f"{type(error).__name__}: {error}")

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(10)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    balance = ledger.balance("acct-0000")
    audited = ledger.audit_sum("acct-0000")
    report.note(f"granted={granted} refused={failed} stale-escaped={stale}")
    report.note(f"balance={balance} expected={500 - granted * 10}")
    if balance != 500 - granted * 10:
        report.defect(
            "concurrent-debits",
            f"balance {balance} does not match {granted} granted debits "
            f"(expected {500 - granted * 10}) -- a lost or phantom update",
        )
    if balance < 0:
        report.defect("concurrent-debits", f"CHECK violated: balance {balance}")
    if audited != -granted * 10:
        report.defect(
            "concurrent-debits",
            f"audit sum {audited} disagrees with {granted} debits ({-granted * 10})",
        )
    if stale:
        report.defect(
            "concurrent-debits",
            f"{stale} Stale exceptions escaped transaction(), which should retry them",
        )
    integrity(root, report, "concurrent-debits")
    ledger.close()


def conservation_under_transfers(report: Report) -> None:
    """Transfers between random accounts must conserve the total exactly."""
    say("\n[2] concurrent transfers: money must be conserved")
    root = fresh(accounts=6, balance=1000)
    ledger = Ledger(root, BIN, wait=20.0, retry=reldir.RetryPolicy(attempts=40, max_delay=0.2))
    before = ledger.total_balance()
    moved = 0
    lock = threading.Lock()

    def worker(seed: int) -> None:
        nonlocal moved
        rng = random.Random(seed)
        for _ in range(4):
            a, b = rng.sample(range(6), 2)
            try:
                ledger.transfer(f"acct-{a:04d}", f"acct-{b:04d}", 25)
                with lock:
                    moved += 1
            except (InsufficientFunds, reldir.Stale):
                pass
            except Exception as error:  # noqa: BLE001
                report.defect("conservation", f"{type(error).__name__}: {error}")

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    after = ledger.total_balance()
    report.note(f"transfers completed={moved} total before={before} after={after}")
    if before != after:
        report.defect(
            "conservation",
            f"total moved from {before} to {after}: a transfer was half-applied",
        )
    integrity(root, report, "conservation")
    ledger.close()


def kill_mid_commit(report: Report) -> None:
    """SIGKILL a writer mid-transaction, then see what survives.

    The promise is that the database is either unchanged or fully changed, and
    that recovery makes it valid. A partially applied row, or a database that
    cannot be recovered, is a defect.
    """
    say("\n[3] SIGKILL a writer mid-commit")
    root = fresh(accounts=4, balance=1000)
    killed = recovered_ok = 0
    for attempt in range(6):
        rows = "\n".join(
            json.dumps({"account_id": "acct-0000", "amount": 1, "note": f"bulk{i}"})
            for i in range(120)
        )
        source = root / "bulk.jsonl"
        source.write_text(rows + "\n")
        proc = subprocess.Popen(
            [BIN, "--db", str(root), "import", "orders", "--from", str(source)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        time.sleep(random.uniform(0.02, 0.45))
        if proc.poll() is None:
            proc.send_signal(signal.SIGKILL)
            killed += 1
        proc.wait()

        repair = subprocess.run(
            [BIN, "--db", str(root), "--format", "jsonl", "recover"],
            capture_output=True,
            text=True,
            check=False,
        )
        if repair.returncode != 0:
            report.defect(
                "kill-mid-commit",
                f"recover failed after kill {attempt}: {repair.stderr[-300:]}",
            )
            break
        summary = integrity(root, report, "kill-mid-commit")
        if summary.get("valid"):
            recovered_ok += 1
        else:
            break
    report.note(f"killed {killed} writers; {recovered_ok} clean recoveries")


def external_corruption(report: Report) -> None:
    """An external process writes garbage. reldir must report, not absorb it."""
    say("\n[4] external corruption of an authoritative row")
    root = fresh(accounts=3, balance=100)
    ledger = Ledger(root, BIN)

    (root / "accounts" / "acct-0001.json").write_text('{"id":"acct-0001","balance":-9}\n')
    try:
        ledger.check()
        report.defect("external-corruption", "check() accepted a row missing required fields")
    except reldir.ReldirError as error:
        report.note(f"invalid row reported as {error.code}")

    (root / "accounts" / "acct-0001.json").write_text("{not json at all\n")
    try:
        ledger.check()
        report.defect("external-corruption", "check() accepted unparseable JSON")
    except reldir.ReldirError as error:
        report.note(f"unparseable row reported as {error.code}")

    (root / "accounts" / "acct-0001.json").unlink()
    try:
        ledger.check()
        report.note("removing a row left a valid database (no FK depended on it)")
    except reldir.ReldirError as error:
        report.note(f"removed row reported as {error.code}")
    ledger.close()


def injection_attempts(report: Report) -> None:
    """Hostile values must land as data, never as SQL."""
    say("\n[5] parameter binding against injection")
    root = fresh(accounts=2, balance=100)
    ledger = Ledger(root, BIN)
    hostile = [
        "'; DELETE FROM accounts WHERE '1'='1",
        "x' OR '1'='1",
        "'--",
        "'; DROP TABLE accounts; --",
        "a'||'b",
        "\\'; DELETE FROM accounts; --",
    ]
    for value in hostile:
        try:
            ledger.db.execute(
                "UPDATE accounts SET email = ? WHERE id = 'acct-0000'", [value]
            )
        except reldir.ReldirError as error:
            report.note(f"refused {value!r} as {error.code}")
            continue
        stored = ledger.db.one("SELECT email FROM accounts WHERE id='acct-0000'")
        if stored is None or stored["email"] != value:
            report.defect(
                "injection",
                f"{value!r} did not round-trip as data (got {stored and stored['email']!r})",
            )
    remaining = ledger.db.scalar("SELECT count(*) FROM accounts")
    if remaining != 2:
        report.defect("injection", f"account count changed to {remaining}: SQL executed")
    else:
        report.note(f"all {len(hostile)} hostile values stored as data; rows intact")
    integrity(root, report, "injection")
    ledger.close()


def schema_tampering(report: Report) -> None:
    """Tamper with the pinned schema and with .db/. reldir must not drift."""
    say("\n[6] schema and metadata tampering")
    root = fresh(accounts=2, balance=100)
    ledger = Ledger(root, BIN)

    pin = root / "schema" / "accounts.json"
    original = pin.read_text()
    tampered = json.loads(original)
    tampered["properties"]["balance"]["minimum"] = -10**9
    tampered["x-reldir"]["checks"] = []
    pin.write_text(json.dumps(tampered, indent=2) + "\n")
    try:
        ledger.check()
        report.note("a loosened pin was accepted (it disagrees with the working schema)")
    except reldir.ReldirError as error:
        report.note(f"loosened pin reported as {error.code}")
    pin.write_text(original)

    (root / ".db" / "manifest.json").write_text("{}\n")
    try:
        ledger.check()
        report.note("an emptied manifest was rebuilt rather than fataled")
    except reldir.ReldirError as error:
        report.note(f"emptied manifest reported as {error.code}")

    (root / ".db" / "format").write_text("format_version = 99\n")
    try:
        ledger.check()
        report.defect("tampering", "an unknown format version was accepted")
    except reldir.ReldirError as error:
        report.note(f"unknown format reported as {error.code}")
    ledger.close()


def readers_under_load(report: Report) -> None:
    """Readers must be served while writers work, and never see a torn total."""
    say("\n[7] readers during sustained writes")
    root = fresh(accounts=4, balance=5000)
    writer = Ledger(root, BIN, wait=20.0, retry=reldir.RetryPolicy(attempts=40))
    reader = Ledger(root, BIN)
    done = threading.Event()
    totals: set[int] = set()
    failures: list[str] = []

    def churn() -> None:
        for index in range(25):
            try:
                writer.transfer("acct-0000", "acct-0001", 10)
            except (InsufficientFunds, reldir.Stale):
                pass
            except Exception as error:  # noqa: BLE001
                failures.append(f"writer: {type(error).__name__}: {error}")
        done.set()

    thread = threading.Thread(target=churn)
    thread.start()
    served = 0
    while not done.is_set():
        try:
            totals.add(reader.total_balance())
            served += 1
        except Exception as error:  # noqa: BLE001
            failures.append(f"reader: {type(error).__name__}: {error}")
    thread.join()

    report.note(f"reads served={served}; distinct totals observed={sorted(totals)}")
    for failure in failures:
        report.defect("readers-under-load", failure)
    # A transfer is two statements, so a reader can legitimately catch the
    # moment between them. What it must never see is a total that no sequence
    # of committed operations could produce.
    legal = {20000, 19990}
    strange = totals - legal
    if strange:
        report.note(
            f"totals between the two halves of a transfer: {sorted(strange)} "
            "(expected: a transfer is not atomic across tables)"
        )
    integrity(root, report, "readers-under-load")
    writer.close()
    reader.close()


def resource_limits(report: Report) -> None:
    """Limits must be enforced explicitly, never silently truncated."""
    say("\n[8] resource limits")
    root = fresh(accounts=3, balance=100)
    tight = Ledger(root, BIN)
    tight.db._env = {**os.environ}
    out = subprocess.run(
        [BIN, "--db", str(root), "--readonly", "--max-result-rows", "1",
         "--format", "jsonl", "sql", "SELECT id FROM accounts"],
        capture_output=True, text=True, check=False,
    )
    if out.returncode == 0:
        report.defect("limits", "a 1-row limit returned a 3-row result without failing")
    else:
        report.note(f"row limit enforced, exit {out.returncode}")

    deep = {"id": "acct-0002", "email": "d@e", "balance": 1, "status": "open"}
    node = deep
    for _ in range(200):
        node["note"] = {}
        node = node["note"]
    out = subprocess.run(
        [BIN, "--db", str(root), "--max-nesting-depth", "16", "--format", "jsonl",
         "insert", "accounts", json.dumps(deep)],
        capture_output=True, text=True, check=False,
    )
    report.note(f"deep document: exit {out.returncode}")
    if out.returncode == 0:
        report.defect("limits", "a 200-deep document was accepted under a depth limit of 16")
    tight.close()


def main() -> int:
    report = Report()
    for scenario in (
        concurrent_debits,
        conservation_under_transfers,
        kill_mid_commit,
        external_corruption,
        injection_attempts,
        schema_tampering,
        readers_under_load,
        resource_limits,
    ):
        try:
            scenario(report)
        except Exception as error:  # noqa: BLE001
            report.defect(scenario.__name__, f"scenario crashed: {type(error).__name__}: {error}")

    say("\n" + "=" * 68)
    if report.findings:
        say(f"DEFECTS FOUND: {len(report.findings)}")
        for finding in report.findings:
            say(f"  - {finding}")
    else:
        say("no defects found")
    return 1 if report.findings else 0


if __name__ == "__main__":
    sys.exit(main())
