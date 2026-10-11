"""A small ledger over reldir: accounts, orders and an audit trail.

Every operation is one transaction, so a transfer moves money from one
account to another or does not happen at all, and the audit trail is written
in the same change as the balances it explains. A balance cannot go below
zero: the table's CHECK says so, and reldir refuses any change that would
break it.

    python examples/ledger.py /tmp/ledger
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import reldir

DIALECT = "https://reldir.dev/schema/reldir-3"


def keyed(table: str) -> dict:
    return {"table": table, "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}


def create(root: Path, accounts: dict[str, int]) -> None:
    """Declare the tables and open the accounts."""
    account = keyed("accounts")
    account["checks"] = [{"name": "never_negative", "expr": "balance >= 0"}]
    order = keyed("orders")
    order["generated"] = {"id": "uuid", "placed": "now"}
    order["foreignKeys"] = [{"from": ["account_id"], "to": {"table": "accounts"}, "onDelete": "restrict"}]
    audit = keyed("audit")
    audit["generated"] = {"id": "uuid", "at": "now"}
    audit["foreignKeys"] = [{"from": ["account_id"], "to": {"table": "accounts"}, "onDelete": "restrict"}]
    schemas = {
        "accounts": {
            "properties": {"id": {"type": "string"}, "balance": {"type": "integer", "x-reldir-type": "int"}},
            "required": ["id", "balance"],
            "x-reldir": account,
        },
        "orders": {
            "properties": {
                "id": {"type": "string", "format": "uuid"},
                "account_id": {"type": "string"},
                "amount": {"type": "integer", "x-reldir-type": "int", "minimum": 1},
                "placed": {"type": "string", "format": "date-time"},
            },
            "required": ["id", "account_id", "amount", "placed"],
            "x-reldir": order,
        },
        "audit": {
            "properties": {
                "id": {"type": "string", "format": "uuid"},
                "account_id": {"type": "string"},
                "delta": {"type": "integer", "x-reldir-type": "int"},
                "reason": {"type": "string"},
                "at": {"type": "string", "format": "date-time"},
            },
            "required": ["id", "account_id", "delta", "reason", "at"],
            "x-reldir": audit,
        },
    }
    for table, body in schemas.items():
        document = {"$schema": DIALECT, "type": "object", "additionalProperties": False, **body}
        path = root / "schema" / f"{table}.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(document, indent=2) + "\n")
    (root / "accounts").mkdir(exist_ok=True)
    with reldir.connect(root) as db:
        db.insert("accounts", [{"id": name, "balance": balance} for name, balance in accounts.items()])


class Ledger:
    def __init__(self, root: Path, *, wait: float = 30) -> None:
        self.db = reldir.connect(root, wait=wait)

    def balance(self, account: str) -> int:
        balance = self.db.scalar("SELECT balance FROM accounts WHERE id = ?", [account])
        if balance is None:
            raise KeyError(account)
        return balance

    def total(self) -> int:
        return self.db.scalar("SELECT sum(balance) FROM accounts") or 0

    def audited(self) -> int:
        return self.db.scalar("SELECT sum(delta) FROM audit") or 0

    def transfer(self, source: str, target: str, amount: int) -> None:
        """Move money, or raise and move nothing: a balance that would go
        below zero is an IntegrityError from the CHECK."""
        with self.db.transaction():
            for account, delta in ((source, -amount), (target, amount)):
                self.db.execute("UPDATE accounts SET balance = balance + ? WHERE id = ?", [delta, account])
                self.db.insert("audit", {"account_id": account, "delta": delta, "reason": f"transfer {source}->{target}"})

    def place_order(self, account: str, amount: int) -> str:
        """Debit the account and record the order, as one change."""
        with self.db.transaction():
            self.db.execute("UPDATE accounts SET balance = balance - ? WHERE id = ?", [amount, account])
            self.db.insert("audit", {"account_id": account, "delta": -amount, "reason": "order"})
            order = self.db.insert("orders", {"account_id": account, "amount": amount})
        return order[0]["id"]

    def close(self) -> None:
        self.db.close()


if __name__ == "__main__":
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "ledger")
    root.mkdir(parents=True, exist_ok=True)
    if not (root / "schema").exists():
        create(root, {"alice": 100, "bob": 50})
    ledger = Ledger(root)
    ledger.transfer("alice", "bob", 30)
    print("alice", ledger.balance("alice"), "bob", ledger.balance("bob"), "total", ledger.total())
    try:
        ledger.transfer("bob", "alice", 10_000)
    except reldir.IntegrityError as refused:
        print("refused:", refused.code)
