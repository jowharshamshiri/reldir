"""A small ledger application over reldir.

The operations an application actually performs: transfer money between
accounts, place an order against a balance, report. Each one is written the way
the driver's documentation says it must be -- compare-and-set carried into the
predicate, the whole block retried -- so that when the stress test finds a lost
update it is a defect in reldir or the driver, not in this app cutting a corner.
"""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, "/Users/bahram/ws/prj/reldir/python")

import reldir  # noqa: E402


class InsufficientFunds(Exception):
    """The debit would take the balance below zero."""


class Ledger:
    def __init__(self, root: Path, binary: str, **kwargs) -> None:
        self.db = reldir.connect(root, binary=binary, **kwargs)

    # ---------------------------------------------------------------- reading

    def balance(self, account: str) -> int:
        row = self.db.one("SELECT balance FROM accounts WHERE id = ?", [account])
        if row is None:
            raise KeyError(account)
        return row["balance"]

    def total_balance(self) -> int:
        return self.db.scalar("SELECT sum(balance) FROM accounts") or 0

    def audit_sum(self, account: str | None = None) -> int:
        if account is None:
            return self.db.scalar("SELECT sum(delta) FROM audit") or 0
        return (
            self.db.scalar(
                "SELECT sum(delta) FROM audit WHERE account_id = ?", [account]
            )
            or 0
        )

    def statement(self, account: str) -> list[reldir.Row]:
        return self.db.query(
            "SELECT o.id, o.amount, o.note, o.placed "
            "FROM orders o WHERE o.account_id = ? ORDER BY o.placed",
            [account],
        )

    def busiest(self, limit: int = 3) -> list[reldir.Row]:
        return self.db.query(
            "SELECT a.id, count(o.id) AS orders, sum(o.amount) AS spent "
            "FROM accounts a INNER JOIN orders o ON o.account_id = a.id "
            "GROUP BY a.id ORDER BY spent DESC LIMIT ?",
            [limit],
        )

    # --------------------------------------------------------------- writing

    def debit(self, account: str, amount: int, reason: str) -> None:
        """Take `amount` from an account, refusing to go negative.

        The read and the write are separate statements, so the predicate has to
        carry the balance that was read. Without `AND balance = ?` two
        concurrent debits both read the same figure, both succeed, and the
        account is short by one of them.
        """

        def body(tx: reldir.Connection) -> None:
            row = tx.one("SELECT balance, status FROM accounts WHERE id = ?", [account])
            if row is None:
                raise KeyError(account)
            if row["status"] != "open":
                raise InsufficientFunds(f"{account} is {row['status']}")
            if row["balance"] < amount:
                raise InsufficientFunds(
                    f"{account} holds {row['balance']}, needs {amount}"
                )
            result = tx.execute(
                "UPDATE accounts SET balance = ? WHERE id = ? AND balance = ?",
                [row["balance"] - amount, account, row["balance"]],
                retry_on_conflict=False,
            )
            if result.changed == 0:
                raise reldir.Stale(f"{account} moved between the read and the write")
            tx.insert("audit", {"account_id": account, "delta": -amount, "reason": reason})

        self.db.transaction(body)

    def credit(self, account: str, amount: int, reason: str) -> None:
        def body(tx: reldir.Connection) -> None:
            row = tx.one("SELECT balance FROM accounts WHERE id = ?", [account])
            if row is None:
                raise KeyError(account)
            result = tx.execute(
                "UPDATE accounts SET balance = ? WHERE id = ? AND balance = ?",
                [row["balance"] + amount, account, row["balance"]],
                retry_on_conflict=False,
            )
            if result.changed == 0:
                raise reldir.Stale(f"{account} moved between the read and the write")
            tx.insert("audit", {"account_id": account, "delta": amount, "reason": reason})

        self.db.transaction(body)

    def place_order(self, account: str, amount: int, note: str | None = None) -> None:
        """Debit the account and record an order, as one logical operation.

        reldir commits one statement at a time, so this is not atomic across
        both tables. The debit happens first: an order that fails to write
        leaves money taken and no order, which the stress test checks for
        rather than pretending cannot happen.
        """
        self.debit(account, amount, f"order:{note or 'unnamed'}")
        row = {"account_id": account, "amount": amount}
        if note is not None:
            row["note"] = note
        self.db.insert("orders", row)

    def transfer(self, source: str, target: str, amount: int) -> None:
        self.debit(source, amount, f"transfer->{target}")
        self.credit(target, amount, f"transfer<-{source}")

    def check(self) -> None:
        """Validate the whole database, raising if it is not valid."""
        self.db.check()

    def close(self) -> None:
        self.db.close()
