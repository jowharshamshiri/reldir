"""Create the ledger database: three tables with real constraints.

Deliberately not a toy schema. It carries a CHECK that concurrent debits can
drive negative, a unique constraint two writers can collide on, a foreign key
with `restrict` that a delete must respect, and generated columns so the app
omits ids the way an application actually would.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

ACCOUNTS = {
    "$schema": "https://reldir.dev/schema/reldir-1",
    "type": "object",
    "properties": {
        "id": {"type": "string", "pattern": "^acct-[0-9]{4}$"},
        "email": {"type": "string", "pattern": "^[^@]+@[^@]+$"},
        "balance": {"type": "integer", "x-reldir-type": "int", "minimum": 0},
        "status": {"type": "string", "enum": ["open", "frozen", "closed"]},
        "opened": {"type": "string", "format": "date-time"},
    },
    "required": ["id", "email", "balance", "status", "opened"],
    "additionalProperties": False,
    "x-reldir": {
        "table": "accounts",
        "schemaVersion": 1,
        "primaryKey": ["id"],
        "columnOrder": ["id", "email", "balance", "status", "opened"],
        "unique": [["email"]],
        "checks": [{"name": "balance_not_negative", "expr": "balance >= 0"}],
        "generated": {"opened": "now"},
    },
}

ORDERS = {
    "$schema": "https://reldir.dev/schema/reldir-1",
    "type": "object",
    "properties": {
        "id": {"type": "string", "format": "uuid"},
        "account_id": {"type": "string", "pattern": "^acct-[0-9]{4}$"},
        "amount": {"type": "integer", "x-reldir-type": "int", "minimum": 1},
        "note": {"type": ["string", "null"]},
        "placed": {"type": "string", "format": "date-time"},
    },
    "required": ["id", "account_id", "amount", "placed"],
    "additionalProperties": False,
    "x-reldir": {
        "table": "orders",
        "schemaVersion": 1,
        "primaryKey": ["id"],
        "columnOrder": ["id", "account_id", "amount", "note", "placed"],
        "indexes": [["account_id"]],
        "foreignKeys": [
            {
                "columns": ["account_id"],
                "references": {"table": "accounts", "columns": ["id"]},
                "onDelete": "restrict",
                "onUpdate": "restrict",
            }
        ],
        "generated": {"id": "uuid", "placed": "now"},
    },
}

AUDIT = {
    "$schema": "https://reldir.dev/schema/reldir-1",
    "type": "object",
    "properties": {
        "id": {"type": "string", "format": "uuid"},
        "account_id": {"type": "string", "pattern": "^acct-[0-9]{4}$"},
        "delta": {"type": "integer", "x-reldir-type": "int"},
        "reason": {"type": "string", "minLength": 1, "maxLength": 200},
        "at": {"type": "string", "format": "date-time"},
    },
    "required": ["id", "account_id", "delta", "reason", "at"],
    "additionalProperties": False,
    "x-reldir": {
        "table": "audit",
        "schemaVersion": 1,
        "primaryKey": ["id"],
        "columnOrder": ["id", "account_id", "delta", "reason", "at"],
        "indexes": [["account_id"]],
        "generated": {"id": "uuid", "at": "now"},
    },
}


def build(root: Path, binary: str, accounts: int = 8, seed_balance: int = 1000) -> None:
    for table in ("accounts", "orders", "audit"):
        (root / table).mkdir(parents=True, exist_ok=True)
    (root / "schema").mkdir(exist_ok=True)
    for name, schema in (
        ("accounts", ACCOUNTS),
        ("orders", ORDERS),
        ("audit", AUDIT),
    ):
        (root / "schema" / f"{name}.json").write_text(
            json.dumps(schema, indent=2) + "\n"
        )

    # Seed rows are written as files rather than inserted, so the database is
    # adopted from data that already exists -- the way a real folder arrives.
    for index in range(accounts):
        (root / "accounts" / f"acct-{index:04d}.json").write_text(
            json.dumps(
                {
                    "id": f"acct-{index:04d}",
                    "email": f"holder{index}@example.test",
                    "balance": seed_balance,
                    "status": "open",
                    "opened": "2026-01-01T00:00:00Z",
                },
                indent=2,
            )
            + "\n"
        )

    result = subprocess.run(
        [binary, "init", str(root), "--adopt"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        sys.exit(f"init failed: {result.stdout}\n{result.stderr}")


if __name__ == "__main__":
    build(Path(sys.argv[1]), sys.argv[2] if len(sys.argv) > 2 else "reldir")
    print("ledger built")
