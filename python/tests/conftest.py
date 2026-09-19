"""Fixtures for the driver suite.

Every test runs the real binary against a real directory. There are no mocks:
this package's entire job is to be correct about what the binary actually does,
and a mocked subprocess would only assert that the driver agrees with my
assumptions rather than with reldir.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest

import reldir


def _binary() -> str:
    """The binary under test.

    Prefers a local debug build so the suite exercises the working tree rather
    than whatever is installed, and says so clearly when neither exists.
    """
    override = os.environ.get("RELDIR_BINARY")
    if override:
        return override
    local = Path(__file__).resolve().parents[2] / "target" / "debug" / "reldir"
    if local.exists():
        return str(local)
    found = shutil.which("reldir")
    if found:
        return found
    pytest.skip(
        "no reldir binary: build one with `cargo build` or set RELDIR_BINARY",
        allow_module_level=True,
    )


@pytest.fixture(scope="session")
def binary() -> str:
    return _binary()


@pytest.fixture
def database(tmp_path: Path, binary: str) -> Path:
    """An adopted database with two tables and a known starting state."""
    (tmp_path / "users").mkdir()
    (tmp_path / "counters").mkdir()
    (tmp_path / "users" / "u1.json").write_text(
        json.dumps({"id": "u1", "name": "Alice"}) + "\n"
    )
    (tmp_path / "users" / "u2.json").write_text(
        json.dumps({"id": "u2", "name": "Bob"}) + "\n"
    )
    for index in range(4):
        (tmp_path / "counters" / f"c{index}.json").write_text(
            json.dumps({"id": f"c{index}", "n": 0}) + "\n"
        )
    subprocess.run(
        [binary, "init", str(tmp_path), "--adopt"],
        capture_output=True,
        check=True,
        text=True,
    )
    return tmp_path


@pytest.fixture
def db(database: Path, binary: str) -> reldir.Connection:
    with reldir.connect(database, binary=binary) as connection:
        yield connection
