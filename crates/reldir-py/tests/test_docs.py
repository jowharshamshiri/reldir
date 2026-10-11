"""The Python page of the documentation is executed.

Every ```python block of docs/python.md runs, in order, in one namespace,
in a folder holding the database the examples use (`data/`, copied from
crates/reldir/tests/docs/snippets/). Blocks that state a signature rather
than run are written as ```text and are not run.
"""

from __future__ import annotations

import os
import re
import shutil
import warnings
from pathlib import Path

REPOSITORY = Path(__file__).resolve().parents[3]


def blocks(page: Path, language: str) -> list[str]:
    return re.findall(rf"^```{language}\n(.*?)^```", page.read_text(), flags=re.S | re.M)


def test_every_python_example_runs(tmp_path: Path) -> None:
    shutil.copytree(REPOSITORY / "crates/reldir/tests/docs/snippets/data", tmp_path / "data")
    examples = blocks(REPOSITORY / "docs/python.md", "python")
    assert len(examples) >= 8, "the page shows its interface working"
    namespace: dict = {}
    previous = os.getcwd()
    os.chdir(tmp_path)
    try:
        for index, source in enumerate(examples, start=1):
            with warnings.catch_warnings():
                # pandas warns about any DB-API connection that is not sqlite3's.
                warnings.simplefilter("ignore", UserWarning)
                try:
                    exec(compile(source, f"docs/python.md example {index}", "exec"), namespace)
                except Exception as error:  # noqa: BLE001 -- reported with its example
                    raise AssertionError(f"example {index} of docs/python.md failed:\n{source}") from error
    finally:
        os.chdir(previous)
