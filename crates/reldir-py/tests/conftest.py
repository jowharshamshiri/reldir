from __future__ import annotations

import json
from pathlib import Path

import pytest

import reldir

DIALECT = "https://reldir.dev/schema/reldir-3"


def write(root: Path, relative: str, value: object) -> None:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n")


def keyed(table: str, column: str = "id") -> dict:
    return {"table": table, "identity": {"source": "body", "columns": [column], "naming": {"rule": "reldir"}}}


@pytest.fixture
def folder(tmp_path: Path) -> Path:
    """users <- posts (cascade on delete); tickets numbered by a sequence;
    amounts with exact decimals and big integers."""
    write(tmp_path, "schema/users.json", {
        "$schema": DIALECT, "type": "object",
        "properties": {"id": {"type": "string"}, "name": {"type": "string"}, "age": {"type": "integer", "x-reldir-type": "int"}},
        "required": ["id", "name"], "additionalProperties": False, "x-reldir": keyed("users"),
    })
    posts = keyed("posts")
    posts["foreignKeys"] = [{"from": ["user_id"], "to": {"table": "users"}, "onDelete": "cascade"}]
    write(tmp_path, "schema/posts.json", {
        "$schema": DIALECT, "type": "object",
        "properties": {"id": {"type": "string"}, "user_id": {"type": "string"}},
        "required": ["id", "user_id"], "additionalProperties": False, "x-reldir": posts,
    })
    tickets = keyed("tickets", "n")
    tickets["generated"] = {"n": "sequence"}
    write(tmp_path, "schema/tickets.json", {
        "$schema": DIALECT, "type": "object",
        "properties": {"n": {"type": "integer", "x-reldir-type": "int"}, "what": {"type": "string"}},
        "required": ["n", "what"], "additionalProperties": False, "x-reldir": tickets,
    })
    write(tmp_path, "schema/amounts.json", {
        "$schema": DIALECT, "type": "object",
        "properties": {"id": {"type": "string"}, "big": {"type": "integer"}, "price": {"type": "string", "x-reldir-type": "decimal"}},
        "required": ["id"], "additionalProperties": False, "x-reldir": keyed("amounts"),
    })
    write(tmp_path, "users/ada.json", {"id": "ada", "name": "Ada", "age": 36})
    write(tmp_path, "users/bob.json", {"id": "bob", "name": "Bob", "age": 41})
    write(tmp_path, "posts/p1.json", {"id": "p1", "user_id": "ada"})
    return tmp_path


@pytest.fixture
def db(folder: Path):
    connection = reldir.connect(folder)
    yield connection
    connection.close()
