"""The ``reldir`` command, as the wheel installs it: the same command as the
standalone binary, run in this process."""

from __future__ import annotations

import sys

from reldir._reldir import cli_main


def main() -> None:
    sys.exit(cli_main(["reldir", *sys.argv[1:]]))


if __name__ == "__main__":
    main()
