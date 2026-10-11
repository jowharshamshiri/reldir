"""The ledger example under concurrent use: transfers race on threads with a
connection each, and money is neither lost nor made."""

from __future__ import annotations

import importlib.util
import random
import threading
from pathlib import Path

import pytest

import reldir

spec = importlib.util.spec_from_file_location("ledger", Path(__file__).parents[1] / "examples" / "ledger.py")
ledger = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ledger)


def test_a_refused_transfer_moves_nothing(tmp_path: Path) -> None:
    ledger.create(tmp_path, {"alice": 100, "bob": 50})
    book = ledger.Ledger(tmp_path)
    book.transfer("alice", "bob", 30)
    assert (book.balance("alice"), book.balance("bob")) == (70, 80)
    with pytest.raises(reldir.IntegrityError) as refused:
        book.transfer("bob", "alice", 81)
    assert refused.value.code == "CHECK_VIOLATION"
    assert (book.balance("alice"), book.balance("bob")) == (70, 80)
    assert book.audited() == 0
    book.place_order("alice", 20)
    assert book.balance("alice") == 50
    assert book.db.scalar("SELECT count(*) FROM orders") == 1


def test_concurrent_transfers_conserve_money(tmp_path: Path) -> None:
    accounts = {f"a{n}": 100 for n in range(5)}
    ledger.create(tmp_path, accounts)
    total = sum(accounts.values())
    errors: list[BaseException] = []
    refused = []

    def worker(seed: int) -> None:
        chance = random.Random(seed)
        book = ledger.Ledger(tmp_path)
        try:
            for _ in range(8):
                source, target = chance.sample(sorted(accounts), 2)
                try:
                    book.transfer(source, target, chance.randint(1, 60))
                except reldir.IntegrityError:
                    refused.append(1)
        except BaseException as error:  # noqa: BLE001 -- reported below
            errors.append(error)
        finally:
            book.close()

    threads = [threading.Thread(target=worker, args=(seed,)) for seed in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert not errors, errors
    book = ledger.Ledger(tmp_path)
    assert book.total() == total, "money was lost or made"
    assert book.audited() == 0, "every transfer is audited in the same change"
    assert all(book.balance(name) >= 0 for name in accounts)
    assert book.db.check()["valid"]
    successful = book.db.scalar("SELECT count(*) FROM audit") // 2
    assert successful + len(refused) == 32
